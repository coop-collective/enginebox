use crate::{process, security};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, path::Path, time::Duration};
pub const MAX_OUTPUT: usize = 262144;
/// Longest native run: a day, so build and verify tasks are not cut off; callers still choose a timeout per run.
pub const MAX_RUN_SECONDS: u64 = 86400;
/// Most caller-supplied environment variables one run may carry.
pub const MAX_ENV_VARS: usize = 32;
/// Largest total of caller-supplied variable names plus values, in bytes. The whole
/// request (prompt included) must still fit the 16 KiB native launch bound.
pub const MAX_ENV_BYTES: usize = 8192;
/// Shortest value that is scrubbed from a run's returned output.
pub const MIN_REDACTED_VALUE: usize = 8;
/// Replacement for scrubbed values; never longer than MIN_REDACTED_VALUE, so
/// scrubbing cannot grow bounded output.
pub const REDACTED: &str = "[secret]";
/// Variables Engine Box sets itself, credential/provider selectors, loader,
/// proxy/TLS trust and shell start-up hooks. A caller can never set these.
const DENIED_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "TERM",
    "DISPLAY",
    "BROWSER",
    "NO_OPEN_BROWSER",
    "DISABLE_AUTOUPDATER",
    "PWD",
    "OLDPWD",
    "IFS",
    "ENV",
    "BASH_ENV",
    "SHELLOPTS",
    "BASHOPTS",
    "PS4",
    "PROMPT_COMMAND",
    "CDPATH",
    "GLOBIGNORE",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "FTP_PROXY",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE",
    "NODE_OPTIONS",
    "NODE_PATH",
    "NODE_EXTRA_CA_CERTS",
    "NODE_TLS_REJECT_UNAUTHORIZED",
    "DEBUG",
    "SSH_AUTH_SOCK",
];
const DENIED_ENV_PREFIXES: &[&str] = &[
    "LD_",
    "DYLD_",
    "ANTHROPIC_",
    "CLAUDE_",
    "OPENAI_",
    "AZURE_OPENAI_",
    "CODEX_",
    "CURSOR_",
    "ENGINEBOX_",
    "XDG_",
    "NPM_CONFIG_",
    "RUST_",
];
/// True when a caller may set this variable for one native run.
pub fn env_key_allowed(key: &str) -> bool {
    let bytes = key.as_bytes();
    (1..=64).contains(&bytes.len())
        && bytes[0].is_ascii_uppercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_')
        && !DENIED_ENV.contains(&key)
        && !DENIED_ENV_PREFIXES.iter().any(|p| key.starts_with(p))
}
/// Bounds and denylist for a map of caller-supplied variables (a run's `env`,
/// a profile's default `env`, or the two merged). Messages name the rule, never a value.
pub fn validate_env_map(env: &BTreeMap<String, String>) -> Result<()> {
    ensure!(
        env.len() <= MAX_ENV_VARS,
        "env accepts at most {MAX_ENV_VARS} variables."
    );
    ensure!(
        env.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>() <= MAX_ENV_BYTES,
        "env names and values must total at most {MAX_ENV_BYTES} bytes."
    );
    for (key, value) in env {
        ensure!(
            env_key_allowed(key),
            "env names must match ^[A-Z][A-Z0-9_]{{0,63}}$ and cannot replace a variable Engine Box controls."
        );
        ensure!(!value.contains('\0'), "env values cannot contain NUL.");
    }
    Ok(())
}
#[derive(Clone, Serialize, Deserialize)]
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
    /// Per-run variables for the native CLI process. Values are secrets: they are
    /// never persisted, logged or echoed; records keep only the key names.
    /// Omitted from serialization when empty so older guest helpers still accept it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}
impl std::fmt::Debug for RunRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunRequest")
            .field("provider", &self.provider)
            .field("prompt", &self.prompt)
            .field("resume_id", &self.resume_id)
            .field("workdir", &self.workdir)
            .field("timeout_seconds", &self.timeout_seconds)
            .field("model", &self.model)
            .field("env_keys", &self.env_keys())
            .finish()
    }
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
            (1..=MAX_RUN_SECONDS).contains(&self.timeout_seconds),
            "timeout_seconds must be 1–86400."
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
        self.validate_env()?;
        ensure!(
            serde_json::to_vec(self)?.len() <= 16384,
            "Native launch request is too large."
        );
        Ok(())
    }
    fn validate_env(&self) -> Result<()> {
        validate_env_map(&self.env)
    }
    /// The caller-supplied variable names, which are safe to record.
    pub fn env_keys(&self) -> Vec<String> {
        self.env.keys().cloned().collect()
    }
}
/// Best-effort removal of caller-supplied secret values (raw and JSON-escaped)
/// from returned output. A native tool can still transform a value before
/// printing it, so callers must treat run output as private.
pub fn redact(output: &str, env: &BTreeMap<String, String>) -> String {
    let mut values: Vec<String> = Vec::new();
    for value in env.values().filter(|v| v.len() >= MIN_REDACTED_VALUE) {
        values.push(value.clone());
        let quoted = serde_json::to_string(value).unwrap_or_default();
        if let Some(escaped) = quoted.get(1..quoted.len().saturating_sub(1))
            && escaped != value
            && escaped.len() >= MIN_REDACTED_VALUE
        {
            values.push(escaped.to_owned());
        }
    }
    // Longest first, so a value containing another is removed whole.
    values.sort_by_key(|v| std::cmp::Reverse(v.len()));
    let mut out = output.to_owned();
    for value in values {
        if out.contains(&value) {
            out = out.replace(&value, REDACTED);
        }
    }
    out
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
/// Package-manager caches in the guest user's home. Codex's workspace-write
/// sandbox makes the home read-only, so corepack, pnpm, npm, bun and cargo
/// fail before a project's tests run; these directories alone are writable,
/// and they persist between runs on the workspace like any cache.
pub const PACKAGE_CACHE_DIRS: &[&str] = &[
    ".cache",
    ".npm",
    ".local/share/pnpm",
    ".bun/install/cache",
    ".cargo/registry",
    ".cargo/git",
];
/// The package caches under `home` that are real directories (never symlinks).
pub fn package_cache_roots(home: &Path) -> Vec<String> {
    PACKAGE_CACHE_DIRS
        .iter()
        .map(|d| home.join(d))
        .filter(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()))
        .filter_map(|p| p.to_str().map(str::to_owned))
        .collect()
}
/// Create the package caches a Codex run may write (best effort: one that
/// cannot be created is simply not offered to the sandbox).
pub fn ensure_package_caches(home: &Path) {
    for dir in PACKAGE_CACHE_DIRS {
        let _ = std::fs::create_dir_all(home.join(dir));
    }
}
/// A TOML array of path strings (JSON string escaping is valid TOML).
pub fn toml_paths(paths: &[String]) -> String {
    format!(
        "[{}]",
        paths
            .iter()
            .map(|p| serde_json::to_string(p).unwrap_or_default())
            .collect::<Vec<_>>()
            .join(", ")
    )
}
pub fn native_command_with_config(
    r: &RunRequest,
    exe: &str,
    home: &Path,
    cwd: &Path,
) -> Vec<String> {
    let mut args = native_command(r, exe);
    if r.provider == "codex" {
        let roots = package_cache_roots(home);
        if !roots.is_empty() {
            args.splice(
                args.len() - 1..args.len() - 1,
                [
                    "-c".into(),
                    format!(
                        "sandbox_workspace_write.writable_roots={}",
                        toml_paths(&roots)
                    ),
                ],
            );
        }
    }
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
    execute_inner(r, home, workspace, run_id, None, None, None)
}

/// Run under a profile: the envelope is re-validated here, its files are
/// materialized into this run's private directory, and that directory is
/// removed again before the writer lease is released.
pub fn execute_profiled(
    envelope: &crate::profile::Envelope,
    home: &Path,
    workspace: &Path,
    run_id: &str,
) -> Result<process::Output> {
    let files = envelope.validate()?;
    execute_inner(
        &envelope.request,
        home,
        workspace,
        Some(run_id),
        Some((&envelope.block, &files)),
        None,
        None,
    )
}

/// Run with a hosted repository (and optionally a profile): the envelope is
/// re-validated here, the run's scoped git config is written into its private
/// directory, and that directory is removed again before the writer lease is
/// released.
pub fn execute_with_repository(
    envelope: &crate::repository::Envelope,
    home: &Path,
    workspace: &Path,
    run_id: &str,
) -> Result<process::Output> {
    let files = envelope.validate()?;
    let profile = envelope.profile.as_ref().zip(files.as_ref());
    execute_inner(
        &envelope.request,
        home,
        workspace,
        Some(run_id),
        profile.map(|(p, files)| (&p.block, files)),
        Some(&envelope.repository),
        envelope.checkout.as_deref(),
    )
}

type ProfileInput<'a> = (
    &'a crate::profile::ProviderProfile,
    &'a BTreeMap<String, Vec<u8>>,
);

fn execute_inner(
    r: &RunRequest,
    home: &Path,
    workspace: &Path,
    run_id: Option<&str>,
    profile: Option<ProfileInput<'_>>,
    repository: Option<&crate::repository::Access>,
    checkout: Option<&str>,
) -> Result<process::Output> {
    r.validate()?;
    ensure!(
        (profile.is_none() && repository.is_none()) || run_id.is_some(),
        "Profile and repository runs require a controller run ID."
    );
    ensure!(
        checkout.is_none() || repository.is_some(),
        "Only repository runs get a checkout."
    );
    if let Some(id) = run_id {
        validate_run_id(id)?;
    }
    let home = security::absolute_existing(home)?;
    let workspace = security::absolute_existing(workspace)?;
    if checkout.is_none() {
        let cwd = workspace.join(&r.workdir).canonicalize()?;
        ensure!(
            cwd.is_dir() && cwd.starts_with(&workspace),
            "workdir escapes its assigned workspace."
        );
    }
    let _state = security::safe_path(&home, ".local/state/enginebox/agent-runs/placeholder", true)?;
    security::private_dir(&home.join(".local/state/enginebox/agent-runs"))?;
    let lease = security::Lease::acquire(
        &home,
        &format!(".local/state/enginebox/agent-runs/{}.lock", r.provider),
    )?;
    // A repository run works in a freshly emptied checkout at a stable path
    // per repository (harnesses key their sessions and memory by cwd).
    let work = match (checkout, run_id) {
        (Some(name), Some(id)) => Some(crate::repository::open_checkout(
            &home, &workspace, name, id,
        )?),
        _ => None,
    };
    let cwd = match &work {
        Some(work) => {
            let cwd = work.path.join(&r.workdir);
            let mut current = work.path.clone();
            for part in Path::new(&r.workdir).components() {
                if let std::path::Component::Normal(part) = part {
                    current.push(part);
                    security::check_components(&current)?;
                    if !current.exists() {
                        std::fs::create_dir(&current)?;
                    }
                }
            }
            let cwd = cwd.canonicalize()?;
            ensure!(
                cwd.is_dir() && cwd.starts_with(work.path.canonicalize()?),
                "workdir escapes its checkout."
            );
            cwd
        }
        None => workspace.join(&r.workdir).canonicalize()?,
    };
    ensure!(
        cwd.is_dir() && cwd.starts_with(&workspace),
        "workdir escapes its assigned workspace."
    );
    // Codex's workspace-write sandbox keeps the `.git` at the top of its
    // writable root read-only (it even mounts an empty one there), so a Codex
    // repository run starts in the workspace root, one level above its
    // checkout: the checkout's own `.git` is then writable and the agent can
    // clone, commit and push. `ENGINEBOX_RUN_DIR` still names the checkout.
    let launch_cwd = match &work {
        Some(work) if r.provider == "codex" && r.workdir == "." => work
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| cwd.clone()),
        _ => cwd.clone(),
    };
    let cwd = launch_cwd;
    let env = environment(&home)?;
    if r.provider == "codex" {
        ensure_package_caches(&home);
    }
    let exe = find_binary(binary(&r.provider), &env)
        .ok_or_else(|| anyhow::anyhow!("Native client is not installed."))?;
    verify_client(&exe, &r.provider, &env, &cwd)?;
    // The version preflight uses only Engine Box's environment; caller variables
    // reach the native run alone and can never replace a controlled variable.
    let caller_env = match profile {
        Some((block, _)) => crate::profile::merged_env(block, r)?,
        None => r.env.clone(),
    };
    ensure!(
        repository.is_none()
            || !caller_env
                .keys()
                .any(|k| crate::repository::reserved_env_key(k)),
        "env cannot set git configuration variables in a repository run."
    );
    let mut run_env = env.clone();
    for (key, value) in &caller_env {
        ensure!(
            env_key_allowed(key) && !env.contains_key(key),
            "env cannot replace a variable Engine Box controls."
        );
        run_env.insert(key.clone(), value.clone());
    }
    let mut argv = native_command_with_config(r, &exe, &home, &cwd);
    let mut redacted = caller_env.clone();
    if let (Some((block, files)), Some(id)) = (profile, run_id) {
        let materialized = crate::profile::materialize(&home, id, &r.provider, block, files)?;
        argv = crate::profile::command(r, &exe, block, &materialized)?;
        if let Some(codex_home) = &materialized.codex_home {
            run_env.insert(
                "CODEX_HOME".into(),
                codex_home
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("Invalid profile path."))?
                    .into(),
            );
        }
        // Delegated Claude access lives in user settings; a profile that does
        // not load them still needs it, so Engine Box passes it itself.
        if r.provider == "claude"
            && block
                .setting_sources
                .as_ref()
                .is_some_and(|s| !s.iter().any(|x| x == "user"))
            && let Some(token) =
                security::read_json(&home, ".claude/settings.json")?.and_then(|v| {
                    v["env"]["CLAUDE_CODE_OAUTH_TOKEN"]
                        .as_str()
                        .map(str::to_owned)
                })
        {
            redacted.insert("CLAUDE_CODE_OAUTH_TOKEN".into(), token.clone());
            run_env.insert("CLAUDE_CODE_OAUTH_TOKEN".into(), token);
        }
    }
    if let (Some(access), Some(id)) = (repository, run_id) {
        let variables = crate::repository::materialize(&home, id, access)
            .and_then(|config| crate::repository::environment(access, &config));
        let variables = match variables {
            Ok(variables) => variables,
            Err(error) => {
                let _ = crate::repository::remove_run_dir(&home, id);
                if profile.is_some() {
                    let _ = crate::profile::remove_run_dir(&home, id);
                }
                return Err(error);
            }
        };
        run_env.extend(variables);
        if let Some(work) = &work {
            run_env.insert(
                crate::repository::ENV_RUN_DIR.into(),
                work.path
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("Invalid checkout path."))?
                    .into(),
            );
            run_env.insert(crate::repository::ENV_RUN_ID.into(), id.into());
        }
        // The token is in a file the agent can read; never let it reach output.
        redacted.insert("ENGINEBOX_REPO_TOKEN".into(), access.token.clone());
        if let Some((secret, _)) = access.token.split_once("?expires=") {
            redacted.insert("ENGINEBOX_REPO_TOKEN_SECRET".into(), secret.to_owned());
        }
    }
    let cancelled = || match run_id {
        Some(id) => cancellation_requested(&home, &r.provider, id),
        None => Ok(false),
    };
    let result = process::bounded_controlled(
        &argv,
        Some(&run_env),
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
        if profile.is_some() {
            let _ = crate::profile::remove_run_dir(&home, id);
        }
        if repository.is_some() {
            let _ = crate::repository::remove_run_dir(&home, id);
        }
    }
    // A per-run fallback checkout is removed here, under the writer lease;
    // the stable checkout stays until the next run empties it.
    drop(work);
    drop(lease);
    let result = result.map(|mut output| {
        if !redacted.is_empty() {
            output.output = redact(&output.output, &redacted);
        }
        output
    });
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
