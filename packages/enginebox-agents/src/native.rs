use crate::{process, security};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, path::Path, time::Duration};
pub const MAX_OUTPUT: usize = 262144;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRequest {
    pub provider: String,
    pub prompt: String,
    #[serde(default)]
    pub resume_id: Option<String>,
    #[serde(default = "default_workdir")]
    pub workdir: String,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub model: Option<String>,
}
fn default_workdir() -> String {
    ".".into()
}
fn default_timeout() -> u64 {
    120
}
impl RunRequest {
    pub fn parse(value: Value) -> Result<Self> {
        let request: Self = serde_json::from_value(value)?;
        request.validate()?;
        Ok(request)
    }
    pub fn validate(&self) -> Result<()> {
        crate::provider(&self.provider)?;
        ensure!(
            !self.prompt.trim().is_empty()
                && self.prompt.len() <= 12000
                && !self.prompt.contains('\0'),
            "Prompt must contain 1–12000 UTF-8 bytes without NUL."
        );
        ensure!(
            (1..=300).contains(&self.timeout_seconds),
            "timeout_seconds must be 1–300."
        );
        ensure!(
            !self.workdir.is_empty()
                && self.workdir.len() <= 512
                && !Path::new(&self.workdir).is_absolute()
                && !self.workdir.split('/').any(|p| p == "..")
                && !self.workdir.chars().any(|c| c.is_control())
                && !self.workdir.contains('\\'),
            "workdir must be relative and inside the workspace."
        );
        if let Some(id) = &self.resume_id {
            ensure!(
                security::exact_uuid(id),
                "Resume requires the exact native session ID."
            );
        }
        if let Some(model) = &self.model {
            ensure!(
                regex::Regex::new(r"^[a-zA-Z0-9][a-zA-Z0-9_.:-]{0,127}$")?.is_match(model),
                "Invalid native model identifier."
            );
        }
        ensure!(
            serde_json::to_vec(self)?.len() <= 16384,
            "Native launch request is too large."
        );
        Ok(())
    }
}
pub fn binary(provider: &str) -> &str {
    match provider {
        "codex" => "codex",
        "claude" => "claude",
        _ => "agent",
    }
}
pub fn native_command(r: &RunRequest, exe: &str) -> Vec<String> {
    let mut args = vec![exe.to_string()];
    match r.provider.as_str() {
        "codex" => {
            args.push("exec".into());
            if let Some(id) = &r.resume_id {
                args.extend(["resume".into(), id.clone()]);
            }
            args.extend(
                [
                    "--json",
                    "--skip-git-repo-check",
                    "-c",
                    "approval_policy=\"never\"",
                    "-c",
                    "sandbox_mode=\"workspace-write\"",
                ]
                .map(str::to_owned),
            );
        }
        "claude" => {
            args.extend(
                [
                    "-p",
                    "--output-format",
                    "stream-json",
                    "--verbose",
                    "--permission-mode",
                    "acceptEdits",
                    "--permission-prompts",
                    "none",
                    "--allowedTools",
                    "Read,Write,Edit,Bash,mcp__enginebox-browser__*",
                    "--max-turns",
                    "20",
                ]
                .map(str::to_owned),
            );
            if let Some(id) = &r.resume_id {
                args.extend(["--resume".into(), id.clone()]);
            }
        }
        _ => {
            args.extend(
                [
                    "-p",
                    "--output-format",
                    "stream-json",
                    "--trust",
                    "--sandbox",
                    "enabled",
                ]
                .map(str::to_owned),
            );
            if let Some(id) = &r.resume_id {
                args.extend(["--resume".into(), id.clone()]);
            }
        }
    }
    if let Some(model) = &r.model {
        args.extend(["--model".into(), model.clone()]);
    }
    if r.provider == "cursor" {
        args.extend(["--".into(), r.prompt.clone()]);
    } else if r.provider == "codex" {
        args.push("-".into());
    }
    args
}
pub fn native_command_with_config(
    r: &RunRequest,
    exe: &str,
    home: &Path,
    cwd: &Path,
) -> Vec<String> {
    let mut args = native_command(r, exe);
    if r.provider == "codex" && codex_browser_needs_default(home, cwd) {
        // A leaf override preserves per-tool policies and disabled_tools. Never
        // create a transport for browser-free homes or override an explicit
        // server policy. Unreadable/malformed configs retain native diagnostics.
        args.splice(
            args.len() - 1..args.len() - 1,
            [
                "-c".into(),
                "mcp_servers.enginebox-browser.default_tools_approval_mode=\"approve\"".into(),
            ],
        );
    }
    args
}
fn codex_browser_needs_default(home: &Path, cwd: &Path) -> bool {
    // Native project configuration has higher precedence than the home. Without
    // reproducing Codex's trust/config resolution, conservatively defer to it.
    for directory in cwd.ancestors().filter(|directory| *directory != home) {
        match std::fs::symlink_metadata(directory.join(".codex/config.toml")) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => return false,
        }
    }
    let Ok(Some(bytes)) = security::read_bytes(home, ".codex/config.toml", 1024 * 1024, true)
    else {
        return false;
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return false;
    };
    let Ok(config) = toml::from_str::<toml::Value>(text) else {
        return false;
    };
    let Some(entry) = config
        .get("mcp_servers")
        .and_then(|servers| servers.get("enginebox-browser"))
        .and_then(toml::Value::as_table)
    else {
        return false;
    };
    entry.get("command").and_then(toml::Value::as_str) == Some("/usr/local/bin/enginebox-guest")
        && entry.get("args")
            == Some(&toml::Value::Array(
                ["browser", "--client", "codex"]
                    .into_iter()
                    .map(|value| toml::Value::String(value.into()))
                    .collect(),
            ))
        && entry
            .get("enabled")
            .is_none_or(|value| value.as_bool() == Some(true))
        && entry
            .get("type")
            .is_none_or(|value| value.as_str() == Some("stdio"))
        && !entry.contains_key("url")
        && !entry.contains_key("default_tools_approval_mode")
}
pub fn environment(home: &Path) -> Result<BTreeMap<String, String>> {
    let home = home
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Invalid home."))?;
    let mut e: BTreeMap<String, String> = [
        ("HOME", home.into()),
        ("USER", std::env::var("USER").unwrap_or("agent".into())),
        ("SHELL", "/bin/bash".into()),
        (
            "PATH",
            format!("{home}/.local/bin:/usr/local/bin:/usr/bin:/bin"),
        ),
        ("LANG", "C.UTF-8".into()),
        ("TERM", "dumb".into()),
        ("NO_OPEN_BROWSER", "1".into()),
        ("BROWSER", "/usr/bin/true".into()),
        ("DISABLE_AUTOUPDATER", "1".into()),
        ("DISPLAY", std::env::var("DISPLAY").unwrap_or(":99".into())),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v))
    .collect();
    e.insert("LOGNAME".into(), e["USER"].clone());
    if let Some(raw) = security::read_bytes(
        Path::new(home),
        ".config/enginebox/agents/cursor.env",
        65536,
        true,
    )? {
        let raw = String::from_utf8(raw)?;
        let token = raw
            .trim()
            .strip_prefix("export CURSOR_API_KEY=")
            .ok_or_else(|| anyhow::anyhow!("Invalid delegated credential."))?;
        let token = unquote(token)?;
        ensure!(
            security::token(&Value::String(token.clone())),
            "Invalid delegated credential."
        );
        e.insert("CURSOR_API_KEY".into(), token);
    }
    Ok(e)
}
fn unquote(s: &str) -> Result<String> {
    if s.starts_with('\'') && s.ends_with('\'') {
        let v = s[1..s.len() - 1].replace("'\"'\"'", "'");
        ensure!(!v.contains('\n'), "Invalid delegated credential.");
        Ok(v)
    } else {
        ensure!(
            !s.chars().any(char::is_whitespace) && !s.contains(['\'', '"']),
            "Invalid delegated credential."
        );
        Ok(s.into())
    }
}
pub fn find_binary(name: &str, env: &BTreeMap<String, String>) -> Option<String> {
    if Path::new(name).is_absolute() {
        return Path::new(name).is_file().then(|| name.into());
    }
    std::env::split_paths(std::ffi::OsStr::new(env.get("PATH")?))
        .map(|p| p.join(name))
        .find(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
}
pub fn verify_client(
    exe: &str,
    provider: &str,
    env: &BTreeMap<String, String>,
    cwd: &Path,
) -> Result<String> {
    let out = process::checked(
        &[exe.into(), "--version".into()],
        Some(env),
        Some(cwd),
        b"",
        10,
        1024,
    )?;
    let version = out.trim();
    let pat = match provider {
        "codex" => r"^codex-cli [0-9]+\.[0-9]+\.[0-9]+[^\r\n]*$",
        "claude" => r"^[0-9]+\.[0-9]+\.[0-9]+ \(Claude Code\)$",
        _ => r"^(Cursor Agent )?20[0-9]{2}\.[0-9]{2}\.[0-9]{2}-[a-zA-Z0-9]+$",
    };
    ensure!(
        regex::Regex::new(pat)?.is_match(version),
        "Installed executable does not identify as the expected native client."
    );
    Ok(version.into())
}
pub fn execute(r: &RunRequest, home: &Path, workspace: &Path) -> Result<process::Output> {
    execute_scoped(r, home, workspace, None)
}

/// The controller supplies a unique run ID separately from the public native request.
pub fn execute_scoped(
    r: &RunRequest,
    home: &Path,
    workspace: &Path,
    run_id: Option<&str>,
) -> Result<process::Output> {
    r.validate()?;
    if let Some(id) = run_id {
        validate_run_id(id)?;
    }
    let home = security::absolute_existing(home)?;
    let workspace = security::absolute_existing(workspace)?;
    let cwd = workspace.join(&r.workdir).canonicalize()?;
    ensure!(
        cwd.is_dir() && cwd.starts_with(&workspace),
        "workdir escapes its assigned workspace."
    );
    let _state = security::safe_path(&home, ".local/state/enginebox/agent-runs/placeholder", true)?;
    security::private_dir(&home.join(".local/state/enginebox/agent-runs"))?;
    let lease = security::Lease::acquire(
        &home,
        &format!(".local/state/enginebox/agent-runs/{}.lock", r.provider),
    )?;
    let env = environment(&home)?;
    let exe = find_binary(binary(&r.provider), &env)
        .ok_or_else(|| anyhow::anyhow!("Native client is not installed."))?;
    verify_client(&exe, &r.provider, &env, &cwd)?;
    let cancelled = || match run_id {
        Some(id) => cancellation_requested(&home, &r.provider, id),
        None => Ok(false),
    };
    let result = process::bounded_controlled(
        &native_command_with_config(r, &exe, &home, &cwd),
        Some(&env),
        Some(&cwd),
        if r.provider == "cursor" {
            b""
        } else {
            r.prompt.as_bytes()
        },
        Duration::from_secs(r.timeout_seconds),
        MAX_OUTPUT,
        process::Control {
            lease: Some(&lease),
            cancelled: &cancelled,
        },
    );
    if let Some(id) = run_id {
        // The writer lease still protects the native home while the helper cleans up.
        let _ = security::remove(&home, &cancel_path(id));
    }
    drop(lease);
    if result
        .as_ref()
        .is_ok_and(|output| output.exit_code == process::CANCELLED_EXIT_CODE)
    {
        // Concurrent fork/exec transitions may briefly inherit a CLOEXEC copy.
        let deadline = std::time::Instant::now() + Duration::from_millis(250);
        while lease_active(&home, &r.provider)? && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        ensure!(
            !lease_active(&home, &r.provider)?,
            "Native cancellation left a descendant holding its writer lease."
        );
    }
    result
}

pub fn validate_run_id(id: &str) -> Result<()> {
    ensure!(
        id.len() == 32
            && id
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "Invalid native run ID."
    );
    Ok(())
}
fn cancel_path(id: &str) -> String {
    format!(".local/state/enginebox/agent-runs/cancellations/{id}.json")
}
pub fn request_cancel(home: &Path, provider: &str, id: &str) -> Result<()> {
    crate::provider(provider)?;
    validate_run_id(id)?;
    security::absolute_existing(home)?;
    security::write_json(
        home,
        &cancel_path(id),
        &serde_json::json!({"run_id":id,"provider":provider}),
    )
}
pub fn cancellation_requested(home: &Path, provider: &str, id: &str) -> Result<bool> {
    crate::provider(provider)?;
    validate_run_id(id)?;
    Ok(security::read_json(home, &cancel_path(id))?
        .is_some_and(|v| v["run_id"] == id && v["provider"] == provider))
}

pub fn lease_active(home: &Path, provider: &str) -> Result<bool> {
    crate::provider(provider)?;
    security::absolute_existing(home)?;
    security::Lease::active(
        home,
        &format!(".local/state/enginebox/agent-runs/{provider}.lock"),
    )
}
pub fn session_id(output: &str) -> Option<String> {
    output
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find_map(|v| {
            let key = if v["type"] == "thread.started" {
                "thread_id"
            } else {
                "session_id"
            };
            v[key]
                .as_str()
                .filter(|s| security::exact_uuid(s))
                .map(str::to_owned)
        })
}
pub fn validate_result(v: &Value) -> Result<()> {
    let o = v
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Invalid native run response."))?;
    ensure!(
        o.len() == 4
            && v["exit_code"].as_i64().is_some()
            && v["output"]
                .as_str()
                .is_some_and(|s| s.len() <= MAX_OUTPUT * 3)
            && v["timed_out"].is_boolean()
            && v["output_truncated"].is_boolean(),
        "Invalid native run response."
    );
    Ok(())
}
