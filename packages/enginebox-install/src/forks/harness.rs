//! Per-run home layout and harness configuration (contract §4.2). The node
//! writes these files itself; nothing comes from the caller except the
//! prompt, the provider and an optional model id.
//!
//! The run's relay token (`ebr_…`) is written only into the run home's
//! harness config file, never into argv or the environment: Claude Code reads
//! it from `settings.json` `env`, Codex from its provider's
//! `experimental_bearer_token`. The prompt goes in on stdin.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};

/// The default Codex model on Xava Inference (OpenAI models only).
pub const DEFAULT_CODEX_MODEL: &str = "openai/gpt-5.4";
/// Claude Code's turn limit for one run.
pub const MAX_TURNS: u32 = 40;

/// `engine.json` `agents`: pinned harness binaries and the slot count.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AgentTools {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex: Option<PathBuf>,
    /// Git for Windows' `bash.exe` (Claude Code's Bash tool needs it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_bash: Option<PathBuf>,
    /// Concurrent agent runs on this machine (owner decision: at most 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slots: Option<u32>,
}
impl AgentTools {
    pub fn validate(&self) -> Result<()> {
        for (what, path) in [
            ("agents.claude", &self.claude),
            ("agents.codex", &self.codex),
            ("agents.git_bash", &self.git_bash),
        ] {
            if let Some(p) = path {
                ensure!(
                    p.is_absolute() && p.is_file(),
                    "{what} must be an absolute path to an existing file"
                );
                crate::release::reject_symlink_components(p)?;
            }
        }
        ensure!(
            self.slots.is_none_or(|s| (1..=3).contains(&s)),
            "agents.slots must be 1-3"
        );
        Ok(())
    }
    pub fn slots(&self) -> usize {
        self.slots.unwrap_or(3) as usize
    }
    fn binary(&self, provider: &str) -> Result<&Path> {
        match provider {
            "claude" => self.claude.as_deref(),
            "codex" => self.codex.as_deref(),
            _ => bail!("Unsupported agent provider"),
        }
        .with_context(|| format!("engine.json names no agents.{provider} binary"))
    }
}

/// One harness launch: everything the spawner needs.
#[derive(Debug)]
pub struct Plan {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// The complete environment (the spawner passes nothing else).
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    /// Written to the child's stdin, then closed.
    pub stdin: String,
}

/// What a run asks for.
pub struct Request<'a> {
    pub provider: &'a str,
    pub model: Option<&'a str>,
    pub prompt: &'a str,
    pub resume_id: Option<&'a str>,
}

/// The node's preamble, prepended to every member prompt.
pub const PREAMBLE: &str = "You are working in a fork of a Unity project that a live Unity Editor session \
shares with other people. Your changes are submitted for review and merged into the live project later. \
Do not edit ProjectSettings/ProjectVersion.txt, Packages/ or anything outside Assets/ unless the request asks \
for it. Scene edits are better done live in the Editor; prefer prefabs, scripts and assets. Git is not available.\n\n";

fn valid_model(m: &str) -> bool {
    !m.is_empty()
        && m.len() <= 100
        && m.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._/:[]".contains(&b))
}
fn valid_resume(id: &str) -> bool {
    !id.is_empty() && id.len() <= 100 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Directory names inside a run home.
pub fn home_dirs(home: &Path) -> [PathBuf; 4] {
    [
        home.join("AppData").join("Roaming"),
        home.join("AppData").join("Local"),
        home.join("tmp"),
        home.join(".config"),
    ]
}

fn toml_string(s: &str) -> String {
    // A TOML basic string: JSON escaping is a valid subset for these values.
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// Write the harness configuration into `home` and return the launch plan.
/// `relay` is the relay's base URL (`http://127.0.0.1:<port>`), `token` the
/// run's relay token.
pub fn prepare(
    tools: &AgentTools,
    request: &Request,
    home: &Path,
    cwd: &Path,
    relay: &str,
    token: &str,
    sandboxed: bool,
) -> Result<Plan> {
    ensure!(
        relay.starts_with("http://127.0.0.1:") && token.starts_with("ebr_"),
        "The relay must be loopback"
    );
    if let Some(m) = request.model {
        ensure!(valid_model(m), "Invalid model id");
    }
    if let Some(id) = request.resume_id {
        ensure!(valid_resume(id), "Invalid resume id");
    }
    ensure!(
        !request.prompt.trim().is_empty() && request.prompt.len() <= 64 * 1024,
        "The prompt must be 1-65536 bytes"
    );
    let program = tools.binary(request.provider)?.to_owned();
    for dir in home_dirs(home) {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = home.join("tmp");
    let mut env: Vec<(String, String)> = Vec::new();
    let mut set = |k: &str, v: &Path| env.push((k.into(), v.to_string_lossy().into_owned()));
    set("HOME", home);
    set("USERPROFILE", home);
    set("APPDATA", &home.join("AppData").join("Roaming"));
    set("LOCALAPPDATA", &home.join("AppData").join("Local"));
    set("XDG_CONFIG_HOME", &home.join(".config"));
    set("TEMP", &tmp);
    set("TMP", &tmp);
    set("TMPDIR", &tmp);
    let mut path_dirs: Vec<PathBuf> = Vec::new();
    if let Some(dir) = program.parent() {
        path_dirs.push(dir.to_owned());
    }
    if let Some(bash) = &tools.git_bash
        && let Some(dir) = bash.parent()
    {
        path_dirs.push(dir.to_owned());
    }
    if cfg!(windows) {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        path_dirs.push(PathBuf::from(&root).join("System32"));
        path_dirs.push(PathBuf::from(&root));
        env.push(("SystemRoot".into(), root));
        for key in [
            "ComSpec",
            "PATHEXT",
            "NUMBER_OF_PROCESSORS",
            "PROCESSOR_ARCHITECTURE",
            "OS",
        ] {
            if let Ok(v) = std::env::var(key) {
                env.push((key.into(), v));
            }
        }
    } else {
        path_dirs.extend(["/usr/local/bin", "/usr/bin", "/bin"].map(PathBuf::from));
    }
    let path = std::env::join_paths(path_dirs)?
        .to_string_lossy()
        .into_owned();
    env.push(("PATH".into(), path));
    env.push(("LANG".into(), "C.UTF-8".into()));
    env.push(("NO_COLOR".into(), "1".into()));
    let stdin = format!("{PREAMBLE}{}", request.prompt);
    let mut args: Vec<String> = Vec::new();
    match request.provider {
        "claude" => {
            let dir = home.join(".claude");
            std::fs::create_dir_all(&dir)?;
            let mut settings = json!({
                "env": {
                    "ANTHROPIC_BASE_URL": relay,
                    "ANTHROPIC_AUTH_TOKEN": token,
                    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
                    "DISABLE_AUTOUPDATER": "1",
                    "DISABLE_TELEMETRY": "1",
                    "DISABLE_ERROR_REPORTING": "1",
                },
                "includeCoAuthoredBy": false,
            });
            if let Some(m) = request.model {
                settings["model"] = json!(m);
            }
            write_private(
                &dir.join("settings.json"),
                &serde_json::to_vec_pretty(&settings)?,
            )?;
            env.push((
                "CLAUDE_CONFIG_DIR".into(),
                dir.to_string_lossy().into_owned(),
            ));
            if let Some(bash) = &tools.git_bash {
                env.push((
                    "CLAUDE_CODE_GIT_BASH_PATH".into(),
                    bash.to_string_lossy().into_owned(),
                ));
            }
            args.extend(
                [
                    "-p",
                    "--output-format",
                    "stream-json",
                    "--verbose",
                    "--permission-mode",
                    "acceptEdits",
                    "--allowedTools",
                    "Read,Write,Edit,Bash",
                    "--max-turns",
                ]
                .map(str::to_owned),
            );
            args.push(MAX_TURNS.to_string());
            if let Some(m) = request.model {
                args.extend(["--model".into(), m.into()]);
            }
            if let Some(id) = request.resume_id {
                args.extend(["--resume".into(), id.into()]);
            }
        }
        "codex" => {
            let dir = home.join(".codex");
            std::fs::create_dir_all(&dir)?;
            let model = request.model.unwrap_or(DEFAULT_CODEX_MODEL);
            ensure!(
                model.starts_with("openai/") || !model.contains('/'),
                "Codex runs OpenAI models only"
            );
            // The slot account is the boundary on Windows; elsewhere (no slot
            // users) Codex's own sandbox keeps the node's files out of reach.
            let sandbox = if sandboxed {
                "workspace-write"
            } else {
                "danger-full-access"
            };
            let config = format!(
                "model_provider = \"xinf\"\nmodel = {model}\napproval_policy = \"never\"\nsandbox_mode = \"{sandbox}\"\n\
check_for_update_on_startup = false\n\n[model_providers.xinf]\nname = \"Xava Inference\"\nbase_url = {base}\n\
wire_api = \"responses\"\nexperimental_bearer_token = {token}\nrequires_openai_auth = false\n\n\
[sandbox_workspace_write]\nnetwork_access = false\n",
                model = toml_string(model),
                base = toml_string(&format!("{relay}/v1")),
                token = toml_string(token),
            );
            write_private(&dir.join("config.toml"), config.as_bytes())?;
            // No auth.json: the provider's bearer token is the only credential.
            let _ = std::fs::remove_file(dir.join("auth.json"));
            env.push(("CODEX_HOME".into(), dir.to_string_lossy().into_owned()));
            args.push("exec".into());
            if let Some(id) = request.resume_id {
                args.extend(["resume".into(), id.into()]);
            }
            args.extend(["--json".into(), "--skip-git-repo-check".into()]);
            args.extend(["--model".into(), model.into()]);
            args.push("-".into());
        }
        _ => bail!("Unsupported agent provider"),
    }
    Ok(Plan {
        program,
        args,
        env,
        cwd: cwd.to_owned(),
        stdin,
    })
}

/// Write a file only its owner can read (Unix); on Windows the run home's
/// ACL (node + slot) applies.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let _ = std::fs::remove_file(path);
    #[cfg(unix)]
    {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(bytes)?;
    }
    #[cfg(not(unix))]
    std::fs::write(path, bytes)?;
    Ok(())
}

/// Remove the run token from the home after the run (the next run writes a
/// new one; the native session state stays for `resume`).
pub fn scrub(home: &Path) {
    let _ = std::fs::remove_file(home.join(".claude").join("settings.json"));
    let _ = std::fs::remove_file(home.join(".codex").join("config.toml"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools(dir: &Path) -> AgentTools {
        let dir = &dir.canonicalize().unwrap();
        let claude = dir.join("bin").join("claude");
        let codex = dir.join("bin").join("codex");
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(&claude, "").unwrap();
        std::fs::write(&codex, "").unwrap();
        AgentTools {
            claude: Some(claude),
            codex: Some(codex),
            git_bash: None,
            slots: Some(3),
        }
    }
    const TOKEN: &str = "ebr_00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    #[test]
    fn claude_gets_its_token_only_in_the_run_home() {
        let dir = tempfile::tempdir().unwrap();
        let t = tools(dir.path());
        t.validate().unwrap();
        let home = dir.path().join("home");
        let plan = prepare(
            &t,
            &Request {
                provider: "claude",
                model: Some("claude-sonnet-4-5"),
                prompt: "Make the card red",
                resume_id: Some("0f1e2d3c-aaaa-bbbb-cccc-1234567890ab"),
            },
            &home,
            &dir.path().join("tree"),
            "http://127.0.0.1:4242",
            TOKEN,
            false,
        )
        .unwrap();
        let argv = plan.args.join(" ");
        let env = format!("{:?}", plan.env);
        assert!(!argv.contains("ebr_") && !env.contains("ebr_"));
        assert!(!env.contains("ANTHROPIC") && !env.contains("XINF"));
        assert!(argv.contains("--output-format stream-json"));
        assert!(argv.contains("--max-turns 40"));
        assert!(argv.contains("--resume 0f1e2d3c-aaaa-bbbb-cccc-1234567890ab"));
        assert!(plan.stdin.starts_with(PREAMBLE) && plan.stdin.ends_with("Make the card red"));
        let settings: serde_json::Value = serde_json::from_slice(
            &std::fs::read(home.join(".claude").join("settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(settings["env"]["ANTHROPIC_AUTH_TOKEN"], TOKEN);
        assert_eq!(
            settings["env"]["ANTHROPIC_BASE_URL"],
            "http://127.0.0.1:4242"
        );
        assert_eq!(settings["model"], "claude-sonnet-4-5");
        let get = |k: &str| {
            plan.env
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(
            get("CLAUDE_CONFIG_DIR").unwrap(),
            home.join(".claude").to_string_lossy()
        );
        assert_eq!(get("HOME").unwrap(), home.to_string_lossy());
        // Nothing from the node's own environment beyond the system basics.
        for (k, _) in &plan.env {
            assert!(!k.starts_with("ENGINEBOX") && !k.starts_with("GIT_"), "{k}");
        }
        scrub(&home);
        assert!(!home.join(".claude").join("settings.json").exists());
    }

    #[test]
    fn codex_uses_the_relay_provider_without_auth_json() {
        let dir = tempfile::tempdir().unwrap();
        let t = tools(dir.path());
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(home.join(".codex").join("auth.json"), "{}").unwrap();
        let plan = prepare(
            &t,
            &Request {
                provider: "codex",
                model: None,
                prompt: "Add a menu",
                resume_id: None,
            },
            &home,
            &dir.path().join("tree"),
            "http://127.0.0.1:4242",
            TOKEN,
            true,
        )
        .unwrap();
        assert!(!format!("{:?}{:?}", plan.args, plan.env).contains("ebr_"));
        assert_eq!(plan.args[..3], ["exec", "--json", "--skip-git-repo-check"]);
        assert_eq!(plan.args.last().unwrap(), "-");
        let config = std::fs::read_to_string(home.join(".codex").join("config.toml")).unwrap();
        assert!(config.contains("model_provider = \"xinf\""));
        assert!(config.contains("base_url = \"http://127.0.0.1:4242/v1\""));
        assert!(config.contains("wire_api = \"responses\""));
        assert!(config.contains(&format!("experimental_bearer_token = \"{TOKEN}\"")));
        assert!(config.contains("model = \"openai/gpt-5.4\""));
        assert!(config.contains("sandbox_mode = \"workspace-write\""));
        assert!(!home.join(".codex").join("auth.json").exists());
    }

    #[test]
    fn caller_input_is_checked() {
        let dir = tempfile::tempdir().unwrap();
        let t = tools(dir.path());
        let home = dir.path().join("home");
        let bad = |provider, model: Option<&str>, resume: Option<&str>, relay: &str| {
            prepare(
                &t,
                &Request {
                    provider,
                    model,
                    prompt: "x",
                    resume_id: resume,
                },
                &home,
                &home,
                relay,
                TOKEN,
                false,
            )
            .is_err()
        };
        assert!(bad("cursor", None, None, "http://127.0.0.1:1"));
        assert!(bad(
            "claude",
            Some("x\" --dangerously"),
            None,
            "http://127.0.0.1:1"
        ));
        assert!(bad("claude", None, Some("../../x"), "http://127.0.0.1:1"));
        assert!(bad("claude", None, None, "https://xavainf.com"));
        assert!(bad(
            "codex",
            Some("anthropic/claude"),
            None,
            "http://127.0.0.1:1"
        ));
    }
}
