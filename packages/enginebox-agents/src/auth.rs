use crate::{delegation, native, process, security};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Write},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
pub fn provider_status(p: &str, installed: bool) -> Value {
    json!({"id":p,"label":match p{"codex"=>"Codex","claude"=>"Claude Code",_=>"Cursor Agent"},"installed":installed,"status":if installed{"unknown"}else{"unavailable"},"auth_method":if installed{"unknown"}else{"none"}})
}
pub fn parse_status(p: &str, code: i32, output: &str) -> Value {
    let mut v = provider_status(p, true);
    let (state, method) = if p == "codex" {
        let s = output.to_ascii_lowercase();
        if code == 0 && s.contains("logged in using chatgpt") {
            ("connected", "subscription")
        } else if code == 0
            && (s.contains("logged in using an api key") || s.contains("logged in using a api key"))
        {
            ("connected", "api_key")
        } else if s.contains("not logged in") {
            ("disconnected", "none")
        } else {
            ("unknown", "unknown")
        }
    } else {
        let j: Value = serde_json::from_str(output).unwrap_or(Value::Null);
        if p == "claude" {
            if j["loggedIn"] == false {
                ("disconnected", "none")
            } else if j["loggedIn"] == true && code == 0 {
                (
                    "connected",
                    match j["authMethod"].as_str() {
                        Some("claude.ai") => "subscription",
                        Some("api_key" | "apiKey") => "api_key",
                        _ => "unknown",
                    },
                )
            } else {
                ("unknown", "unknown")
            }
        } else if j["isAuthenticated"] == false {
            ("disconnected", "none")
        } else if j["isAuthenticated"] == true && code == 0 {
            ("connected", "account")
        } else {
            ("unknown", "unknown")
        }
    };
    v["status"] = json!(state);
    v["auth_method"] = json!(method);
    v
}
pub fn controller_environment(
    home: &Path,
    binaries: &BTreeMap<String, Option<String>>,
) -> Result<BTreeMap<String, String>> {
    let mut e = native::environment(home)?;
    let mut paths = binaries
        .values()
        .flatten()
        .filter_map(|s| Path::new(s).parent())
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    paths.extend(["/usr/local/bin".into(), "/usr/bin".into(), "/bin".into()]);
    paths.dedup();
    e.insert(
        "PATH".into(),
        std::env::join_paths(paths)?.to_string_lossy().into_owned(),
    );
    for (k, v) in [
        ("XDG_CONFIG_HOME", home.join(".config")),
        ("XDG_CACHE_HOME", home.join(".cache")),
        ("XDG_DATA_HOME", home.join(".local/share")),
        ("CODEX_HOME", home.join(".codex")),
        ("CLAUDE_CONFIG_DIR", home.join(".claude")),
    ] {
        e.insert(k.into(), v.to_string_lossy().into_owned());
    }
    e.insert("TERM".into(), "xterm-256color".into());
    e.insert("USER".into(), "enginebox".into());
    e.insert("LOGNAME".into(), "enginebox".into());
    e.remove("CURSOR_API_KEY");
    Ok(e)
}
pub fn probe(p: &str, exe: Option<&str>, env: &BTreeMap<String, String>) -> Value {
    probe_with_lease(p, exe, env, None)
}
pub fn probe_with_lease(
    p: &str,
    exe: Option<&str>,
    env: &BTreeMap<String, String>,
    lease: Option<&security::Lease>,
) -> Value {
    let Some(exe) = exe else {
        return provider_status(p, false);
    };
    if native::verify_client(exe, p, env, Path::new(&env["HOME"])).is_err() {
        return provider_status(p, true);
    }
    let args = match p {
        "codex" => vec![
            exe,
            "-c",
            "cli_auth_credentials_store=\"file\"",
            "login",
            "status",
        ],
        "claude" => vec![exe, "auth", "status", "--json"],
        _ => vec![exe, "status", "--format", "json"],
    }
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    match process::bounded(
        &args,
        Some(env),
        Some(Path::new(&env["HOME"])),
        b"",
        Duration::from_secs(10),
        65536,
        lease,
    ) {
        Ok(o) if !o.timed_out && !o.output_truncated => parse_status(p, o.exit_code, &o.output),
        _ => provider_status(p, true),
    }
}
pub fn authorization_url(p: &str, s: &str) -> Option<String> {
    if s.len() > 8192 || s.chars().any(|c| c as u32 <= 32) {
        return None;
    }
    let u = url::Url::parse(s).ok()?;
    let (host, path) = match p {
        "codex" => ("auth.openai.com", "/codex/device"),
        "claude" => ("claude.com", "/cai/oauth/authorize"),
        "cursor" => ("cursor.com", "/loginDeepControl"),
        _ => return None,
    };
    if u.scheme() != "https"
        || u.host_str() != Some(host)
        || u.path() != path
        || !u.username().is_empty()
        || u.password().is_some()
        || u.port().is_some_and(|p| p != 443)
        || u.fragment().is_some()
    {
        return None;
    }
    let allowed: &[&str] = match p {
        "codex" => &[],
        "claude" => &[
            "client_id",
            "code",
            "code_challenge",
            "code_challenge_method",
            "redirect_uri",
            "response_type",
            "scope",
            "state",
        ],
        _ => &[
            "challenge",
            "mode",
            "redirectTarget",
            "supportsSelectedTeamLogin",
            "uuid",
        ],
    };
    let pairs = u.query_pairs().collect::<Vec<_>>();
    let mut seen = BTreeSet::new();
    for (k, _) in &pairs {
        if !allowed.contains(&k.as_ref()) || !seen.insert(k) {
            return None;
        }
    }
    let query: BTreeMap<_, _> = pairs.into_iter().collect();
    let checks: &[(&str, &str)] = if p == "claude" {
        &[
            ("code", "true"),
            ("response_type", "code"),
            ("code_challenge_method", "S256"),
            (
                "redirect_uri",
                "https://platform.claude.com/oauth/code/callback",
            ),
        ]
    } else if p == "cursor" {
        &[
            ("redirectTarget", "cli"),
            ("mode", "login"),
            ("supportsSelectedTeamLogin", "true"),
        ]
    } else {
        &[]
    };
    if checks
        .iter()
        .any(|(k, v)| query.get(*k).is_some_and(|x| x != v))
    {
        return None;
    }
    Some(s.into())
}
pub fn valid_input(s: &str) -> bool {
    (8..=2048).contains(&s.len())
        && regex::Regex::new(r"^[A-Za-z0-9_.~+/=#-]+$")
            .unwrap()
            .is_match(s)
}
pub struct LoginOutput {
    provider: String,
    pub buffer: String,
    state: u8,
    url: Option<String>,
    code: Option<String>,
    pub input_required: bool,
}
impl LoginOutput {
    pub fn new(p: &str) -> Self {
        Self {
            provider: p.into(),
            buffer: String::new(),
            state: 0,
            url: None,
            code: None,
            input_required: false,
        }
    }
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Value> {
        for c in String::from_utf8_lossy(bytes).chars() {
            match self.state {
                1 => {
                    self.state = match c {
                        '[' => 2,
                        ']' => 3,
                        _ => 0,
                    }
                }
                2 => {
                    if ('@'..='~').contains(&c) {
                        self.state = 0
                    }
                }
                3 => match c {
                    '\x07' | '\u{9c}' => self.state = 0,
                    '\x1b' => self.state = 4,
                    _ => {}
                },
                4 => match c {
                    '\\' | '\x07' | '\u{9c}' => self.state = 0,
                    '\x1b' => {}
                    _ => self.state = 3,
                },
                _ => match c {
                    '\x1b' => self.state = 1,
                    '\u{9b}' => self.state = 2,
                    '\u{9d}' => self.state = 3,
                    _ => {
                        if !c.is_control() || c == '\n' || c == '\t' {
                            self.buffer.push(c)
                        }
                    }
                },
            }
        }
        while self.buffer.len() > 16384 {
            self.buffer.remove(0);
        }
        let mut events = Vec::new();
        let re = regex::Regex::new(r#"https://[^\s<>"']+[\s<>"']"#).unwrap();
        for matched in re.find_iter(&self.buffer) {
            let s = matched
                .as_str()
                .trim_end_matches(|c: char| c.is_whitespace() || "<>\"'".contains(c));
            if let Some(url) = authorization_url(&self.provider, s)
                && self.url.as_ref() != Some(&url)
            {
                self.url = Some(url.clone());
                events.push(json!({"type":"authorization","authorization_url":url}));
            }
        }
        if self.provider == "codex"
            && self.url.is_some()
            && let Some(code) = regex::Regex::new(r"\b[A-Z0-9]{4,8}-[A-Z0-9]{4,8}\b")
                .unwrap()
                .find(&self.buffer)
                .map(|m| m.as_str().to_owned())
            && self.code.as_ref() != Some(&code)
        {
            self.code = Some(code.clone());
            events.push(json!({"type":"code","user_code":code}));
        }
        if self.provider=="claude"&&self.url.is_some()&&!self.input_required&&regex::Regex::new(r"(?i)paste (the )?(authentication )?code|enter (the )?authorization code|paste\s*code\s*here\s*if\s*prompted\s*>").unwrap().is_match(&self.buffer){self.input_required=true;events.push(json!({"type":"input_required"}));}
        events
    }
}
#[derive(Debug)]
pub enum Control {
    Input(String),
    Cancel,
}
/// Native login owns its private lock for the entire PTY and verification lifetime.
pub fn login<F: FnMut(Value)>(
    home: &Path,
    p: &str,
    exe: &str,
    binaries: &BTreeMap<String, Option<String>>,
    control: mpsc::Receiver<Control>,
    cancelled: &AtomicBool,
    mut emit: F,
) -> Result<()> {
    let lease = security::Lease::acquire(home, &format!(".local/state/enginebox/auth-{p}.lock"))?;
    let env = controller_environment(home, binaries)?;
    native::verify_client(exe, p, &env, home)?;
    let argv = match p {
        "codex" => vec![
            exe,
            "-c",
            "cli_auth_credentials_store=\"file\"",
            "login",
            "--device-auth",
        ],
        "claude" => vec![exe, "setup-token"],
        _ => vec![exe, "login"],
    }
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    let mut cmd = process::command(&argv, Some(&env), Some(home))?;
    process::inherit_lease(&mut cmd, Some(&lease));
    #[cfg(unix)]
    let (mut input, output_raw, output) = {
        use std::os::fd::{AsRawFd, FromRawFd};
        let mut master = 0;
        let mut slave = 0;
        ensure!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            } == 0,
            "Could not allocate native login terminal."
        );
        let master = unsafe { std::fs::File::from_raw_fd(master) };
        let slave = unsafe { std::fs::File::from_raw_fd(slave) };
        let mut attrs: libc::termios = unsafe { std::mem::zeroed() };
        unsafe {
            libc::tcgetattr(slave.as_raw_fd(), &mut attrs);
            attrs.c_lflag &= !libc::ECHO;
            libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &attrs);
            let size = libc::winsize {
                ws_row: 24,
                ws_col: 4096,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            libc::ioctl(slave.as_raw_fd(), libc::TIOCSWINSZ, &size);
        }
        cmd.stdin(slave.try_clone()?)
            .stdout(slave.try_clone()?)
            .stderr(slave);
        (
            Box::new(master.try_clone()?) as Box<dyn Write + Send>,
            process::raw_pipe(&master),
            Box::new(master) as Box<dyn Read + Send>,
        )
    };
    #[cfg(not(unix))]
    {
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
    }
    let mut child = cmd.spawn()?;
    #[cfg(not(unix))]
    let (mut input, output_raw, output) = {
        let stdout = child.stdout.take().unwrap();
        let raw = process::raw_pipe(&stdout);
        (
            Box::new(child.stdin.take().unwrap()) as Box<dyn Write + Send>,
            raw,
            Box::new(stdout) as Box<dyn Read + Send>,
        )
    };
    drop(cmd);
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(8);
    let cancel_read = std::sync::Arc::new(AtomicBool::new(false));
    let reader = match process::pipe_reader(output, output_raw, tx, cancel_read.clone()) {
        Ok(reader) => reader,
        Err(error) => {
            process::stop(&mut child);
            return Err(error);
        }
    };
    let deadline = Instant::now() + Duration::from_secs(600);
    let mut parser = LoginOutput::new(p);
    let mut setup_token = None;
    let mut reason = "failed";
    let token_pattern = regex::Regex::new(r"sk-ant-oat01-[A-Za-z0-9_-]{20,4096}")?;
    emit(json!({"type":"started"}));
    let result = (|| -> Result<()> {
        loop {
            if cancelled.load(Ordering::SeqCst) {
                reason = "cancelled";
                break;
            }
            if Instant::now() >= deadline {
                reason = "expired";
                break;
            }
            while let Ok(c) = control.try_recv() {
                match c {
                    Control::Cancel => {
                        reason = "cancelled";
                        return Ok(());
                    }
                    Control::Input(text) => {
                        ensure!(
                            p == "claude" && parser.input_required && valid_input(&text),
                            "Invalid authorization code input."
                        );
                        input.write_all(text.as_bytes())?;
                        input.write_all(b"\n")?;
                        input.flush()?;
                        parser.input_required = false;
                        parser.buffer.clear();
                        emit(json!({"type":"input_accepted"}));
                    }
                }
            }
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(b) => {
                    for e in parser.feed(&b) {
                        emit(e)
                    }
                    if p == "claude"
                        && let Some(m) = token_pattern.find(&parser.buffer)
                    {
                        setup_token = Some(m.as_str().to_owned());
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    if let Some(status) = child.try_wait()? {
                        if status.success() {
                            reason = "completed";
                        }
                        break;
                    }
                }
            }
            if let Some(status) = child.try_wait()? {
                process::stop(&mut child);
                let drain_deadline = Instant::now() + Duration::from_millis(500);
                while Instant::now() < drain_deadline {
                    match rx.recv_timeout(Duration::from_millis(25)) {
                        Ok(b) => {
                            for e in parser.feed(&b) {
                                emit(e)
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
                if p == "claude"
                    && let Some(m) = token_pattern.find(&parser.buffer)
                {
                    setup_token = Some(m.as_str().to_owned());
                }
                if status.success() {
                    reason = "completed";
                }
                break;
            }
        }
        Ok(())
    })();
    process::stop(&mut child);
    cancel_read.store(true, Ordering::SeqCst);
    drop(input);
    drop(rx);
    let _ = reader.join();
    if result.is_err() {
        reason = "failed";
    }
    if reason == "completed" && !cancelled.load(Ordering::SeqCst) {
        let mut verify_env = env;
        if let Some(token) = &setup_token {
            verify_env.insert("CLAUDE_CODE_OAUTH_TOKEN".into(), token.clone());
        }
        let mut status = probe_with_lease(p, Some(exe), &verify_env, Some(&lease));
        if setup_token.is_some() && status["status"] == "connected" {
            status["auth_method"] = json!("subscription");
        }
        if cancelled.load(Ordering::SeqCst) || matches!(control.try_recv(), Ok(Control::Cancel)) {
            reason = "cancelled";
        } else if status["status"] == "connected"
            && ["subscription", "account"].contains(&status["auth_method"].as_str().unwrap_or(""))
        {
            if let Some(token) = setup_token {
                security::write_json(
                    home,
                    delegation::TOKEN_FILE,
                    &json!({"kind":"setup_token","value":token,"expires_at":chrono::Utc::now().timestamp()+364*86400}),
                )?;
                status["auth_method"] = json!("subscription");
            }
            emit(json!({"type":"result","reason":"completed","account":status}));
            return Ok(());
        } else {
            reason = "unverified";
        }
    }
    emit(json!({"type":"result","reason":reason}));
    Ok(())
}
