//! Bounded subprocess supervision, with process-group termination before leases drop.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
#[derive(Debug, Serialize, Deserialize)]
pub struct Output {
    pub exit_code: i32,
    pub output: String,
    pub timed_out: bool,
    pub output_truncated: bool,
}
pub fn command(
    argv: &[String],
    environment: Option<&BTreeMap<String, String>>,
    cwd: Option<&Path>,
) -> Result<Command> {
    ensure!(!argv.is_empty(), "Empty native command.");
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    if let Some(e) = environment {
        cmd.env_clear().envs(e);
    }
    if let Some(c) = cwd {
        cmd.current_dir(c);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    Ok(cmd)
}
pub fn stop(child: &mut Child) {
    enginebox_core::transport::kill_process_group(child, Duration::from_millis(250));
}
#[cfg(unix)]
pub fn raw_pipe<T: std::os::fd::AsRawFd>(pipe: &T) -> usize {
    pipe.as_raw_fd() as usize
}
#[cfg(windows)]
pub fn raw_pipe<T: std::os::windows::io::AsRawHandle>(pipe: &T) -> usize {
    pipe.as_raw_handle() as usize
}

/// Cancellable pipe reads. A detached descendant retaining stdout must not
/// prevent controller drain after the supervised process has stopped.
pub fn pipe_reader(
    mut reader: Box<dyn Read + Send>,
    raw: usize,
    tx: mpsc::SyncSender<Vec<u8>>,
    stop: Arc<AtomicBool>,
) -> Result<thread::JoinHandle<()>> {
    #[cfg(unix)]
    unsafe {
        let flags = libc::fcntl(raw as i32, libc::F_GETFL);
        ensure!(
            flags >= 0 && libc::fcntl(raw as i32, libc::F_SETFL, flags | libc::O_NONBLOCK) >= 0,
            "Cannot configure bounded pipe reads."
        );
    }
    Ok(thread::spawn(move || {
        let mut bytes = [0; 4096];
        while !stop.load(Ordering::SeqCst) {
            #[allow(unused_mut)]
            let mut available = bytes.len();
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
                    break;
                }
                if count == 0 {
                    thread::sleep(Duration::from_millis(20));
                    continue;
                }
                available = available.min(count as usize);
            }
            match reader.read(&mut bytes[..available]) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(bytes[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    thread::sleep(Duration::from_millis(10))
                }
                Err(_) => break,
            }
        }
    }))
}
pub fn inherit_lease(cmd: &mut Command, lease: Option<&crate::security::Lease>) {
    #[cfg(unix)]
    if let Some(lease) = lease {
        use std::os::unix::process::CommandExt;
        let fd = lease.fd();
        unsafe {
            cmd.pre_exec(move || {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(not(unix))]
    let _ = (cmd, lease);
}
/// A supervisor-confirmed cancellation, distinct from native exits and timeouts.
pub const CANCELLED_EXIT_CODE: i32 = -125;

pub struct Control<'a> {
    pub lease: Option<&'a crate::security::Lease>,
    pub cancelled: &'a dyn Fn() -> Result<bool>,
}

pub fn bounded(
    argv: &[String],
    environment: Option<&BTreeMap<String, String>>,
    cwd: Option<&Path>,
    input: &[u8],
    timeout: Duration,
    max: usize,
    lease: Option<&crate::security::Lease>,
) -> Result<Output> {
    bounded_controlled(
        argv,
        environment,
        cwd,
        input,
        timeout,
        max,
        Control {
            lease,
            cancelled: &|| Ok(false),
        },
    )
}

pub fn bounded_controlled(
    argv: &[String],
    environment: Option<&BTreeMap<String, String>>,
    cwd: Option<&Path>,
    input: &[u8],
    timeout: Duration,
    max: usize,
    control: Control<'_>,
) -> Result<Output> {
    if (control.cancelled)()? {
        return Ok(Output {
            exit_code: CANCELLED_EXIT_CODE,
            output: String::new(),
            timed_out: false,
            output_truncated: false,
        });
    }
    let mut file = tempfile::tempfile()?;
    file.write_all(input)?;
    use std::io::{Seek, SeekFrom};
    file.seek(SeekFrom::Start(0))?;
    let mut cmd = command(argv, environment, cwd)?;
    cmd.stdin(file)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    inherit_lease(&mut cmd, control.lease);
    let mut child = cmd.spawn()?;
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(8);
    let cancel_read = Arc::new(AtomicBool::new(false));
    let mut joins = Vec::new();
    let stdout = child.stdout.take().unwrap();
    let stdout_raw = raw_pipe(&stdout);
    let stderr = child.stderr.take().unwrap();
    let stderr_raw = raw_pipe(&stderr);
    let readers = (|| -> Result<()> {
        joins.push(pipe_reader(
            Box::new(stdout),
            stdout_raw,
            tx.clone(),
            cancel_read.clone(),
        )?);
        joins.push(pipe_reader(
            Box::new(stderr),
            stderr_raw,
            tx.clone(),
            cancel_read.clone(),
        )?);
        Ok(())
    })();
    drop(tx);
    let deadline = Instant::now() + timeout;
    let mut output = Vec::new();
    let mut timed_out = false;
    let mut cancelled = false;
    let mut truncated = false;
    let result = (|| -> Result<()> {
        readers?;
        loop {
            if (control.cancelled)()? {
                cancelled = true;
                break;
            }
            if Instant::now() >= deadline {
                timed_out = true;
                break;
            }
            match rx.recv_timeout(Duration::from_millis(25)) {
                Ok(b) => {
                    let keep = b.len().min(max.saturating_sub(output.len()));
                    output.extend_from_slice(&b[..keep]);
                    if keep < b.len() {
                        truncated = true;
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    if child.try_wait()?.is_some() {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        Ok(())
    })();
    stop(&mut child);
    cancel_read.store(true, Ordering::SeqCst);
    drop(rx);
    for j in joins {
        let _ = j.join();
    }
    result?;
    let status = child.wait()?;
    #[cfg(unix)]
    let code = {
        use std::os::unix::process::ExitStatusExt;
        status.code().unwrap_or(-status.signal().unwrap_or(1))
    };
    #[cfg(not(unix))]
    let code = status.code().unwrap_or(-1);
    Ok(Output {
        exit_code: if cancelled { CANCELLED_EXIT_CODE } else { code },
        output: String::from_utf8_lossy(&output).into_owned(),
        timed_out,
        output_truncated: truncated,
    })
}
pub fn checked(
    argv: &[String],
    env: Option<&BTreeMap<String, String>>,
    cwd: Option<&Path>,
    input: &[u8],
    timeout: u64,
    max: usize,
) -> Result<String> {
    checked_with_lease(argv, env, cwd, input, timeout, max, None)
}
pub fn checked_with_lease(
    argv: &[String],
    env: Option<&BTreeMap<String, String>>,
    cwd: Option<&Path>,
    input: &[u8],
    timeout: u64,
    max: usize,
    lease: Option<&crate::security::Lease>,
) -> Result<String> {
    let o = bounded(
        argv,
        env,
        cwd,
        input,
        Duration::from_secs(timeout),
        max,
        lease,
    )?;
    ensure!(
        o.exit_code == 0 && !o.timed_out && !o.output_truncated,
        "Native operation failed or exceeded its bounds."
    );
    Ok(o.output)
}
/// Two-step native JSON-lines request. Only initialization and the supplied read method are sent.
pub fn native_rpc(
    argv: &[String],
    env: &BTreeMap<String, String>,
    method: &str,
    params: Option<serde_json::Value>,
    timeout: u64,
    max: usize,
) -> Result<serde_json::Value> {
    native_rpc_with_lease(argv, env, method, params, timeout, max, None)
}
pub fn native_rpc_with_lease(
    argv: &[String],
    env: &BTreeMap<String, String>,
    method: &str,
    params: Option<serde_json::Value>,
    timeout: u64,
    max: usize,
    lease: Option<&crate::security::Lease>,
) -> Result<serde_json::Value> {
    let mut cmd = command(argv, Some(env), Some(Path::new(&env["HOME"])))?;
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    inherit_lease(&mut cmd, lease);
    let mut child = cmd.spawn()?;
    let mut input = child.stdin.take().unwrap();
    let reader = child.stdout.take().unwrap();
    let raw = raw_pipe(&reader);
    let (tx, rx) = mpsc::sync_channel(8);
    let cancel_read = Arc::new(AtomicBool::new(false));
    let handle = match pipe_reader(Box::new(reader), raw, tx, cancel_read.clone()) {
        Ok(handle) => handle,
        Err(error) => {
            stop(&mut child);
            return Err(error);
        }
    };
    let result = (|| -> Result<serde_json::Value> {
        use serde_json::json;
        writeln!(
            input,
            "{}",
            json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"enginebox","version":env!("CARGO_PKG_VERSION")}}})
        )?;
        input.flush()?;
        let deadline = Instant::now() + Duration::from_secs(timeout);
        let mut buffer = Vec::new();
        let mut total = 0;
        let mut initialized = false;
        loop {
            ensure!(Instant::now() < deadline, "Native request timed out.");
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(chunk) => {
                    total += chunk.len();
                    ensure!(total <= max, "Native response exceeds bound.");
                    buffer.extend(chunk);
                    while let Some(end) = buffer.iter().position(|b| *b == b'\n') {
                        let line: Vec<u8> = buffer.drain(..=end).collect();
                        let value: serde_json::Value = serde_json::from_slice(&line)?;
                        if value["id"] == 1 && !initialized {
                            ensure!(
                                value["error"].is_null() && value["result"].is_object(),
                                "Native initialization failed."
                            );
                            initialized = true;
                            writeln!(input, "{}", json!({"method":"initialized"}))?;
                            let mut request = json!({"id":2,"method":method});
                            if let Some(p) = &params {
                                request["params"] = p.clone();
                            }
                            writeln!(input, "{request}")?;
                            input.flush()?;
                        } else if value["id"] == 2 && initialized {
                            ensure!(value["error"].is_null(), "Native method unavailable.");
                            return Ok(value["result"].clone());
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => anyhow::bail!("Native connection closed."),
            }
        }
    })();
    drop(input);
    stop(&mut child);
    cancel_read.store(true, Ordering::SeqCst);
    drop(rx);
    let _ = handle.join();
    result
}
