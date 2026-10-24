use crate::model::{GuestConfig, Host, identifier};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use wait_timeout::ChildExt;

pub fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&c))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}
pub fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|s| shell_quote(s))
        .collect::<Vec<_>>()
        .join(" ")
}
pub fn run_process(argv: &[String], input: Option<&[u8]>, timeout: Duration) -> Result<Vec<u8>> {
    run_process_environment(argv, input, timeout, None)
}
/// Execute with only the supplied environment. Sensitive values stay out of argv
/// and are never included in diagnostics by this runner.
pub fn run_isolated_process(
    argv: &[String],
    input: Option<&[u8]>,
    timeout: Duration,
    environment: &[(&str, &str)],
) -> Result<Vec<u8>> {
    run_process_environment(argv, input, timeout, Some(environment))
}
fn run_process_environment(
    argv: &[String],
    input: Option<&[u8]>,
    timeout: Duration,
    environment: Option<&[(&str, &str)]>,
) -> Result<Vec<u8>> {
    ensure!(
        !argv.is_empty() && argv.iter().all(|v| !v.contains('\0')),
        "Invalid command argv"
    );
    let mut cmd = Command::new(&argv[0]);
    if let Some(environment) = environment {
        cmd.env_clear().envs(environment.iter().copied());
    }
    #[cfg(windows)]
    if std::path::Path::new(&argv[0])
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|name| name.eq_ignore_ascii_case("powershell.exe"))
    {
        // A PowerShell 7 parent can export module paths that Windows
        // PowerShell 5 cannot load. Reconstruct this child's default paths.
        cmd.env_remove("PSModulePath");
    }
    cmd.args(&argv[1..]);
    run_command(cmd, input, timeout)
}

/// Run an explicitly configured command with the same bounded I/O and process-tree
/// cleanup as the SSH transport. Callers may set a private cwd/environment.
pub fn run_command(cmd: Command, input: Option<&[u8]>, timeout: Duration) -> Result<Vec<u8>> {
    run_command_bounded(cmd, input, timeout, 16 * 1024 * 1024, 64 * 1024)
}

/// Build tools can emit substantial stderr even on success. Callers still choose
/// bounded streams; ordinary transport commands retain the stricter defaults.
pub fn run_command_bounded(
    mut cmd: Command,
    input: Option<&[u8]>,
    timeout: Duration,
    max_stdout: usize,
    max_stderr: usize,
) -> Result<Vec<u8>> {
    ensure!(
        (1..=64 * 1024 * 1024).contains(&max_stdout)
            && (1..=64 * 1024 * 1024).contains(&max_stderr),
        "Invalid process output bounds"
    );
    let deadline = Instant::now() + timeout;
    // A child can stop reading stdin while its descendant retains that handle.
    // An anonymous private file removes the otherwise unbounded writer thread.
    if let Some(data) = input.filter(|data| !data.is_empty()) {
        let mut file = tempfile::tempfile()?;
        file.write_all(data)?;
        file.seek(SeekFrom::Start(0))?;
        cmd.stdin(file);
    } else {
        cmd.stdin(Stdio::null());
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().context("Cannot start command")?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let stdout_raw = pipe_handle(&stdout);
    let stderr_raw = pipe_handle(&stderr);
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_out = cancel.clone();
    let cancel_err = cancel.clone();
    let out =
        thread::spawn(move || read_bounded(stdout, stdout_raw, max_stdout, deadline, &cancel_out));
    let err =
        thread::spawn(move || read_bounded(stderr, stderr_raw, max_stderr, deadline, &cancel_err));
    // Keep the parent unreaped until its streams finish. If a descendant keeps
    // a pipe open, the original process-group ID cannot be recycled before kill.
    let stdout = out
        .join()
        .unwrap_or_else(|_| Err(anyhow::anyhow!("Output reader failed")));
    if stdout.is_err() {
        cancel.store(true, Ordering::SeqCst);
        kill_tree(&mut child);
    }
    let stderr = err
        .join()
        .unwrap_or_else(|_| Err(anyhow::anyhow!("Error reader failed")));
    if stdout.is_err() || stderr.is_err() {
        kill_tree(&mut child);
        let _ = child.wait_timeout(Duration::from_secs(1));
        stdout?;
        stderr?;
        unreachable!("A stream failed");
    }
    let status = match child.wait_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Some(status)) => status,
        other => {
            kill_tree(&mut child);
            let _ = child.wait_timeout(Duration::from_secs(1));
            other?;
            bail!("Command exceeded its deadline; operation outcome is uncertain");
        }
    };
    let stdout = stdout?;
    let stderr = stderr?;
    ensure!(
        status.success(),
        "Command failed: {}",
        String::from_utf8_lossy(&stderr)
            .trim()
            .chars()
            .take(2000)
            .collect::<String>()
    );
    Ok(stdout)
}
#[cfg(unix)]
fn pipe_handle<T: std::os::fd::AsRawFd>(pipe: &T) -> usize {
    pipe.as_raw_fd() as usize
}
#[cfg(windows)]
fn pipe_handle<T: std::os::windows::io::AsRawHandle>(pipe: &T) -> usize {
    pipe.as_raw_handle() as usize
}
fn read_bounded(
    mut stream: impl Read,
    raw: usize,
    max: usize,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    #[cfg(unix)]
    unsafe {
        let flags = libc::fcntl(raw as i32, libc::F_GETFL);
        ensure!(
            flags >= 0 && libc::fcntl(raw as i32, libc::F_SETFL, flags | libc::O_NONBLOCK) >= 0,
            "Cannot configure bounded command output"
        );
    }
    let mut result = Vec::new();
    let mut buf = [0; 8192];
    let mut exceeded = false;
    loop {
        ensure!(
            !cancel.load(Ordering::SeqCst) && Instant::now() < deadline,
            "Command output did not close before its deadline; operation outcome is uncertain"
        );
        #[allow(unused_mut)]
        let mut available = buf.len();
        #[cfg(windows)]
        unsafe {
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn PeekNamedPipe(
                    handle: *mut std::ffi::c_void,
                    buffer: *mut std::ffi::c_void,
                    size: u32,
                    read: *mut u32,
                    available: *mut u32,
                    left: *mut u32,
                ) -> i32;
            }
            let mut count = 0;
            if PeekNamedPipe(
                raw as *mut _,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut count,
                std::ptr::null_mut(),
            ) == 0
            {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(109) {
                    break;
                }
                return Err(error.into());
            }
            if count == 0 {
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            available = available.min(count as usize);
        }
        let n = match stream.read(&mut buf[..available]) {
            Ok(n) => n,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        if n == 0 {
            break;
        }
        let keep = n.min(max.saturating_sub(result.len()));
        result.extend_from_slice(&buf[..keep]);
        exceeded |= keep < n;
    }
    ensure!(!exceeded, "Command output exceeded its bound");
    Ok(result)
}
/// Terminates a child that leads its own process group (`setsid`).
///
/// A zero `grace` kills immediately (SIGKILL) and returns without waiting. A
/// non-zero `grace` sends SIGTERM first, polls for exit up to `grace`, then
/// kills and reaps the child.
pub fn kill_process_group(child: &mut Child, grace: Duration) {
    #[cfg(unix)]
    let pid = child.id() as i32;
    #[cfg(unix)]
    let signal_group = |signal: i32| unsafe {
        libc::kill(-pid, signal);
    };
    if grace.is_zero() {
        #[cfg(unix)]
        signal_group(libc::SIGKILL);
        let _ = child.kill();
        return;
    }
    #[cfg(unix)]
    signal_group(libc::SIGTERM);
    #[cfg(not(unix))]
    let _ = child.kill();
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    #[cfg(unix)]
    signal_group(libc::SIGKILL);
    let _ = child.kill();
    let _ = child.wait();
}
fn kill_tree(child: &mut Child) {
    kill_process_group(child, Duration::ZERO);
}
pub fn host_command(host: &Host, argv: &[String]) -> Vec<String> {
    let mut result = vec!["sudo".into(), "-n".into()];
    result.extend_from_slice(argv);
    if host.target == "local" {
        result
    } else {
        vec![
            "ssh".into(),
            "-T".into(),
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "ConnectTimeout=10".into(),
            host.target.clone(),
            shell_join(&result),
        ]
    }
}
pub fn host_run(
    host: &Host,
    argv: &[String],
    input: Option<&[u8]>,
    timeout: Duration,
) -> Result<Vec<u8>> {
    run_process(&host_command(host, argv), input, timeout)
}
/// A VM snapshot cannot capture an externally mounted checkout's file state.
/// A partial attachment marker is also a reason to stop rather than guess.
pub fn ensure_no_external_checkout(instance: &Value, operation: &str) -> Result<()> {
    for field in ["config", "expanded_config"] {
        let config = &instance[field];
        ensure!(
            config.get("user.enginebox.layerfs").is_none()
                && config.get("user.enginebox.layerfs.workspace").is_none(),
            "Detach the LayerFS checkout before {operation}; external files are not part of VM disk or RAM snapshots"
        );
    }
    Ok(())
}
#[derive(Debug, Clone)]
pub struct EngineBox {
    pub host: String,
    pub project: String,
    pub guest: GuestConfig,
}
impl EngineBox {
    pub fn new(host: &str, project: &str, guest: GuestConfig) -> Result<Self> {
        let h = Host {
            id: "operator".into(),
            target: host.into(),
            project: project.into(),
            guest,
            ..Default::default()
        };
        h.validate()?;
        Ok(h.engine_box())
    }
    pub fn from_env() -> Result<Self> {
        ensure!(
            std::env::var("ENGINEBOX_GUEST").unwrap_or_default() != "1",
            "Host control unavailable in guest mode"
        );
        Self::new(
            &std::env::var("ENGINEBOX_HOST").context("Set ENGINEBOX_HOST explicitly")?,
            &std::env::var("ENGINEBOX_PROJECT").unwrap_or("enginebox".into()),
            GuestConfig::from_env()?,
        )
    }
    pub fn command(&self, args: &[String]) -> Vec<String> {
        let mut command = vec!["sudo".into(), "-n".into(), "incus".into()];
        if args.first().map(String::as_str) != Some("query") {
            command.extend(["--project".into(), self.project.clone()]);
        }
        command.extend_from_slice(args);
        if self.host == "local" {
            command
        } else {
            vec![
                "ssh".into(),
                "-T".into(),
                "-o".into(),
                "BatchMode=yes".into(),
                "-o".into(),
                "ConnectTimeout=10".into(),
                self.host.clone(),
                shell_join(&command),
            ]
        }
    }
    pub fn run(&self, args: &[String], timeout: Duration) -> Result<Vec<u8>> {
        run_process(&self.command(args), None, timeout)
    }
    pub fn run_input(&self, args: &[String], input: &[u8], timeout: Duration) -> Result<Vec<u8>> {
        run_process(&self.command(args), Some(input), timeout)
    }
    pub fn call(&self, args: &[&str]) -> Result<Vec<u8>> {
        self.run(
            &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            Duration::from_secs(300),
        )
    }
    pub fn query(&self, path: &str) -> Result<Value> {
        Ok(serde_json::from_slice(&self.call(&["query", path])?)?)
    }
    pub fn instance(&self, name: &str) -> Result<Value> {
        identifier(name, true)?;
        self.query(&format!("/1.0/instances/{name}?project={}", self.project))
    }
    pub fn status(&self, name: &str) -> Result<Value> {
        let v = self.instance(name)?;
        Ok(
            json!({"name":v["name"],"status":v["status"],"stateful":v["stateful"].as_bool().unwrap_or(false)}),
        )
    }
    pub fn list(&self) -> Result<Value> {
        let data: Value = serde_json::from_slice(&self.call(&["list", "--format=json"])?)?;
        Ok(Value::Array(
            data.as_array()
                .context("Invalid Incus inventory")?
                .iter()
                .filter(|v| v["name"].as_str().unwrap_or("").starts_with("ebox-"))
                .map(|v| {
                    let ipv4 = v["state"]["network"].as_object().into_iter().flat_map(|m|m.values())
                        .flat_map(|n|n["addresses"].as_array().into_iter().flatten())
                        .filter(|a|a["family"]=="inet"&&a["scope"]=="global")
                        .filter_map(|a|a["address"].as_str()).collect::<Vec<_>>();
                    json!({"name":v["name"],"status":v["status"],"stateful":v["stateful"].as_bool().unwrap_or(false),"ipv4":ipv4})
                })
                .collect(),
        ))
    }
    pub fn desktop(&self, name: &str) -> Result<Value> {
        identifier(name, true)?;
        let list = self.list()?;
        let address = list
            .as_array()
            .context("Invalid inventory")?
            .iter()
            .find(|v| v["name"] == name)
            .and_then(|v| v["ipv4"].as_array())
            .and_then(|v| v.first())
            .and_then(Value::as_str)
            .context("Workspace has no IPv4 address")?
            .parse::<std::net::Ipv4Addr>()?;
        if self.host == "local" {
            return Ok(
                json!({"tunnel":null,"tunnel_argv":null,"url":format!("http://{address}:6080/vnc.html?autoconnect=true")}),
            );
        }
        let argv = vec![
            "ssh".into(),
            "-N".into(),
            "-o".into(),
            "ExitOnForwardFailure=yes".into(),
            "-L".into(),
            format!("127.0.0.1:16080:{address}:6080"),
            self.host.clone(),
        ];
        Ok(
            json!({"tunnel":shell_join(&argv),"tunnel_argv":argv,"url":"http://127.0.0.1:16080/vnc.html?autoconnect=true"}),
        )
    }
    pub fn exec_args(&self, name: &str, argv: &[String]) -> Result<Vec<String>> {
        identifier(name, true)?;
        ensure!(
            !argv.is_empty() && argv.iter().all(|s| !s.contains('\0')),
            "Provide a nonempty command argv without NUL bytes"
        );
        Ok(vec![
            "exec".into(),
            name.into(),
            format!("--user={}", self.guest.uid),
            format!("--group={}", self.guest.gid),
            format!("--cwd={}", self.guest.workspace),
            format!("--env=HOME={}", self.guest.home),
            format!("--env=USER={}", self.guest.user),
            format!("--env=DISPLAY={}", self.guest.display),
            "--".into(),
            "bash".into(),
            "-lc".into(),
            format!("exec -- {}", shell_join(argv)),
        ])
    }
    pub fn execute(&self, name: &str, argv: &[String], timeout: Duration) -> Result<Vec<u8>> {
        ensure!(
            (1..=300).contains(&timeout.as_secs()),
            "Guest command timeout must be 1–300 seconds"
        );
        self.run(&self.exec_args(name, argv)?, timeout)
    }
    pub fn execute_with_input(
        &self,
        name: &str,
        argv: &[String],
        input: &[u8],
        timeout: Duration,
    ) -> Result<Vec<u8>> {
        ensure!(
            (1..=300).contains(&timeout.as_secs()),
            "Guest command timeout must be 1–300 seconds"
        );
        self.run_input(&self.exec_args(name, argv)?, input, timeout)
    }
    pub fn lifecycle(&self, name: &str, action: &str) -> Result<Value> {
        let v = self.instance(name)?;
        if action == "hibernate" {
            ensure_no_external_checkout(&v, "hibernation")?;
        }
        let state = v["status"].as_str().unwrap_or("");
        let args = match (action, state) {
            ("pause", "Running") => vec!["pause", name],
            ("resume", "Frozen" | "Stopped") => vec!["start", name],
            ("hibernate", "Running") => vec!["stop", name, "--stateful"],
            ("stop", "Running") => vec!["stop", name, "--timeout=60"],
            _ => bail!("Cannot {action} instance in {state}; resume a frozen VM before stopping"),
        };
        self.call(&args)?;
        self.status(name)
    }
    pub fn snapshot(&self, name: &str, snapshot: &str) -> Result<Value> {
        identifier(snapshot, false)?;
        let v = self.instance(name)?;
        ensure_no_external_checkout(&v, "snapshot")?;
        let state = v["status"].as_str().unwrap_or("");
        let mut args = vec!["snapshot", "create", name, snapshot];
        match state {
            "Running" => args.push("--stateful"),
            "Stopped" if v["stateful"] != true => {}
            _ => bail!("Snapshot requires a running VM or a cleanly stopped VM"),
        };
        self.call(&args)?;
        Ok(
            json!({"snapshot":format!("{name}/{snapshot}"),"private":true,"stateful":state=="Running"}),
        )
    }
    pub fn fork(&self, source: &str, snapshot: &str, target: &str) -> Result<Value> {
        identifier(source, true)?;
        identifier(snapshot, false)?;
        identifier(target, true)?;
        ensure_no_external_checkout(&self.instance(source)?, "fork")?;
        ensure_no_external_checkout(
            &self.query(&format!(
                "/1.0/instances/{source}/snapshots/{snapshot}?project={}",
                self.project
            ))?,
            "fork",
        )?;
        self.call(&[
            "copy",
            &format!("{source}/{snapshot}"),
            target,
            "--stateless",
        ])?;
        self.status(target)
    }
    pub fn restore(&self, name: &str, snapshot: &str, stateful: bool) -> Result<Value> {
        identifier(snapshot, false)?;
        let instance = self.instance(name)?;
        ensure_no_external_checkout(&instance, "restore")?;
        ensure!(
            instance["status"] == "Stopped",
            "Stop explicitly before restoring disk and memory"
        );
        ensure_no_external_checkout(
            &self.query(&format!(
                "/1.0/instances/{name}/snapshots/{snapshot}?project={}",
                self.project
            ))?,
            "restore",
        )?;
        let mut args = vec!["snapshot", "restore", name, snapshot];
        if stateful {
            args.push("--stateful");
        }
        self.call(&args)?;
        self.status(name)
    }
}
pub struct HostOperationLock {
    child: Child,
}
impl Drop for HostOperationLock {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        if self
            .child
            .wait_timeout(Duration::from_secs(5))
            .ok()
            .flatten()
            .is_none()
        {
            kill_tree(&mut self.child);
            let _ = self.child.wait();
        }
    }
}
pub fn operation_lock(host: &Host) -> Result<HostOperationLock> {
    let args = host_command(host, &["enginebox-host".into(), "lock".into()]);
    let mut command = Command::new(&args[0]);
    command
        .args(&args[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let mut line = String::new();
        let r = BufReader::new(stdout).read_line(&mut line).map(|_| line);
        let _ = tx.send(r);
    });
    if rx
        .recv_timeout(Duration::from_secs(30))
        .ok()
        .and_then(Result::ok)
        .as_deref()
        != Some("locked\n")
    {
        kill_tree(&mut child);
        let _ = child.wait();
        bail!("Cannot acquire host operation lock; another controller or installer may be active");
    }
    Ok(HostOperationLock { child })
}
