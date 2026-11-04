//! Unity as the low-privilege `enginebox-editor` account (contract §3),
//! behind `engine.json` `"editor_account": "enginebox-editor"`. The default
//! stays the single-account behaviour (Unity runs as the node) until the
//! machine passes `assets/qualify-editor-account.ps1`, which runs
//! `enginebox-node engine-qualify-editor` ([`qualify`]) as the node account.
//!
//! In editor mode the node shares only the dev tree (modify), the staged
//! editor bridge and `work\engine\editor\<session>\` (bridge configuration,
//! bridge token, session package, Unity log, handshake files) with the
//! editor account. The licensing client and Unity start with
//! `CreateProcessWithLogonW` (with the editor's profile, on `WinSta0\Default`)
//! inside the session's Job Object. The stream host, git and every secret
//! stay with the node. The licensing client's logs live in the editor's
//! profile, so the node scrubs them by starting `enginebox-node
//! scrub-licensing-logs` as the editor: it removes whatever follows
//! `--accessToken` and never needs the token itself.
use anyhow::Result;
use std::path::Path;

/// Replace the value after every `--accessToken` (` ` or `=` separated) in
/// Unity licensing client logs under `dir`. Returns files changed.
pub fn scrub_access_token_args(dir: &Path) -> Result<usize> {
    if !dir.is_dir() {
        return Ok(0);
    }
    let pattern = regex::bytes::Regex::new(r#"(--accessToken[= ]+"?)[^\s"',;<]{8,}"#)?;
    let mut changed = 0;
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let named = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("Unity.Licensing.Client") && n.contains(".log"));
        if !named || !path.is_file() {
            continue;
        }
        let text = std::fs::read(&path)?;
        if !pattern.is_match(&text) {
            continue;
        }
        let out = pattern.replace_all(&text, &b"${1}<redacted>"[..]);
        std::fs::write(&path, out.as_ref())?;
        changed += 1;
    }
    Ok(changed)
}
/// Whether any licensing log under `dir` still holds a token after
/// `--accessToken`.
pub fn access_token_remains(dir: &Path) -> Result<bool> {
    let pattern = regex::bytes::Regex::new(r#"--accessToken[= ]+"?[^\s"',;<]{8,}"#)?;
    if !dir.is_dir() {
        return Ok(false);
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("Unity.Licensing.Client"))
            && path.is_file()
            && pattern.is_match(&std::fs::read(&path)?)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(windows)]
pub use imp::*;

#[cfg(windows)]
mod imp {
    use super::*;
    use crate::engine::{
        EngineConfig, Group, SessionContext, add_packages, bridge_config, licensing_argv,
        licensing_client, stage_bridge, unity_argv, write_session_package,
    };
    use crate::forks::{
        slots::{Credential, Limits, load_accounts},
        windows::{self, LogonChild, LogonSpec},
    };
    use anyhow::Context;
    use anyhow::{bail, ensure};
    use serde_json::{Value, json};
    use std::{
        path::PathBuf,
        time::{Duration, Instant},
    };
    use tokio::sync::watch;

    pub type EditorChild = LogonChild;
    pub fn child_pid(c: &EditorChild) -> u32 {
        c.pid
    }
    pub fn child_exit(c: &EditorChild) -> Option<u32> {
        c.exit_code()
    }

    /// The editor account's sign-in and SID.
    pub struct Editor {
        pub cred: Credential,
        pub sid: String,
    }

    /// The editor account when `engine.json` asks for it.
    pub fn resolve(ctx: &SessionContext) -> Result<Option<Editor>> {
        if ctx.config.editor_account.is_none() {
            return Ok(None);
        }
        let cred = ctx
            .accounts
            .as_ref()
            .and_then(|a| a.editor.clone())
            .context("engine.json asks for the editor account, but the installer created none; rerun install-worker-node.ps1 -EngineAccount")?;
        let sid = windows::account_sid(&cred.user)?;
        Ok(Some(Editor { cred, sid }))
    }

    impl Editor {
        /// Share the dev tree and the staged bridge with the editor and
        /// return its session directory.
        pub fn prepare(&self, work: &Path, id: &str, checkout: &Path) -> Result<PathBuf> {
            let dir = work.join("engine").join("editor").join(id);
            crate::release::private_dir(&dir)?;
            windows::share_dir(&dir, &self.sid)?;
            windows::share_dir(checkout, &self.sid)?;
            let packages = work.join("engine").join("packages");
            std::fs::create_dir_all(&packages)?;
            windows::share_dir(&packages, &self.sid)?;
            Ok(dir)
        }
        /// Start `program` as the editor in the session's job.
        pub fn spawn(
            &self,
            program: &Path,
            args: &[String],
            cwd: &Path,
            group: &Group,
        ) -> Result<LogonChild> {
            let env = editor_env();
            windows::spawn_logon_into(
                &self.cred,
                &LogonSpec {
                    program,
                    args,
                    env: &env,
                    cwd,
                    profile: true,
                    desktop: None,
                    limits: None,
                    pipes: false,
                },
                Some(group.job_handle()),
            )
        }
        /// Run to completion (`None` on timeout or stop; the job ends it).
        pub async fn run(
            &self,
            program: &Path,
            args: &[String],
            cwd: &Path,
            group: &Group,
            limit: Duration,
            stop: &mut watch::Receiver<bool>,
        ) -> Result<Option<u32>> {
            let child = self.spawn(program, args, cwd, group)?;
            let t = Instant::now();
            loop {
                if let Some(code) = child.exit_code() {
                    return Ok(Some(code));
                }
                if t.elapsed() > limit || *stop.borrow() {
                    child.terminate();
                    return Ok(None);
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
        /// Scrub the licensing client's logs in the editor's profile.
        pub async fn scrub(&self, group: &Group, stop: &mut watch::Receiver<bool>) -> Result<bool> {
            let me = std::env::current_exe()?;
            let cwd = me.parent().context("node directory")?.to_owned();
            let code = self
                .run(
                    &me,
                    &["scrub-licensing-logs".into()],
                    &cwd,
                    group,
                    Duration::from_secs(60),
                    stop,
                )
                .await?;
            Ok(code == Some(0))
        }
    }

    /// A minimal environment: the profile supplies the rest.
    fn editor_env() -> Vec<(String, String)> {
        [
            "SystemRoot",
            "windir",
            "ComSpec",
            "PATHEXT",
            "PATH",
            "NUMBER_OF_PROCESSORS",
            "PROCESSOR_ARCHITECTURE",
            "OS",
            "ProgramData",
            "ProgramFiles",
            "ProgramFiles(x86)",
            "CommonProgramFiles",
            "PUBLIC",
            "SystemDrive",
        ]
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect()
    }

    // ------------------------------------------------------------ qualification

    struct Checks(Vec<Value>);
    impl Checks {
        fn add(&mut self, name: &str, ok: bool, detail: impl Into<String>) -> bool {
            self.0
                .push(json!({"name": name, "ok": ok, "detail": detail.into()}));
            ok
        }
    }

    /// Run `cmd /c <line>` as `cred` and return (exit, stdout).
    async fn cmd_as(
        cred: &Credential,
        line: &str,
        desktop: Option<&str>,
        profile: bool,
    ) -> Result<(u32, String)> {
        use std::io::Read;
        let comspec =
            std::env::var("ComSpec").unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".into());
        let sys = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let mut env = editor_env();
        env.push((
            "TEMP".into(),
            std::env::temp_dir().to_string_lossy().into_owned(),
        ));
        let args = vec!["/d".to_owned(), "/c".to_owned(), line.to_owned()];
        let child = windows::spawn_logon(
            cred,
            &LogonSpec {
                program: Path::new(&comspec),
                args: &args,
                env: &env,
                cwd: Path::new(&sys),
                profile,
                desktop,
                limits: Some(Limits::default()),
                pipes: true,
            },
        )?;
        let (stdin, mut stdout, _stderr) = child.take_pipes()?;
        drop(stdin);
        let reader = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = stdout.read_to_string(&mut s);
            s
        });
        let t = Instant::now();
        let code = loop {
            if let Some(c) = child.exit_code() {
                break c;
            }
            if t.elapsed() > Duration::from_secs(60) {
                child.terminate();
                bail!("timed out");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        Ok((code, reader.join().unwrap_or_default()))
    }

    /// `enginebox-node engine-qualify-editor`: prove the editor account and
    /// slot isolation on this machine. Writes the result JSON to `out`.
    pub async fn qualify(
        config_path: &Path,
        project: &Path,
        token_file: Option<&Path>,
        seconds: u64,
    ) -> Result<Value> {
        let mut c = Checks(Vec::new());
        let config = EngineConfig::load(config_path)?;
        let root = config_path
            .parent()
            .context("engine.json has no directory")?;
        let private = root.join("private");
        let token = match token_file {
            Some(f) => {
                let t = std::fs::read_to_string(f)
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                let _ = std::fs::remove_file(f);
                (!t.is_empty()).then_some(t)
            }
            None => None,
        };
        let accounts = load_accounts(&private)?;
        let Some(editor) = accounts.as_ref().and_then(|a| a.editor.clone()) else {
            c.add(
                "accounts",
                false,
                "no sealed enginebox-editor account (rerun the installer)",
            );
            return Ok(json!({"ok": false, "checks": c.0, "unity_log_tail": ""}));
        };
        c.add(
            "accounts",
            true,
            format!(
                "{} slot account(s) sealed",
                accounts.as_ref().map_or(0, |a| a.slots.len())
            ),
        );
        let sid = windows::account_sid(&editor.user)?;
        match cmd_as(&editor, "whoami", None, true).await {
            Ok((0, out)) => c.add(
                "editor-logon",
                out.to_lowercase().contains("enginebox-editor"),
                out.trim().to_owned(),
            ),
            Ok((code, out)) => c.add(
                "editor-logon",
                false,
                format!("exit {code}: {}", out.trim()),
            ),
            Err(e) => c.add("editor-logon", false, format!("{e:#}")),
        };
        for (name, file) in [
            (
                "editor-cannot-read-node-identity",
                private.join("worker.json"),
            ),
            (
                "editor-cannot-read-accounts",
                private.join("accounts.dpapi"),
            ),
        ] {
            let r = cmd_as(&editor, &format!("type \"{}\"", file.display()), None, true).await;
            c.add(
                name,
                matches!(r, Ok((code, _)) if code != 0),
                format!("{:?}", r.map(|(code, _)| code)),
            );
        }
        // Unity as the editor, in this (the node's) desktop session.
        let work = root.join("work");
        let editor_dir = work.join("engine").join("editor").join("qualify");
        let _ = std::fs::remove_dir_all(&editor_dir);
        let ed = Editor {
            cred: editor.clone(),
            sid: sid.clone(),
        };
        let mut log_tail = String::new();
        let unity_ok: Result<()> = async {
            let version = std::fs::read_to_string(project.join("ProjectSettings/ProjectVersion.txt"))?
                .lines()
                .find_map(|l| l.strip_prefix("m_EditorVersion:").map(|v| v.trim().to_owned()))
                .context("no editor version")?;
            let unity = config.unity.get(&version).with_context(|| format!("Unity {version} is not in engine.json"))?.clone();
            let dir = ed.prepare(&work, "qualify", project)?;
            let package = dir.join("packages").join(crate::engine::SESSION_PACKAGE);
            write_session_package(&package)?;
            let bridge = stage_bridge(&config.editor_bridge, &work.join("engine").join("packages"))?;
            add_packages(&project.join("Packages").join("manifest.json"), &bridge, &package)?;
            let port = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
            let token_value = uuid::Uuid::new_v4().simple().to_string() + &uuid::Uuid::new_v4().simple().to_string();
            let spec: crate::engine::Spec = serde_json::from_value(json!({"repository": "qualify", "ref": "qualify",
                "project_path": "", "engine": {"kind": "unity", "version": version}, "width": 1280, "height": 720,
                "namespace": "qualify", "session": "qualify", "tag": 0}))?;
            let bridge_json = dir.join("bridge.json");
            std::fs::write(&bridge_json, serde_json::to_vec(&bridge_config(port, &token_value, &spec))?)?;
            let group = Group::new()?;
            let (_tx, mut stop) = watch::channel(false);
            if let Some(token) = &token {
                let code = ed.run(&licensing_client(&unity), &licensing_argv(token), project, &group, Duration::from_secs(300), &mut stop).await?;
                let scrubbed = ed.scrub(&group, &mut stop).await?;
                c.add("licensing", code == Some(0), format!("licensing client exit {code:?}"));
                c.add("licensing-logs-scrubbed", scrubbed, "scrub-licensing-logs ran as the editor");
            }
            let log = dir.join("unity.log");
            let argv: Vec<String> = unity_argv(project, &bridge_json, &log).iter().map(|a| a.to_string_lossy().into_owned()).collect();
            let child = ed.spawn(&unity, &argv, project, &group)?;
            let t = Instant::now();
            let limit = Duration::from_secs(seconds);
            let mut first: Option<Value> = None;
            let mut playing = false;
            while t.elapsed() < limit {
                if child.exit_code().is_some() {
                    break;
                }
                if let Some(s) = crate::engine::bridge_status(port, &token_value).await {
                    if first.is_none() && s["compiling"] != true {
                        first = Some(s.clone());
                    }
                    if s["playing"] == true {
                        playing = true;
                        let frames0 = s["frames"].as_u64().unwrap_or(0);
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        let s2 = crate::engine::bridge_status(port, &token_value).await.unwrap_or_default();
                        let frames1 = s2["frames"].as_u64().unwrap_or(0);
                        c.add("unity-renders", frames1 > frames0, format!("frames {frames0} -> {frames1}"));
                        let frame = reqwest::Client::builder().no_proxy().build()?
                            .get(format!("http://127.0.0.1:{port}/frame")).bearer_auth(&token_value).send().await?;
                        let ok = frame.status().is_success();
                        let bytes = frame.bytes().await.map(|b| b.len()).unwrap_or(0);
                        c.add("bridge-frame-to-node", ok && bytes > 1000, format!("{bytes} byte JPEG"));
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            c.add("bridge-answers-node", first.is_some(), "bridge /status from the node account");
            c.add("play-mode", playing, "session package entered Play mode");
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            let renderer = text.lines().find(|l| l.contains("Renderer:")).unwrap_or("").trim().to_owned();
            c.add(
                "gpu-renderer",
                !renderer.is_empty() && !renderer.contains("Basic Render") && !renderer.contains("WARP"),
                renderer,
            );
            let mut group = group;
            group.terminate().await;
            tokio::time::sleep(Duration::from_secs(2)).await;
            c.add("job-ends-editor", child.exit_code().is_some(), "TerminateJobObject ended Unity");
            log_tail = text.lines().rev().take(80).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
            Ok(())
        }
        .await;
        if let Err(e) = unity_ok {
            c.add("unity", false, format!("{e:#}"));
        }
        // Slot isolation.
        if let Some(slot) = accounts.as_ref().and_then(|a| a.slots.first().cloned()) {
            let slot_sid = windows::account_sid(&slot.user)?;
            let desk = windows::ensure_agent_desktop(std::slice::from_ref(&slot_sid));
            c.add(
                "agent-desktop",
                desk.is_ok(),
                desk.err().map(|e| format!("{e:#}")).unwrap_or_default(),
            );
            match cmd_as(&slot, "whoami", Some(windows::AGENT_DESKTOP), false).await {
                Ok((0, out)) => c.add(
                    "slot-logon",
                    out.to_lowercase().contains(&slot.user),
                    out.trim().to_owned(),
                ),
                other => c.add(
                    "slot-logon",
                    false,
                    format!("{:?}", other.map(|(code, _)| code)),
                ),
            };
            for (name, file) in [
                (
                    "slot-cannot-read-node-identity",
                    private.join("worker.json"),
                ),
                (
                    "slot-cannot-read-dev-tree",
                    project.join("ProjectSettings").join("ProjectVersion.txt"),
                ),
            ] {
                let r = cmd_as(
                    &slot,
                    &format!("type \"{}\"", file.display()),
                    Some(windows::AGENT_DESKTOP),
                    false,
                )
                .await;
                c.add(
                    name,
                    matches!(r, Ok((code, _)) if code != 0),
                    format!("{:?}", r.map(|(code, _)| code)),
                );
            }
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let port = listener.local_addr()?.port();
            tokio::spawn(async move {
                while let Ok((mut s, _)) = listener.accept().await {
                    use tokio::io::AsyncWriteExt;
                    let _ = s
                        .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
                        .await;
                }
            });
            let curl = r"%SystemRoot%\System32\curl.exe -s -m 5 -o NUL";
            let local = cmd_as(
                &slot,
                &format!("{curl} http://127.0.0.1:{port}/"),
                Some(windows::AGENT_DESKTOP),
                false,
            )
            .await;
            c.add(
                "slot-reaches-loopback",
                matches!(local, Ok((0, _))),
                format!("{:?}", local.map(|(code, _)| code)),
            );
            let lan = cmd_as(
                &slot,
                &format!("{curl} http://192.168.1.1/"),
                Some(windows::AGENT_DESKTOP),
                false,
            )
            .await;
            c.add(
                "slot-lan-blocked",
                matches!(lan, Ok((code, _)) if code != 0),
                format!("{:?}", lan.map(|(code, _)| code)),
            );
        } else {
            c.add("slots", false, "no slot accounts sealed");
        }
        let _ = std::fs::remove_dir_all(&editor_dir);
        let ok = c.0.iter().all(|x| x["ok"] == true);
        ensure!(!c.0.is_empty(), "no checks ran");
        Ok(json!({"ok": ok, "checks": c.0, "unity_log_tail": log_tail}))
    }
}

/// Off Windows there is no editor account: `resolve` is always `None`.
#[cfg(not(windows))]
mod stub {
    use anyhow::{Result, bail};
    use std::{
        path::{Path, PathBuf},
        time::Duration,
    };
    use tokio::sync::watch;
    pub enum Never {}
    pub type EditorChild = Never;
    pub struct Editor;
    pub fn resolve(_: &crate::engine::SessionContext) -> Result<Option<Editor>> {
        Ok(None)
    }
    pub fn child_pid(c: &EditorChild) -> u32 {
        match *c {}
    }
    pub fn child_exit(c: &EditorChild) -> Option<u32> {
        match *c {}
    }
    impl Editor {
        pub fn prepare(&self, _: &Path, _: &str, _: &Path) -> Result<PathBuf> {
            bail!("no editor account")
        }
        pub fn spawn(
            &self,
            _: &Path,
            _: &[String],
            _: &Path,
            _: &crate::engine::Group,
        ) -> Result<EditorChild> {
            bail!("no editor account")
        }
        pub async fn run(
            &self,
            _: &Path,
            _: &[String],
            _: &Path,
            _: &crate::engine::Group,
            _: Duration,
            _: &mut watch::Receiver<bool>,
        ) -> Result<Option<u32>> {
            bail!("no editor account")
        }
        pub async fn scrub(
            &self,
            _: &crate::engine::Group,
            _: &mut watch::Receiver<bool>,
        ) -> Result<bool> {
            bail!("no editor account")
        }
    }
}
#[cfg(not(windows))]
pub use stub::*;

/// Off Windows there is no editor account.
#[cfg(not(windows))]
pub async fn qualify(
    _config: &Path,
    _project: &Path,
    _token_file: Option<&Path>,
    _seconds: u64,
) -> Result<serde_json::Value> {
    anyhow::bail!("The editor account is a Windows engine machine feature")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn licensing_logs_lose_the_value_after_access_token() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("Unity.Licensing.Client.log");
        std::fs::write(
            &log,
            "args: --activate-all --accessToken abcdefghijklmnop123 --include-personal\n\
             cmd=\"--accessToken=QRSTUVWXYZ0123456789\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("other.log"),
            "--accessToken keepthisvalue123",
        )
        .unwrap();
        assert!(access_token_remains(dir.path()).unwrap());
        assert_eq!(scrub_access_token_args(dir.path()).unwrap(), 1);
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(!text.contains("abcdefghijklmnop123") && !text.contains("QRSTUVWXYZ"));
        assert!(text.contains("--accessToken <redacted> --include-personal"));
        assert!(!access_token_remains(dir.path()).unwrap());
        assert_eq!(scrub_access_token_args(dir.path()).unwrap(), 0);
    }
}
