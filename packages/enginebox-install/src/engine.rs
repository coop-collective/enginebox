//! Engine sessions on a personal Windows engine machine (docs/ENGINE-SESSIONS.md).
//!
//! The node runs as the dedicated `enginebox` account in its own interactive
//! session. For each session the controller places here it checks out the
//! exact ref (Git LFS included) into a per-repository checkout under the
//! node's work directory, adds the Gamejam editor bridge and a small session
//! package to the project's `Packages/manifest.json`, activates Unity with the
//! starting member's access token, starts the Editor, waits until the bridge
//! reports Play mode, and runs the Gamejam stream host publishing to the MoQ
//! relay. Every process of a session is held in its own Job Object; stopping,
//! a lease loss or an epoch change terminates the job. A startup sweep kills
//! anything a previous node process left behind.
//!
//! A world session (`mode: "world"`) checks out and activates the same way,
//! then runs the owner-configured `world_host` (Gamejam's
//! `gamejam-world-host`) instead of the Editor and stream host. It hands the
//! host an owner-only session file (`--session`), with the room's one-time
//! enrollment code inside, and is ready once the host prints
//! `{"event":"world-host-connected"}`. The host's content-addressed player
//! builds persist across sessions in `work\engine\world-builds`, lent to the
//! session's work directory while it runs.
//!
//! Owner-written `engine.json` (beside the node's `config/` directory, e.g.
//! `C:\ProgramData\Enginebox\engine-node\engine.json`) names the tools; the
//! controller never chooses an executable. Secrets never appear in argv or
//! logs, with one documented exception: Unity's licensing client takes the
//! access token only as `--accessToken`.
use crate::release::{atomic_json, private_dir, reject_symlink_components};
use anyhow::{Context, Result, bail, ensure};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt},
    process::{Child, Command},
    sync::{mpsc, watch},
};

pub const CONFIG_FILE: &str = "engine.json";
pub const BRIDGE_PACKAGE: &str = "dev.unityjam.editor-bridge";
pub const SESSION_PACKAGE: &str = "dev.enginebox.session";
/// The stream host's outer permit (its maximum); the node restarts it while
/// the session runs.
pub const STREAM_SECONDS: u64 = 600;
const DEFAULT_STARTUP_SECONDS: u64 = 3600;
const GIT_SECONDS: u64 = 3600;
const LICENSE_SECONDS: u64 = 300;
/// The world host's readiness line on stdout.
pub const WORLD_READY_EVENT: &str = "world-host-connected";
/// The world host's build cache directory (persistent under `work\engine`).
pub const WORLD_BUILDS: &str = "world-builds";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EngineConfig {
    pub schema: u32,
    /// Unity version → `Unity.exe`.
    pub unity: BTreeMap<String, PathBuf>,
    /// `enginebox-stream-host.exe` built with `--features moq`.
    pub stream_host: PathBuf,
    /// An `ffmpeg.exe` with libvpx.
    pub ffmpeg: PathBuf,
    /// The `packages/unity-editor-bridge` directory.
    pub editor_bridge: PathBuf,
    /// Optional `git.exe`; otherwise found on PATH or in Program Files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<PathBuf>,
    /// How long the Editor may take to import and enter Play mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_timeout_seconds: Option<u64>,
    /// `global` keeps Unity's services on the global region. Unity's config CDN
    /// answers by IP, and from mainland China or Hong Kong it hands out Unity
    /// China, which does not know global Unity IDs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unity_region: Option<String>,
    /// Optional `gamejam-world-host.exe`: this machine runs world sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub world_host: Option<PathBuf>,
    /// Agent forks: pinned `claude`/`codex`/Git Bash and the slot count
    /// (docs/AGENT-FORKS.md). Absent: no agent runs on this machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<crate::forks::harness::AgentTools>,
    /// `enginebox-editor`: run Unity and its licensing client as that
    /// low-privilege account (Windows; qualify first with
    /// `qualify-editor-account.ps1`). Absent: Unity runs as the node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor_account: Option<String>,
}

fn valid_version(v: &str) -> bool {
    let Some((numbers, suffix)) = v.split_once(|c: char| "abfpx".contains(c)) else {
        return false;
    };
    let parts: Vec<&str> = numbers.split('.').collect();
    v.len() <= 24
        && parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()))
        && !suffix.is_empty()
        && suffix.len() <= 4
        && suffix.bytes().all(|b| b.is_ascii_digit())
}
fn file_named(path: &Path, names: &[&str]) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| names.iter().any(|x| n.eq_ignore_ascii_case(x)))
}
/// Unity's bundled licensing client for an editor.
pub fn licensing_client(unity: &Path) -> PathBuf {
    let editor = unity.parent().unwrap_or(Path::new(""));
    editor
        .join("Data")
        .join("Resources")
        .join("Licensing")
        .join("Client")
        .join(if cfg!(windows) {
            "Unity.Licensing.Client.exe"
        } else {
            "Unity.Licensing.Client"
        })
}

impl EngineConfig {
    pub fn load(path: &Path) -> Result<Self> {
        reject_symlink_components(path)?;
        let bytes = std::fs::read(path)
            .with_context(|| format!("Engine configuration {} is unreadable", path.display()))?;
        ensure!(
            bytes.len() <= 64 * 1024,
            "Engine configuration is too large"
        );
        let config: Self =
            serde_json::from_slice(&bytes).context("Engine configuration has the wrong shape")?;
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema == 1, "engine.json schema must be 1");
        ensure!(
            (1..=8).contains(&self.unity.len()),
            "engine.json names 1-8 Unity versions"
        );
        let exe = |path: &Path, names: &[&str], what: &str| -> Result<()> {
            ensure!(
                path.is_absolute() && file_named(path, names) && path.is_file(),
                "{what} must be an absolute path to an existing {}",
                names[0]
            );
            reject_symlink_components(path)?;
            Ok(())
        };
        for (version, unity) in &self.unity {
            ensure!(valid_version(version), "Invalid Unity version {version}");
            exe(unity, &["Unity.exe", "Unity"], &format!("unity.{version}"))?;
            ensure!(
                licensing_client(unity).is_file(),
                "Unity {version} has no bundled licensing client at {}",
                licensing_client(unity).display()
            );
        }
        exe(
            &self.stream_host,
            &["enginebox-stream-host.exe", "enginebox-stream-host"],
            "stream_host",
        )?;
        exe(&self.ffmpeg, &["ffmpeg.exe", "ffmpeg"], "ffmpeg")?;
        if let Some(git) = &self.git {
            exe(git, &["git.exe", "git"], "git")?;
        }
        if let Some(host) = &self.world_host {
            exe(
                host,
                &["gamejam-world-host.exe", "gamejam-world-host"],
                "world_host",
            )?;
        }
        ensure!(
            self.editor_bridge.is_absolute() && self.editor_bridge.is_dir(),
            "editor_bridge must be an absolute path to the unity-editor-bridge package"
        );
        let manifest: Value = serde_json::from_slice(
            &std::fs::read(self.editor_bridge.join("package.json"))
                .context("editor_bridge has no package.json")?,
        )?;
        ensure!(
            manifest["name"] == BRIDGE_PACKAGE,
            "editor_bridge is not the {BRIDGE_PACKAGE} package"
        );
        ensure!(
            self.startup_timeout_seconds
                .is_none_or(|t| (60..=4 * 3600).contains(&t)),
            "startup_timeout_seconds must be 60-14400"
        );
        ensure!(
            self.unity_region.as_deref().is_none_or(|r| r == "global"),
            "unity_region must be global"
        );
        if let Some(agents) = &self.agents {
            agents.validate()?;
        }
        ensure!(
            self.editor_account
                .as_deref()
                .is_none_or(|a| a == crate::forks::slots::EDITOR_ACCOUNT),
            "editor_account must be {}",
            crate::forks::slots::EDITOR_ACCOUNT
        );
        ensure!(
            self.editor_account.is_none() || cfg!(windows),
            "editor_account is supported on Windows only"
        );
        Ok(())
    }
    pub fn git(&self) -> Result<PathBuf> {
        match &self.git {
            Some(git) => Ok(git.clone()),
            None => crate::contributor::find_executable("git")
                .context("Git for Windows is not installed for the engine account"),
        }
    }
}

/// `tools.engine` for the heartbeat: whether sessions can run and which
/// editor versions are installed.
pub fn inventory(config_path: &Path, running: usize) -> Value {
    match EngineConfig::load(config_path) {
        Ok(c) => {
            json!({"ready": true, "unity": c.unity.keys().collect::<Vec<_>>(), "running": running,
                "world": c.world_host.is_some()})
        }
        Err(e) => {
            let reason: String = format!("{e:#}").chars().take(300).collect();
            json!({"ready": false, "unity": [], "running": running, "world": false, "error": reason})
        }
    }
}

// ---------------------------------------------------------------- the wire

#[derive(Clone, Debug, Deserialize)]
pub struct Spec {
    pub repository: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub project_path: String,
    pub engine: EngineSpec,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub scene: Option<String>,
    pub namespace: String,
    pub session: String,
    pub tag: u64,
    /// `editor` (the default, also for controllers that do not send it) or
    /// `world`.
    #[serde(default)]
    pub mode: Option<String>,
    /// A world session's room.
    #[serde(default)]
    pub world: Option<WorldSpec>,
}
#[derive(Clone, Debug, Deserialize)]
pub struct WorldSpec {
    pub room_origin: String,
}
#[derive(Clone, Debug, Deserialize)]
pub struct EngineSpec {
    pub kind: String,
    pub version: String,
}
#[derive(Clone, Deserialize)]
pub struct Secrets {
    pub repository: RepositorySecret,
    pub unity: UnitySecret,
    /// The relay publish token (editor sessions).
    #[serde(default)]
    pub moq: Option<MoqSecret>,
    /// The WebRTC fallback's host token, when the controller offers it.
    #[serde(default)]
    pub webrtc: Option<WebrtcSecret>,
    /// The room's one-time enrollment code (world sessions).
    #[serde(default)]
    pub world: Option<WorldSecret>,
}
#[derive(Clone, Deserialize)]
pub struct WorldSecret {
    pub enroll_code: String,
}
impl Secrets {
    /// Every secret value, for redaction.
    fn values(&self) -> Vec<&str> {
        let mut out = vec![
            self.repository.token.as_str(),
            self.unity.access_token.as_str(),
        ];
        if let Some(moq) = &self.moq {
            out.extend([moq.publish_token.as_str(), moq.control_key.as_str()]);
        }
        if let Some(w) = &self.webrtc {
            out.push(&w.host_token);
        }
        if let Some(w) = &self.world {
            out.push(&w.enroll_code);
        }
        out
    }
}
#[derive(Clone, Deserialize)]
pub struct WebrtcSecret {
    pub host_token: String,
}
#[derive(Clone, Deserialize)]
pub struct RepositorySecret {
    pub remote: String,
    pub token: String,
    pub auth: String,
}
#[derive(Clone, Deserialize)]
pub struct UnitySecret {
    pub access_token: String,
}
#[derive(Clone, Deserialize)]
pub struct MoqSecret {
    pub publish_token: String,
    pub control_key: String,
}
impl Spec {
    /// Whether this is a world session (`mode: "world"`).
    pub fn is_world(&self) -> bool {
        self.mode.as_deref() == Some("world")
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            matches!(self.mode.as_deref(), None | Some("editor" | "world")),
            "Unsupported session mode"
        );
        match (&self.world, self.is_world()) {
            (Some(world), true) => ensure!(
                https_origin(&world.room_origin),
                "The world session's room is not an https origin"
            ),
            (None, false) => {}
            _ => bail!("A world session names its room, and only a world session does"),
        }
        ensure!(
            self.engine.kind == "unity" && valid_version(&self.engine.version),
            "Unsupported engine"
        );
        ensure!(
            (160..=1920).contains(&self.width) && (90..=1080).contains(&self.height),
            "Invalid stream size"
        );
        ensure!(
            !self.git_ref.starts_with('-')
                && !self.git_ref.is_empty()
                && self.git_ref.len() <= 128
                && self
                    .git_ref
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_./-".contains(&b))
                && !self.git_ref.contains(".."),
            "Invalid ref"
        );
        ensure!(
            self.project_path.len() <= 200
                && !self.project_path.starts_with('/')
                && !self.project_path.contains('\\')
                && !self.project_path.contains(':')
                && self.project_path.split('/').all(|c| c != ".."
                    && c.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b" _.-".contains(&b))),
            "Invalid project path"
        );
        ensure!(
            (1..=80).contains(&self.namespace.len())
                && self
                    .namespace
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "Invalid namespace"
        );
        ensure!(
            uuid::Uuid::parse_str(&self.session)
                .is_ok_and(|u| u.hyphenated().to_string() == self.session),
            "Invalid session UUID"
        );
        if let Some(scene) = &self.scene {
            ensure!(
                scene.starts_with("Assets/") && scene.ends_with(".unity") && !scene.contains(".."),
                "Invalid scene"
            );
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- plans (pure)

/// `https://host[:port]` exactly, with nothing after it.
fn https_origin(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("https://") else {
        return false;
    };
    let host = rest.rsplit_once(':').map_or(rest, |(h, port)| {
        if !port.is_empty() && port.len() <= 5 && port.bytes().all(|b| b.is_ascii_digit()) {
            h
        } else {
            ""
        }
    });
    s.len() <= 300
        && !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
}

/// The world host's owner-only session file (`--session`; Gamejam's
/// `packages/world-host` reads it with unknown fields denied).
#[allow(clippy::too_many_arguments)]
pub fn world_session(
    world: &WorldSpec,
    secret: &WorldSecret,
    id: &str,
    generation: u64,
    unity: &Path,
    project: &Path,
    work_dir: &Path,
) -> Value {
    json!({
        "schema": 1,
        "room_origin": world.room_origin,
        "enroll_code": secret.enroll_code,
        "engine_session": id,
        "generation": generation,
        "unity_exe": unity,
        "project_dir": project,
        "work_dir": work_dir,
    })
}
/// The world host's argv: the session file only, never a secret.
pub fn world_host_argv(session_file: &Path) -> Vec<OsString> {
    vec!["--session".into(), session_file.as_os_str().to_owned()]
}
/// Whether a world host stdout line is its readiness event (its first
/// `HostWelcome` from the room).
pub fn world_host_ready(line: &str) -> bool {
    serde_json::from_str::<Value>(line.trim()).is_ok_and(|v| {
        v.as_object()
            .is_some_and(|o| o.get("event").and_then(Value::as_str) == Some(WORLD_READY_EVENT))
    })
}
/// The persistent world build cache.
pub fn world_builds(work: &Path) -> PathBuf {
    work.join("engine").join(WORLD_BUILDS)
}
/// Lend the persistent build cache to a world session's work directory (a
/// rename; a fresh cache when there is none).
pub fn lend_world_builds(work: &Path, work_dir: &Path) -> Result<()> {
    let shared = world_builds(work);
    let lent = work_dir.join(WORLD_BUILDS);
    if shared.is_dir() && !lent.exists() {
        std::fs::rename(&shared, &lent)
            .context("Cannot lend the world build cache to the session")?;
    }
    private_dir(&lent)
}
/// Move a session's lent build cache back (after its processes ended, or in
/// the startup sweep). Returns whether it moved.
pub fn return_world_builds(work: &Path, session_dir: &Path) -> bool {
    let lent = session_dir.join("world").join(WORLD_BUILDS);
    let shared = world_builds(work);
    lent.is_dir()
        && !shared.exists()
        && private_dir(&work.join("engine")).is_ok()
        && std::fs::rename(&lent, &shared).is_ok()
}

/// The environment that gives git the repository token: an `extraHeader`
/// scoped to the remote, through `GIT_CONFIG_*` (never argv), and no
/// credential helper or prompt.
pub fn git_env(access: &RepositorySecret) -> Vec<(String, String)> {
    let header = match access.auth.as_str() {
        "bearer" => format!("Authorization: Bearer {}", access.token),
        _ => format!(
            "Authorization: Basic {}",
            base64::engine::general_purpose::STANDARD
                .encode(format!("x-access-token:{}", access.token))
        ),
    };
    vec![
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        ("GCM_INTERACTIVE".into(), "never".into()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ("GIT_CONFIG_COUNT".into(), "3".into()),
        ("GIT_CONFIG_KEY_0".into(), "credential.helper".into()),
        ("GIT_CONFIG_VALUE_0".into(), String::new()),
        (
            "GIT_CONFIG_KEY_1".into(),
            format!("http.{}.extraHeader", access.remote),
        ),
        ("GIT_CONFIG_VALUE_1".into(), header),
        ("GIT_CONFIG_KEY_2".into(), "core.longpaths".into()),
        ("GIT_CONFIG_VALUE_2".into(), "true".into()),
    ]
}
/// The git commands for one checkout, in order. None carries a secret.
/// The repository lives in the node-private `git_dir`; the work tree (the
/// dev tree the Editor opens) contains no `.git`.
pub fn git_plan(
    git_dir: &Path,
    work_tree: &Path,
    remote: &str,
    git_ref: &str,
    fresh: bool,
) -> Vec<Vec<OsString>> {
    let c = |args: &[&str]| -> Vec<OsString> {
        let mut gd = OsString::from("--git-dir=");
        gd.push(git_dir);
        let mut wt = OsString::from("--work-tree=");
        wt.push(work_tree);
        let mut v: Vec<OsString> = vec![gd, wt];
        v.extend(args.iter().map(OsString::from));
        v
    };
    let mut plan = Vec::new();
    if fresh {
        plan.push(c(&["init", "-q"]));
        plan.push(c(&["remote", "add", "origin", remote]));
    } else {
        plan.push(c(&["remote", "set-url", "origin", remote]));
    }
    plan.push(c(&["lfs", "install", "--local", "--skip-smudge"]));
    plan.push(c(&["fetch", "--no-tags", "--depth=1", "origin", git_ref]));
    plan.push(c(&["checkout", "--detach", "-f", "FETCH_HEAD"]));
    // Untracked files go; ignored ones (Unity's Library cache) stay.
    plan.push(c(&["clean", "-ffdq"]));
    plan.push(c(&["lfs", "pull", "origin"]));
    plan
}
pub fn licensing_argv(token: &str) -> Vec<String> {
    vec![
        "--activate-all".into(),
        "--accessToken".into(),
        token.into(),
        "--include-personal".into(),
    ]
}
/// Unity's licensing client writes its command line, access token included, to
/// these logs. The node removes the token once activation has finished.
pub fn licensing_log_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Unity"))
    } else {
        std::env::var_os("HOME").map(|d| PathBuf::from(d).join(".config/unity3d/Unity"))
    }
}
/// Replace every copy of `token` in the licensing client's logs under `dir`;
/// returns how many files changed.
pub fn scrub_licensing_logs(dir: &Path, token: &str) -> Result<usize> {
    if token.len() < 16 || !dir.is_dir() {
        return Ok(0);
    }
    let needle = token.as_bytes();
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
        if !text.windows(needle.len()).any(|w| w == needle) {
            continue;
        }
        let mut out = Vec::with_capacity(text.len());
        let mut rest = &text[..];
        while let Some(at) = rest.windows(needle.len()).position(|w| w == needle) {
            out.extend_from_slice(&rest[..at]);
            out.extend_from_slice(b"<redacted>");
            rest = &rest[at + needle.len()..];
        }
        out.extend_from_slice(rest);
        std::fs::write(&path, out)?;
        changed += 1;
    }
    Ok(changed)
}
pub fn unity_argv(project: &Path, bridge_config: &Path, log: &Path) -> Vec<OsString> {
    vec![
        // Newer Editors honour this for unattended opens, as Unity's CLI passes
        // it; 6000.6 does not, so accept_unity_terms also records the acceptance.
        "-acceptSoftwareTermsForThisRunOnly".into(),
        "-projectPath".into(),
        project.as_os_str().to_owned(),
        "-jam-editor-config".into(),
        bridge_config.as_os_str().to_owned(),
        "-logFile".into(),
        log.as_os_str().to_owned(),
    ]
}
pub fn bridge_config(port: u16, token: &str, spec: &Spec) -> Value {
    json!({"port": port, "token": token, "scene": spec.scene.clone().unwrap_or_default(),
        "width": spec.width, "height": spec.height, "playOnly": false})
}
/// The stream host's private MoQ session file (`--moq-session`).
pub fn stream_session(
    spec: &Spec,
    moq: &MoqSecret,
    port: u16,
    token_file: &Path,
    ffmpeg: &Path,
) -> Value {
    json!({
        "schema": 1, "token": moq.publish_token, "controlKey": moq.control_key,
        "session": spec.session, "namespace": spec.namespace, "tag": spec.tag,
        "durationSeconds": STREAM_SECONDS, "width": spec.width, "height": spec.height,
        "capture": {"kind": "unity-editor", "origin": format!("http://127.0.0.1:{port}"),
            "tokenFile": token_file, "ffmpeg": ffmpeg, "editor": true},
    })
}
/// The stream host's WebRTC fallback block (`moq-session.json` `webrtc`):
/// it polls the controller for viewers' peers with the session's host token.
/// The file is owner-only, like the publish token beside it.
pub fn webrtc_block(
    controller: &str,
    secret: &WebrtcSecret,
    spec: &Spec,
    id: &str,
    generation: u64,
) -> Option<Value> {
    let project = spec
        .repository
        .split('/')
        .next()
        .filter(|p| !p.is_empty())?;
    let token = &secret.host_token;
    let valid = token.len() == 68
        && token.starts_with("ehp_")
        && token[4..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    valid.then(|| {
        json!({"controller": controller, "token": token, "project": project,
            "engineSession": id, "generation": generation})
    })
}
fn file_url(path: &Path) -> String {
    format!("file:{}", path.to_string_lossy().replace('\\', "/"))
}
/// Add the editor bridge and the session package to a project manifest.
pub fn add_packages(manifest: &Path, bridge: &Path, session_package: &Path) -> Result<()> {
    let mut value: Value = serde_json::from_slice(&std::fs::read(manifest)?)
        .context("Packages/manifest.json is not JSON")?;
    let dependencies = value["dependencies"]
        .as_object_mut()
        .context("Packages/manifest.json has no dependencies")?;
    dependencies.insert(BRIDGE_PACKAGE.into(), json!(file_url(bridge)));
    dependencies.insert(SESSION_PACKAGE.into(), json!(file_url(session_package)));
    std::fs::write(manifest, serde_json::to_vec_pretty(&value)?)?;
    Ok(())
}
const AUTOPLAY: &str = r#"// Engine Box session package: enters Play mode once the Editor has
// imported and compiled, so the Unity Jam editor bridge streams the Game view
// (play-only). Active only with -enginebox-autoplay.
using System;
using UnityEditor;

namespace Enginebox.Session
{
    [InitializeOnLoad]
    static class AutoPlay
    {
        static double since;
        static AutoPlay()
        {
            if (Array.IndexOf(Environment.GetCommandLineArgs(), "-enginebox-autoplay") < 0) return;
            if (SessionState.GetBool("Enginebox.AutoPlayed", false)) return;
            since = EditorApplication.timeSinceStartup;
            EditorApplication.update += Tick;
        }
        static void Tick()
        {
            if (EditorApplication.isPlaying || EditorApplication.isPlayingOrWillChangePlaymode) { EditorApplication.update -= Tick; return; }
            if (EditorApplication.isCompiling || EditorApplication.isUpdating) { since = EditorApplication.timeSinceStartup; return; }
            if (EditorApplication.timeSinceStartup - since < 5) return;
            EditorApplication.update -= Tick;
            SessionState.SetBool("Enginebox.AutoPlayed", true);
            EditorApplication.EnterPlaymode();
        }
    }
}
"#;
pub fn write_session_package(dir: &Path) -> Result<()> {
    let editor = dir.join("Editor");
    std::fs::create_dir_all(&editor)?;
    std::fs::write(
        dir.join("package.json"),
        serde_json::to_vec_pretty(&json!({"name": SESSION_PACKAGE, "version": "1.0.0",
            "displayName": "Engine Box session", "unity": "6000.0"}))?,
    )?;
    std::fs::write(
        editor.join("Enginebox.Session.Editor.asmdef"),
        serde_json::to_vec_pretty(&json!({"name": "Enginebox.Session.Editor",
            "includePlatforms": ["Editor"], "autoReferenced": false}))?,
    )?;
    std::fs::write(editor.join("AutoPlay.cs"), AUTOPLAY)?;
    Ok(())
}

/// Copy the configured editor bridge into the node's own cache and return the
/// copy. The configured package can be read-only for the engine account, and
/// Unity writes `.meta` files into a `file:` package; a missing one in a
/// read-only package keeps the Editor retrying forever at "Opening project".
/// The copy is keyed by the source's content, so an updated bridge is copied
/// again and older copies are removed.
pub fn stage_bridge(source: &Path, cache: &Path) -> Result<PathBuf> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) -> Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<std::io::Result<_>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let kind = entry.file_type()?;
            let path = entry.path();
            let name = entry.file_name();
            if kind.is_symlink() || name == ".git" || name == "node_modules" {
                continue;
            }
            if kind.is_dir() {
                walk(root, &path, out)?;
            } else if kind.is_file() {
                ensure!(
                    out.len() < 2000,
                    "The editor bridge package has too many files"
                );
                out.push((path.strip_prefix(root)?.to_owned(), std::fs::read(&path)?));
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    walk(source, source, &mut files)?;
    let mut digest = Sha256::new();
    for (path, bytes) in &files {
        digest.update(path.to_string_lossy().replace('\\', "/").as_bytes());
        digest.update([0]);
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(bytes);
    }
    let digest = hex::encode(digest.finalize());
    let name = format!("editor-bridge-{}", &digest[..16]);
    private_dir(cache)?;
    let staged = cache.join(&name);
    if staged.join(".enginebox-staged").is_file() {
        return Ok(staged);
    }
    let pending = cache.join(format!(".{name}-{}", random_hex(4)));
    for (path, bytes) in &files {
        let target = pending.join(path);
        std::fs::create_dir_all(target.parent().context("bridge file has no parent")?)?;
        std::fs::write(&target, bytes)?;
    }
    std::fs::write(pending.join(".enginebox-staged"), &digest)?;
    if staged.exists() {
        std::fs::remove_dir_all(&staged)?;
    }
    std::fs::rename(&pending, &staged)?;
    for entry in std::fs::read_dir(cache)?.flatten() {
        let other = entry.file_name().to_string_lossy().to_string();
        if other != name && other.starts_with("editor-bridge-") {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
    Ok(staged)
}

/// Remove the project's `Temp` folder before the Editor opens it. A session's
/// Editor is always stopped by the node, which leaves crash scene backups there
/// ("Scene Backup Detected") and a stale lock file; imports live in `Library`.
pub fn clear_unity_temp(project: &Path) -> Result<()> {
    let temp = project.join("Temp");
    reject_symlink_components(&temp)?;
    match std::fs::remove_dir_all(&temp) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("Cannot clear {}", temp.display()))
        }
        _ => Ok(()),
    }
}

/// Dialogs that stop the Editor until someone answers them. Nobody is at a
/// session's locked console, so the node fails the session with the title
/// rather than waiting out the startup limit.
pub const BLOCKING_DIALOGS: &[&str] = &[
    "Scene Backup Detected",
    "Unity Editor Software Terms",
    "Enter Safe Mode?",
    "Project Upgrade Required",
    "Script Updating Consent",
    "Opening Project in Non-Matching Editor Installation",
    "Unity Package Manager Error",
];
pub fn blocking_dialog(titles: &[String]) -> Option<String> {
    titles
        .iter()
        .find(|t| BLOCKING_DIALOGS.iter().any(|d| t.trim() == *d))
        .cloned()
}
/// Titles of the visible top-level windows of process `pid`.
#[cfg(windows)]
pub fn window_titles(pid: u32) -> Vec<String> {
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    };
    struct Search {
        pid: u32,
        titles: Vec<String>,
    }
    unsafe extern "system" fn visit(hwnd: HWND, data: LPARAM) -> i32 {
        // SAFETY: `data` is the `Search` passed to EnumWindows below, alive for the call.
        let search = unsafe { &mut *(data as *mut Search) };
        let mut owner = 0u32;
        // SAFETY: plain Win32 queries on a window handle EnumWindows supplied.
        unsafe {
            GetWindowThreadProcessId(hwnd, &mut owner);
            if owner == search.pid && IsWindowVisible(hwnd) != 0 && search.titles.len() < 64 {
                let mut buffer = [0u16; 256];
                let n = GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32);
                if n > 0 {
                    search
                        .titles
                        .push(String::from_utf16_lossy(&buffer[..n as usize]));
                }
            }
        }
        1
    }
    let mut search = Search {
        pid,
        titles: Vec::new(),
    };
    // SAFETY: the callback only touches `search`, which outlives the call.
    unsafe { EnumWindows(Some(visit), &mut search as *mut Search as LPARAM) };
    search.titles
}
#[cfg(not(windows))]
pub fn window_titles(_pid: u32) -> Vec<String> {
    Vec::new()
}

/// The EditorPrefs registry/plist name Unity stores for `key`: the key plus
/// `_h` and its 32-bit djb2-xor hash.
pub fn editor_prefs_name(key: &str) -> String {
    let hash = key
        .bytes()
        .fold(5381u32, |h, b| h.wrapping_mul(33) ^ u32::from(b));
    format!("{key}_h{hash}")
}
/// The `Terms-<hash>` EditorPrefs key an Editor build checks before it shows
/// its one-time "Unity Editor Software Terms" dialog, read from its binaries.
pub fn unity_terms_key(unity: &Path) -> Option<String> {
    let editor = unity.parent()?;
    let candidates = [editor.join("Unity.dll"), unity.to_owned()];
    candidates.iter().find_map(|file| {
        let bytes = std::fs::read(file).ok()?;
        bytes.windows(38).find_map(|w| {
            (w.starts_with(b"Terms-")
                && w[6..]
                    .iter()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b)))
            .then(|| String::from_utf8_lossy(w).into_owned())
        })
    })
}
/// Record the member's acceptance of the Editor terms for the engine account,
/// as the dialog's Accept does: nobody is at a session's locked console, and
/// the Editor would wait at the dialog before writing its log. The member
/// accepted Unity's terms with the Unity account the session uses.
async fn accept_unity_terms(unity: &Path) -> Result<Option<String>> {
    let Some(key) = unity_terms_key(unity) else {
        return Ok(None);
    };
    if !cfg!(windows) {
        return Ok(Some(key));
    }
    let name = editor_prefs_name(&key);
    let status = Command::new("reg.exe")
        .args([
            "add",
            r"HKCU\Software\Unity Technologies\Unity Editor 5.x",
            "/v",
            &name,
            "/t",
            "REG_DWORD",
            "/d",
            "1",
            "/f",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .context("Cannot run reg.exe")?;
    ensure!(
        status.success(),
        "Could not record Unity's Editor terms for the engine account"
    );
    Ok(Some(key))
}

/// Unity's global services configuration, as its config CDN serves it outside
/// China (`public-cdn.cloud.unity3d.com/config/production`).
const UNITY_GLOBAL_CONFIG: &str = include_str!("unity-global-config.json");
fn unity_config_file() -> Option<PathBuf> {
    licensing_log_dir().map(|dir| dir.join("config").join("production.json"))
}
/// With `unity_region: global`, keep the licensing client's cached services
/// configuration on the global region. The file is left read-only so the
/// client cannot replace it with the regional copy it downloads.
pub fn pin_unity_region(file: &Path) -> Result<bool> {
    let global = |bytes: &[u8]| {
        serde_json::from_slice::<Value>(bytes)
            .is_ok_and(|v| v["license"] == "https://license.unity3d.com")
    };
    if std::fs::read(file).is_ok_and(|b| global(&b)) {
        let mut permissions = std::fs::metadata(file)?.permissions();
        if !permissions.readonly() {
            permissions.set_readonly(true);
            std::fs::set_permissions(file, permissions)?;
        }
        return Ok(false);
    }
    std::fs::create_dir_all(file.parent().context("config file has no parent")?)?;
    if let Ok(meta) = std::fs::metadata(file) {
        let mut permissions = meta.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        std::fs::set_permissions(file, permissions)?;
    }
    std::fs::write(file, UNITY_GLOBAL_CONFIG)
        .context("Cannot pin Unity's services configuration to the global region")?;
    let mut permissions = std::fs::metadata(file)?.permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(file, permissions)?;
    Ok(true)
}
fn random_hex(bytes: usize) -> String {
    let mut out = String::new();
    while out.len() < bytes * 2 {
        out.push_str(&uuid::Uuid::new_v4().simple().to_string());
    }
    out.truncate(bytes * 2);
    out
}
fn redact(text: &str, secrets: &[&str]) -> String {
    let mut text = text.to_owned();
    for s in secrets.iter().filter(|s| s.len() >= 8) {
        text = text.replace(s, "[redacted]");
    }
    text
}
pub fn checkout_key(remote: &str) -> String {
    hex::encode(&Sha256::digest(remote.as_bytes())[..8])
}

// ---------------------------------------------------------------- processes

/// A session's processes: a Job Object on Windows (terminating it ends every
/// descendant), and the spawned children elsewhere.
pub struct Group {
    #[cfg(windows)]
    job: std::os::windows::io::OwnedHandle,
    children: Vec<Child>,
}
impl Group {
    pub fn new() -> Result<Self> {
        #[cfg(windows)]
        {
            use std::os::windows::io::FromRawHandle;
            use windows_sys::Win32::System::JobObjects::*;
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                ensure!(!job.is_null(), "Cannot create the session's Job Object");
                let job = std::os::windows::io::OwnedHandle::from_raw_handle(job);
                let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                use std::os::windows::io::AsRawHandle;
                ensure!(
                    SetInformationJobObject(
                        job.as_raw_handle(),
                        JobObjectExtendedLimitInformation,
                        (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                        std::mem::size_of_val(&limits) as u32,
                    ) != 0,
                    "Cannot configure the session's Job Object"
                );
                Ok(Self {
                    job,
                    children: Vec::new(),
                })
            }
        }
        #[cfg(not(windows))]
        Ok(Self {
            children: Vec::new(),
        })
    }
    /// Hold `child` (and its future descendants) in this session.
    pub fn adopt(&mut self, child: Child, record: &PidRecord) -> Result<usize> {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
            let handle = child
                .raw_handle()
                .context("Process exited before adoption")?;
            if unsafe { AssignProcessToJobObject(self.job.as_raw_handle(), handle) } == 0 {
                let mut child = child;
                let _ = child.start_kill();
                bail!("Cannot hold the process in the session's Job Object");
            }
        }
        if let Some(pid) = child.id() {
            record.add(pid)?;
        }
        self.children.push(child);
        Ok(self.children.len() - 1)
    }
    /// The session's Job Object (processes started as another account join it).
    #[cfg(windows)]
    pub fn job_handle(&self) -> windows_sys::Win32::Foundation::HANDLE {
        use std::os::windows::io::AsRawHandle;
        self.job.as_raw_handle() as _
    }
    pub fn child(&mut self, index: usize) -> &mut Child {
        &mut self.children[index]
    }
    pub async fn terminate(&mut self) {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::JobObjects::TerminateJobObject;
            unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) };
        }
        for child in &mut self.children {
            let _ = child.start_kill();
        }
        for child in &mut self.children {
            let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
        }
    }
}

/// Process ids a node started, so a later node process can sweep them.
pub struct PidRecord {
    path: PathBuf,
}
impl PidRecord {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
    fn read(&self) -> Vec<Value> {
        std::fs::read(&self.path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
    }
    pub fn add(&self, pid: u32) -> Result<()> {
        let mut rows = self.read();
        rows.retain(|r| r["pid"] != pid);
        rows.push(json!({"pid": pid, "created": process_created(pid)}));
        rows.truncate(256);
        atomic_json(&self.path, &rows)
    }
    pub fn take(&self) -> Vec<(u32, Option<u64>)> {
        let rows = self.read();
        let _ = std::fs::remove_file(&self.path);
        rows.iter()
            .filter_map(|r| Some((r["pid"].as_u64()? as u32, r["created"].as_u64())))
            .collect()
    }
}
#[cfg(windows)]
fn process_created(pid: u32) -> Option<u64> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, FILETIME},
        System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut times: [FILETIME; 4] = std::mem::zeroed();
        let [created, exited, kernel, user] = &mut times;
        let ok = GetProcessTimes(h, created, exited, kernel, user) != 0;
        CloseHandle(h);
        ok.then(|| (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }
}
#[cfg(not(windows))]
fn process_created(_pid: u32) -> Option<u64> {
    None
}
/// Terminate a recorded process if it is still the same process.
#[cfg(windows)]
fn kill_recorded(pid: u32, created: Option<u64>) -> bool {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess},
    };
    if created.is_none() || process_created(pid) != created {
        return false;
    }
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if h.is_null() {
            return false;
        }
        let killed = TerminateProcess(h, 1) != 0;
        CloseHandle(h);
        killed
    }
}
#[cfg(not(windows))]
fn kill_recorded(_pid: u32, _created: Option<u64>) -> bool {
    false
}

/// Startup sweep: kill processes an earlier node process started for engine
/// sessions, and remove every session directory (bridge tokens, stream
/// session files). Checkouts and their Library caches stay.
pub fn sweep(work: &Path, private: &Path) -> Result<Value> {
    let killed = PidRecord::new(private.join("engine-pids.json"))
        .take()
        .into_iter()
        .filter(|(pid, created)| kill_recorded(*pid, *created))
        .count();
    let sessions = work.join("engine").join("sessions");
    let mut removed = 0;
    if sessions.is_dir() {
        for entry in std::fs::read_dir(&sessions)? {
            let path = entry?.path();
            // A world session's lent build cache survives the sweep.
            return_world_builds(work, &path);
            if std::fs::remove_dir_all(&path).is_ok() {
                removed += 1;
            }
        }
    }
    // Agent forks and editor-account directories of earlier node processes
    // (their snapshots stay as refs in the node-private repositories).
    let mut removed_forks = 0;
    for dir in ["forks", "editor"] {
        let root = work.join("engine").join(dir);
        if root.is_dir() {
            for entry in std::fs::read_dir(&root)? {
                #[cfg(windows)]
                let _ = crate::forks::slots::revoke_dirs(&[&entry
                    .as_ref()
                    .map(|e| e.path())
                    .unwrap_or_default()]);
                if crate::forks::git::remove_tree(&entry?.path()).is_ok() {
                    removed_forks += 1;
                }
            }
        }
    }
    Ok(json!({"killed": killed, "removed_sessions": removed, "removed_forks": removed_forks}))
}

// ---------------------------------------------------------------- running a session

/// A state change a session task reports to the controller.
#[derive(Debug, Clone)]
pub struct Report {
    pub id: String,
    pub generation: u64,
    pub state: &'static str,
    pub error: Option<String>,
    pub detail: Value,
}

pub struct SessionContext {
    pub config: EngineConfig,
    pub work: PathBuf,
    pub private: PathBuf,
    /// The controller's public origin, for the stream host's WebRTC fallback
    /// signaling (None: the stream host offers MoQ only).
    pub controller: Option<String>,
    /// Agent forks of the session's dev tree (None: no forks).
    pub forks: Option<crate::forks::executor::ForkExecutor>,
    /// The installer's sealed accounts (`enginebox-editor`, slots).
    pub accounts: Option<std::sync::Arc<crate::forks::slots::Accounts>>,
}

async fn cancelled(stop: &mut watch::Receiver<bool>) {
    while !*stop.borrow() {
        if stop.changed().await.is_err() {
            return;
        }
    }
}

async fn run_git(
    git: &Path,
    args: &[OsString],
    env: &[(String, String)],
    secret: &str,
    stop: &mut watch::Receiver<bool>,
) -> Result<String> {
    let mut command = Command::new(git);
    command
        .args(args)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().context("Cannot start git")?;
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let read = async {
        let (mut o, mut e) = (Vec::new(), Vec::new());
        let (mut out_limited, mut err_limited) =
            ((&mut out).take(1 << 20), (&mut err).take(1 << 20));
        let _ = tokio::join!(
            out_limited.read_to_end(&mut o),
            err_limited.read_to_end(&mut e)
        );
        (o, e)
    };
    let outcome = tokio::select! {
        _ = cancelled(stop) => bail!("Stopped"),
        r = tokio::time::timeout(Duration::from_secs(GIT_SECONDS), async {
            let (o, e) = read.await;
            (child.wait().await, o, e)
        }) => r.context("git timed out")?,
    };
    let (status, out, err) = outcome;
    let status = status?;
    if !status.success() {
        let stderr = redact(&String::from_utf8_lossy(&err), &[secret]);
        let last: String = stderr
            .lines()
            .rfind(|l| !l.trim().is_empty())
            .unwrap_or("")
            .chars()
            .take(300)
            .collect();
        bail!(
            "git {} failed: {last}",
            args.get(2)
                .map(|a| a.to_string_lossy().to_string())
                .unwrap_or_default()
        );
    }
    Ok(String::from_utf8_lossy(&out).trim().to_owned())
}

/// Check out the exact ref with LFS into the repository's checkout. Returns
/// (checkout, commit). The repository is node-private
/// (`work\engine\repos\<key>.git`, index beside it); a checkout from an
/// older node with `.git` inside is moved there once.
pub async fn checkout(
    ctx: &SessionContext,
    spec: &Spec,
    access: &RepositorySecret,
    stop: &mut watch::Receiver<bool>,
) -> Result<(PathBuf, String)> {
    let git = ctx.config.git()?;
    let checkouts = ctx.work.join("engine").join("checkouts");
    let repos = ctx.work.join("engine").join("repos");
    private_dir(&checkouts)?;
    private_dir(&repos)?;
    let key = checkout_key(&access.remote);
    let (dir, git_dir, index) = crate::forks::executor::repo_paths(&ctx.work, &key);
    // Empty hooks and global config for every node git call.
    let node_git = repos.join(".node");
    crate::forks::git::GitTreeProvider::new(git.clone(), &node_git, true)?;
    let inner = dir.join(".git");
    if inner.is_dir() && !git_dir.exists() {
        std::fs::rename(&inner, &git_dir)?;
        if git_dir.join("index").is_file() {
            std::fs::rename(git_dir.join("index"), &index)?;
        }
    } else if inner.exists() {
        crate::forks::git::remove_tree(&inner)?;
    }
    let fresh = !git_dir.join("HEAD").is_file();
    if fresh && dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    if fresh && git_dir.exists() {
        std::fs::remove_dir_all(&git_dir)?;
    }
    std::fs::create_dir_all(&dir)?;
    let mut env = git_env(access);
    // The node never runs repository hooks or an fsmonitor.
    if let Some(count) = env.iter_mut().find(|(k, _)| k == "GIT_CONFIG_COUNT") {
        count.1 = "5".into();
    }
    env.extend([
        ("GIT_CONFIG_KEY_3".into(), "core.hooksPath".into()),
        (
            "GIT_CONFIG_VALUE_3".into(),
            node_git.join("hooks").to_string_lossy().into_owned(),
        ),
        ("GIT_CONFIG_KEY_4".into(), "core.fsmonitor".into()),
        ("GIT_CONFIG_VALUE_4".into(), "false".into()),
        (
            "GIT_CONFIG_GLOBAL".into(),
            node_git.join("gitconfig").to_string_lossy().into_owned(),
        ),
        (
            "GIT_INDEX_FILE".into(),
            index.to_string_lossy().into_owned(),
        ),
    ]);
    let mut commit = String::new();
    for args in git_plan(&git_dir, &dir, &access.remote, &spec.git_ref, fresh) {
        let mut env = env.clone();
        if args.iter().any(|a| a == "checkout") {
            env.push(("GIT_LFS_SKIP_SMUDGE".into(), "1".into()));
        }
        run_git(&git, &args, &env, &access.token, stop).await?;
        if args.iter().any(|a| a == "fetch") {
            let mut rev = args[..2].to_vec();
            rev.extend(["rev-parse".into(), "FETCH_HEAD".into()]);
            commit = run_git(&git, &rev, &env, &access.token, stop).await?;
        }
    }
    ensure!(
        commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit()),
        "Could not resolve the ref to a commit"
    );
    Ok((dir, commit))
}

/// The session's dev tree for agent forks.
pub fn dev_tree(
    ctx: &SessionContext,
    spec: &Spec,
    remote: &str,
    id: &str,
) -> crate::forks::DevTree {
    let key = checkout_key(remote);
    let (work_tree, git_dir, index) = crate::forks::executor::repo_paths(&ctx.work, &key);
    crate::forks::DevTree {
        session: id.to_owned(),
        git_dir,
        work_tree,
        index,
        project_path: spec.project_path.trim_matches('/').to_owned(),
        forks_root: ctx.work.join("engine").join("forks"),
    }
}
/// Prepare the project: version check, packages, bridge configuration.
/// Returns (project, bridge config path, bridge token file, port).
pub fn prepare_project(
    ctx: &SessionContext,
    spec: &Spec,
    checkout: &Path,
    session_dir: &Path,
) -> Result<(PathBuf, PathBuf, PathBuf, u16)> {
    let project = project_dir(spec, checkout)?;
    let package = session_dir.join("packages").join(SESSION_PACKAGE);
    write_session_package(&package)?;
    let bridge = stage_bridge(
        &ctx.config.editor_bridge,
        &ctx.work.join("engine").join("packages"),
    )?;
    add_packages(
        &project.join("Packages").join("manifest.json"),
        &bridge,
        &package,
    )?;
    let port = std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port();
    let token = random_hex(32);
    let config = session_dir.join("bridge.json");
    let token_file = session_dir.join("bridge.token");
    std::fs::write(
        &config,
        serde_json::to_vec(&bridge_config(port, &token, spec))?,
    )?;
    std::fs::write(&token_file, &token)?;
    Ok((project, config, token_file, port))
}

/// The session's Unity project in the checkout, checked against the
/// requested editor version.
pub fn project_dir(spec: &Spec, checkout: &Path) -> Result<PathBuf> {
    let project = if spec.project_path.trim_matches('/').is_empty() {
        checkout.to_owned()
    } else {
        checkout.join(spec.project_path.trim_matches('/'))
    };
    reject_symlink_components(&project)?;
    let version = std::fs::read_to_string(project.join("ProjectSettings/ProjectVersion.txt"))
        .context("project_path is not a Unity project (no ProjectSettings/ProjectVersion.txt)")?;
    let found = version
        .lines()
        .find_map(|l| l.strip_prefix("m_EditorVersion:"))
        .map(str::trim)
        .unwrap_or("");
    ensure!(
        found == spec.engine.version,
        "The project uses Unity {found}, but the session asked for {}",
        spec.engine.version
    );
    Ok(project)
}

/// Checks in a row (about two seconds apart) that something other than the
/// bridge must hold the bridge's port before the node moves it.
pub const BRIDGE_PORT_STRIKES: u32 = 3;
/// Port moves a session allows before it fails.
pub const BRIDGE_PORT_MOVES: u32 = 5;

/// A free loopback port, released for the bridge to bind.
fn free_port() -> Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port())
}
/// Whether something already listens on `127.0.0.1:port`.
fn port_held(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_err()
}

/// The Editor bridge's loopback port. The node picks a free port and writes
/// it into `bridge.json`, but another program can take it before Unity binds
/// it. The bridge then fails its start and retries on every Editor update,
/// reading `bridge.json` again each time. So when the port stays held by
/// something that does not answer as the bridge, the node writes a new free
/// port there and waits on that one instead. `port` is always the port the
/// bridge was last told, and it is the one the stream host is given once the
/// bridge answers on it.
pub struct BridgePort {
    pub port: u16,
    token: String,
    config: PathBuf,
    strikes: u32,
    pub moves: u32,
}
impl BridgePort {
    pub fn new(port: u16, token: &str, config: &Path) -> Self {
        Self {
            port,
            token: token.trim().to_owned(),
            config: config.to_owned(),
            strikes: 0,
            moves: 0,
        }
    }
    /// The bridge's status when it answers on the current port. Otherwise
    /// `None`, after moving the bridge to a new port if a foreign listener
    /// has held this one for [`BRIDGE_PORT_STRIKES`] checks.
    pub async fn check(&mut self) -> Result<Option<Value>> {
        if let Some(status) = bridge_status(self.port, &self.token).await
            && status["type"] == "unity-editor"
        {
            self.strikes = 0;
            return Ok(Some(status));
        }
        if !port_held(self.port) {
            // Free: the Editor is still importing or compiling.
            self.strikes = 0;
            return Ok(None);
        }
        self.strikes += 1;
        if self.strikes < BRIDGE_PORT_STRIKES {
            return Ok(None);
        }
        ensure!(
            self.moves < BRIDGE_PORT_MOVES,
            "The Unity editor bridge could not keep a local port: another program on the engine machine kept taking it"
        );
        let next = free_port()?;
        let mut config: Value = serde_json::from_slice(&std::fs::read(&self.config)?)?;
        config["port"] = json!(next);
        atomic_json(&self.config, &config)?;
        self.port = next;
        self.moves += 1;
        self.strikes = 0;
        Ok(None)
    }
    /// Wait (up to `limit`) until the bridge answers, moving its port if needed.
    pub async fn answering(
        &mut self,
        limit: Duration,
        stop: &mut watch::Receiver<bool>,
    ) -> Result<()> {
        let t = Instant::now();
        loop {
            if self.check().await?.is_some() {
                return Ok(());
            }
            ensure!(
                t.elapsed() < limit,
                "The Unity editor bridge stopped answering"
            );
            tokio::select! {
                _ = cancelled(stop) => bail!("Stopped"),
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
        }
    }
}

/// The bridge's `/status`, when it answers.
pub(crate) async fn bridge_status(port: u16, token: &str) -> Option<Value> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .ok()?;
    let response = client
        .get(format!("http://127.0.0.1:{port}/status"))
        .bearer_auth(token)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

/// The Editor process: a child of the node, or started as the editor account.
enum EditorProc {
    Node(usize),
    Account(crate::editor_account::EditorChild),
}
impl EditorProc {
    fn pid(&self, g: &mut Group) -> u32 {
        match self {
            Self::Node(i) => g.child(*i).id().unwrap_or(0),
            Self::Account(c) => crate::editor_account::child_pid(c),
        }
    }
    fn exited(&self, g: &mut Group) -> Result<Option<i64>> {
        Ok(match self {
            Self::Node(i) => g
                .child(*i)
                .try_wait()?
                .map(|s| i64::from(s.code().unwrap_or(-1))),
            Self::Account(c) => crate::editor_account::child_exit(c).map(i64::from),
        })
    }
}

/// Run one session until it is stopped or fails; reports go to `reports`.
pub async fn run_session(
    ctx: std::sync::Arc<SessionContext>,
    id: String,
    generation: u64,
    spec: Spec,
    secrets: Secrets,
    mut stop: watch::Receiver<bool>,
    reports: mpsc::UnboundedSender<Report>,
) {
    let started = Instant::now();
    let report = |state: &'static str, error: Option<String>, detail: Value| {
        let _ = reports.send(Report {
            id: id.clone(),
            generation,
            state,
            error,
            detail,
        });
    };
    report("preparing", None, json!({}));
    let session_dir = ctx.work.join("engine").join("sessions").join(&id);
    let mut group = None;
    let mut receipt = json!({"id": id, "generation": generation, "repository": spec.repository,
        "ref": spec.git_ref, "engine": spec.engine.version, "started_at": chrono::Utc::now().to_rfc3339()});
    let outcome: Result<()> = async {
        spec.validate()?;
        let unity = ctx
            .config
            .unity
            .get(&spec.engine.version)
            .with_context(|| format!("Unity {} is not installed on this engine machine", spec.engine.version))?
            .clone();
        private_dir(&session_dir)?;
        let t = Instant::now();
        let (checkout, commit) = checkout(&ctx, &spec, &secrets.repository, &mut stop).await?;
        receipt["commit"] = json!(commit);
        receipt["checkout_seconds"] = json!(t.elapsed().as_secs());
        // Unity as `enginebox-editor` (engine.json opt-in, Windows).
        let as_editor = crate::editor_account::resolve(&ctx)?;
        let bridge_dir = match &as_editor {
            Some(e) => e.prepare(&ctx.work, &id, &checkout)?,
            None => session_dir.clone(),
        };
        receipt["editor_account"] = json!(as_editor.is_some());
        report("preparing", None, json!({"commit": commit}));
        // A world session runs the world host, not the Editor: no packages,
        // bridge or relay.
        let editor = if spec.is_world() {
            ensure!(
                ctx.config.world_host.is_some(),
                "This engine machine is not set up for world sessions: engine.json names no world_host"
            );
            None
        } else {
            let moq = secrets.moq.as_ref().context("The session has no relay token; start a new one.")?;
            Some((prepare_project(&ctx, &spec, &checkout, &bridge_dir)?, moq))
        };
        let project = match &editor {
            Some(((project, ..), _)) => project.clone(),
            None => project_dir(&spec, &checkout)?,
        };
        let pids = PidRecord::new(ctx.private.join("engine-pids.json"));
        let mut g = Group::new()?;
        if as_editor.is_none() && ctx.config.unity_region.as_deref() == Some("global") {
            let file = unity_config_file().context("The engine account has no Unity config folder")?;
            receipt["unity_region_pinned"] = json!(pin_unity_region(&file)?);
        }
        if as_editor.is_none() {
            receipt["unity_terms_key"] = json!(accept_unity_terms(&unity).await?);
        }
        // Activation: the access token is the licensing client's only input.
        let t = Instant::now();
        if let Some(e) = &as_editor {
            // The editor's own licensing; its logs are scrubbed by the editor.
            group = Some(g);
            let code = e
                .run(&licensing_client(&unity), &licensing_argv(&secrets.unity.access_token), &project,
                    group.as_ref().unwrap(), Duration::from_secs(LICENSE_SECONDS), &mut stop)
                .await?;
            ensure!(!*stop.borrow(), "Stopped");
            receipt["licensing_logs_scrubbed"] = json!(e.scrub(group.as_ref().unwrap(), &mut stop).await?);
            ensure!(
                code == Some(0),
                "Unity could not activate a license with your Unity account (licensing client exit {}). Check the account on the Agents page.",
                code.map_or(-1, |c| c as i64)
            );
        } else {
            let mut activate = Command::new(licensing_client(&unity));
            activate
                .args(licensing_argv(&secrets.unity.access_token))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            let index = g.adopt(activate.spawn().context("Cannot start Unity's licensing client")?, &pids)?;
            group = Some(g);
            let status = tokio::select! {
                _ = cancelled(&mut stop) => bail!("Stopped"),
                s = tokio::time::timeout(Duration::from_secs(LICENSE_SECONDS), group.as_mut().unwrap().child(index).wait()) =>
                    s.context("Unity license activation timed out")??,
            };
            // Whether or not activation worked, the token leaves the licensing client's logs.
            receipt["licensing_logs_scrubbed"] =
                match licensing_log_dir().map(|dir| scrub_licensing_logs(&dir, &secrets.unity.access_token)) {
                    Some(Ok(files)) => json!(files),
                    Some(Err(error)) => json!(format!("failed: {error:#}")),
                    None => json!(0),
                };
            ensure!(
                status.success(),
                "Unity could not activate a license with your Unity account (licensing client exit {}). Check the account on the Agents page.",
                status.code().unwrap_or(-1)
            );
        }
        receipt["activation_seconds"] = json!(t.elapsed().as_secs());
        let Some(((_, bridge_json, token_file, port), moq)) = editor else {
            return run_world_host(
                &ctx, &spec, &secrets, &id, generation, &unity, &project, &session_dir,
                group.as_mut().unwrap(), &pids, &mut stop, &mut receipt, started, &report,
            )
            .await;
        };
        let mut bridge = BridgePort::new(port, &std::fs::read_to_string(&token_file)?, &bridge_json);

        // The Editor, in this node's interactive session.
        clear_unity_temp(&project)?;
        let log = bridge_dir.join("unity.log");
        let g = group.as_mut().unwrap();
        let editor_proc = match &as_editor {
            Some(e) => {
                let argv: Vec<String> = unity_argv(&project, &bridge_json, &log)
                    .iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect();
                let child = e.spawn(&unity, &argv, &project, g).context("Cannot start the Unity Editor as the editor account")?;
                pids.add(crate::editor_account::child_pid(&child))?;
                EditorProc::Account(child)
            }
            None => {
                let mut editor = Command::new(&unity);
                editor
                    .args(unity_argv(&project, &bridge_json, &log))
                    .current_dir(&project)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .kill_on_drop(true);
                EditorProc::Node(g.adopt(editor.spawn().context("Cannot start the Unity Editor")?, &pids)?)
            }
        };
        report("starting", None, json!({}));
        let t = Instant::now();
        let limit = Duration::from_secs(ctx.config.startup_timeout_seconds.unwrap_or(DEFAULT_STARTUP_SECONDS));
        let editor_pid = editor_proc.pid(g);
        let mut dialog: Option<(String, Instant)> = None;
        loop {
            // The full Editor (Scene and Game views, Play / Stop / Save from
            // the controlling client): ready once the bridge answers and the
            // first import and compile are done.
            if let Some(status) = bridge.check().await?
                && status["compiling"] != true
            {
                break;
            }
            if let Some(exit) = editor_proc.exited(g)? {
                bail!("The Unity Editor exited during startup (exit {exit}); see unity.log on the engine machine");
            }
            ensure!(t.elapsed() < limit, "The Unity Editor did not finish opening in time");
            match blocking_dialog(&window_titles(editor_pid)) {
                Some(title) => {
                    let since = match &dialog {
                        Some((seen, since)) if *seen == title => *since,
                        _ => Instant::now(),
                    };
                    ensure!(
                        since.elapsed() < Duration::from_secs(20),
                        "The Unity Editor is waiting at the \"{title}\" dialog; nobody can answer it in a session. Open the project once in Unity on this machine, or start a new session."
                    );
                    dialog = Some((title, since));
                }
                None => dialog = None,
            }
            tokio::select! {
                _ = cancelled(&mut stop) => bail!("Stopped"),
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
        }
        receipt["editor_ready_seconds"] = json!(t.elapsed().as_secs());
        receipt["bridge_port_moves"] = json!(bridge.moves);
        // Agent forks: the dev tree is ready once the Editor answers.
        if let Some(forks) = &ctx.forks {
            let registered: Result<()> = async {
                let tree = dev_tree(&ctx, &spec, &secrets.repository.remote, &id);
                let provider = std::sync::Arc::new(crate::forks::git::GitTreeProvider::new(
                    ctx.config.git()?,
                    &ctx.work.join("engine").join("repos").join(".node"),
                    true,
                )?);
                receipt["forks_git"] = json!(provider.check_version()?);
                let project = tree.project_path.clone();
                let rel = |p: &str| if project.is_empty() { p.to_owned() } else { format!("{project}/{p}") };
                provider
                    .hide_node_edits(&tree, &[rel("Packages/manifest.json"), rel("Packages/packages-lock.json")])
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                let bridge = crate::forks::submit::Bridge {
                    config: bridge_json.clone(),
                    token: std::fs::read_to_string(&token_file)?.trim().to_owned(),
                };
                let known = secrets.values().into_iter().map(str::to_owned).collect();
                forks
                    .register(&id, generation, tree, provider, Some(bridge),
                        crate::forks::Commit::parse(&commit).map_err(|e| anyhow::anyhow!("{e}"))?, known)
                    .await
            }
            .await;
            receipt["forks"] = match registered {
                Ok(()) => json!(true),
                Err(e) => json!(format!("unavailable: {e:#}")),
            };
        }
        // The stream host, restarted while the session runs (its permit is fixed).
        let session_file = session_dir.join("moq-session.json");
        let mut ready = false;
        let mut quick_failures = 0;
        loop {
            // Always the port the bridge answers on now (it may have moved
            // after a domain reload let another program take the old one).
            bridge.answering(Duration::from_secs(120), &mut stop).await?;
            receipt["bridge_port_moves"] = json!(bridge.moves);
            std::fs::write(
                &session_file,
                serde_json::to_vec(&{
                    let mut session = stream_session(&spec, moq, bridge.port, &token_file, &ctx.config.ffmpeg);
                    if let Some(block) = ctx.controller.as_deref().zip(secrets.webrtc.as_ref()).and_then(|(origin, w)| webrtc_block(origin, w, &spec, &id, generation)) {
                        session["webrtc"] = block;
                    }
                    session
                })?,
            )?;
            let mut host = Command::new(&ctx.config.stream_host);
            host.arg("--moq-session")
                .arg(&session_file)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            let index = g.adopt(host.spawn().context("Cannot start the stream host")?, &pids)?;
            let stdout = g.child(index).stdout.take().context("stream host output")?;
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            let launched = Instant::now();
            loop {
                tokio::select! {
                    _ = cancelled(&mut stop) => bail!("Stopped"),
                    line = lines.next_line() => match line {
                        Ok(Some(line)) => {
                            if !ready && line.contains("\"moq-source-running\"") {
                                ready = true;
                                receipt["first_stream_seconds"] = json!(started.elapsed().as_secs());
                                report("ready", None, json!({"commit": receipt["commit"]}));
                            }
                        }
                        _ => break,
                    },
                }
            }
            let exit = g.child(index).wait().await?;
            if editor_proc.exited(g)?.is_some() {
                bail!("The Unity Editor exited");
            }
            // A clean exit is the stream host's own end: its fixed permit ran
            // out, or no controlling client kept it alive (about 15 s without
            // control liveness). Restart it; only failures count.
            if !exit.success() && launched.elapsed() < Duration::from_secs(30) {
                quick_failures += 1;
                ensure!(quick_failures < 5, "The stream host keeps failing to publish");
            } else {
                quick_failures = 0;
            }
            tokio::select! {
                _ = cancelled(&mut stop) => bail!("Stopped"),
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
    }
    .await;
    if let Some(forks) = &ctx.forks {
        forks.unregister(&id).await;
    }
    if let Some(g) = group.as_mut() {
        g.terminate().await;
    }
    // The world build cache outlives the session (its processes have ended).
    return_world_builds(&ctx.work, &session_dir);
    // Keep the Editor log (its last MiB) beside the receipt for the owner;
    // secrets on disk go with the session directory.
    let receipts = ctx.private.join("engine-receipts");
    if private_dir(&receipts).is_ok()
        && let Ok(log) = std::fs::read(session_dir.join("unity.log")).or_else(|_| {
            std::fs::read(
                ctx.work
                    .join("engine")
                    .join("editor")
                    .join(&id)
                    .join("unity.log"),
            )
        })
    {
        let tail = &log[log.len().saturating_sub(1 << 20)..];
        let _ = std::fs::write(receipts.join(format!("{id}.unity.log")), tail);
    }
    let _ = std::fs::remove_dir_all(&session_dir);
    let _ = std::fs::remove_dir_all(ctx.work.join("engine").join("editor").join(&id));
    let stopped = *stop.borrow();
    let error = match &outcome {
        Err(e) if !stopped => Some(redact(&format!("{e:#}"), &secrets.values())),
        _ => None,
    };
    receipt["state"] = json!(if error.is_some() { "failed" } else { "ended" });
    receipt["error"] = json!(error);
    receipt["seconds"] = json!(started.elapsed().as_secs());
    receipt["ended_at"] = json!(chrono::Utc::now().to_rfc3339());
    if private_dir(&receipts).is_ok() {
        let _ = atomic_json(&receipts.join(format!("{id}.json")), &receipt);
    }
    match error {
        Some(error) => report("failed", Some(error), json!({})),
        None => report("ended", None, json!({})),
    }
}

/// A world session after activation: lend the build cache, write the
/// owner-only session file, and run the world host in the session's Job
/// Object until the session stops. Ready on the host's first
/// `world-host-connected` line; restarted on exit like the stream host (only
/// quick failures count, five in a row fail the session).
#[allow(clippy::too_many_arguments)]
async fn run_world_host(
    ctx: &SessionContext,
    spec: &Spec,
    secrets: &Secrets,
    id: &str,
    generation: u64,
    unity: &Path,
    project: &Path,
    session_dir: &Path,
    g: &mut Group,
    pids: &PidRecord,
    stop: &mut watch::Receiver<bool>,
    receipt: &mut Value,
    started: Instant,
    report: &(dyn Fn(&'static str, Option<String>, Value) + Send + Sync),
) -> Result<()> {
    let host = ctx
        .config
        .world_host
        .clone()
        .context("engine.json names no world_host")?;
    let world = spec
        .world
        .as_ref()
        .context("The world session names no room")?;
    let secret = secrets
        .world
        .as_ref()
        .context("The world session has no enrollment code; start the world again.")?;
    let work_dir = session_dir.join("world");
    private_dir(&work_dir)?;
    lend_world_builds(&ctx.work, &work_dir)?;
    let session_file = session_dir.join("world-session.json");
    atomic_json(
        &session_file,
        &world_session(world, secret, id, generation, unity, project, &work_dir),
    )?;
    report("starting", None, json!({}));
    let mut ready = false;
    let mut quick_failures = 0;
    loop {
        let mut command = Command::new(&host);
        command
            .args(world_host_argv(&session_file))
            .current_dir(&work_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let index = g.adopt(
            command.spawn().context("Cannot start the world host")?,
            pids,
        )?;
        let stdout = g.child(index).stdout.take().context("world host output")?;
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        let launched = Instant::now();
        loop {
            tokio::select! {
                _ = cancelled(stop) => bail!("Stopped"),
                line = lines.next_line() => match line {
                    Ok(Some(line)) => {
                        if !ready && world_host_ready(&line) {
                            ready = true;
                            receipt["world_host_connected_seconds"] = json!(started.elapsed().as_secs());
                            report("ready", None, json!({"commit": receipt["commit"]}));
                        }
                    }
                    _ => break,
                },
            }
        }
        let exit = g.child(index).wait().await?;
        receipt["world_host_exits"] = json!(receipt["world_host_exits"].as_u64().unwrap_or(0) + 1);
        if !exit.success() && launched.elapsed() < Duration::from_secs(30) {
            quick_failures += 1;
            ensure!(
                quick_failures < 5,
                "The world host keeps failing (exit {}); see the world host's log on the engine machine",
                exit.code().unwrap_or(-1)
            );
        } else {
            quick_failures = 0;
        }
        tokio::select! {
            _ = cancelled(stop) => bail!("Stopped"),
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }
}

/// The node side of the engine channel: running sessions and pending reports.
pub struct Executor {
    config_path: PathBuf,
    work: PathBuf,
    private: PathBuf,
    running: BTreeMap<String, (u64, watch::Sender<bool>)>,
    reports_tx: mpsc::UnboundedSender<Report>,
    reports_rx: mpsc::UnboundedReceiver<Report>,
    controller: Option<String>,
    forks: Option<crate::forks::executor::ForkExecutor>,
    accounts: Option<std::sync::Arc<crate::forks::slots::Accounts>>,
}
impl Executor {
    /// `config_path` is read when a session starts, so an owner's fix to
    /// `engine.json` applies without restarting the node.
    pub fn new(config_path: PathBuf, work: PathBuf, private: PathBuf) -> Self {
        let (reports_tx, reports_rx) = mpsc::unbounded_channel();
        Self {
            config_path,
            work,
            private,
            running: BTreeMap::new(),
            reports_tx,
            reports_rx,
            controller: None,
            forks: None,
            accounts: None,
        }
    }
    /// Run agent forks of this machine's sessions (needs a Tokio runtime).
    /// Seals the installer's account passwords on Windows.
    pub fn with_forks(mut self) -> Self {
        let accounts = match crate::forks::slots::load_accounts(&self.private) {
            Ok(a) => a.map(std::sync::Arc::new),
            Err(_) => None,
        };
        let tools = EngineConfig::load(&self.config_path)
            .ok()
            .and_then(|c| c.agents)
            .unwrap_or_default();
        self.forks = Some(crate::forks::executor::ForkExecutor::new(
            accounts.as_deref(),
            tools,
        ));
        self.accounts = accounts;
        self
    }
    /// `tools.engine.forks` for the heartbeat (None without forks).
    pub fn forks_inventory(&self) -> Option<Value> {
        self.forks.as_ref().map(|f| f.inventory())
    }
    /// Fork posts (`fork-report`, `fork-events`, `fork-artifact`) to send.
    pub fn drain_forks(&mut self) -> Vec<crate::forks::executor::Post> {
        self.forks.as_ref().map(|f| f.drain()).unwrap_or_default()
    }
    /// The controller origin sessions' stream hosts signal WebRTC peers through.
    pub fn with_controller(mut self, origin: String) -> Self {
        self.controller = Some(origin);
        self
    }
    pub fn running(&self) -> usize {
        self.running.len()
    }
    /// Apply one `engine-poll` reply; returns reports to send now (in order).
    pub fn apply(&mut self, polled: &Value) -> Vec<Report> {
        let mut immediate = Vec::new();
        let rows = polled["sessions"].as_array().cloned().unwrap_or_default();
        let mut listed = Vec::new();
        for row in rows.iter().take(16) {
            let (Some(id), Some(generation)) = (row["id"].as_str(), row["generation"].as_u64())
            else {
                continue;
            };
            if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            listed.push(id.to_owned());
            let desired = row["desired"].as_str().unwrap_or("stop");
            match (desired, self.running.get(id)) {
                ("run", Some((g, _))) if *g == generation => {
                    if let Some(f) = &self.forks {
                        f.apply(id, row);
                    }
                }
                ("run", existing) => {
                    if let Some((_, stop)) = existing {
                        let _ = stop.send(true);
                    }
                    let parsed = serde_json::from_value::<Spec>(row["spec"].clone())
                        .and_then(|spec| {
                            serde_json::from_value::<Secrets>(row["secrets"].clone())
                                .map(|s| (spec, s))
                        })
                        .map_err(|_| {
                            "The engine machine could not start this session; start a new one."
                                .to_owned()
                        })
                        .and_then(|(spec, secrets)| {
                            EngineConfig::load(&self.config_path)
                                .map(|config| (spec, secrets, config))
                                .map_err(|e| {
                                    format!("The engine machine is not set up for sessions: {e:#}")
                                        .chars()
                                        .take(400)
                                        .collect()
                                })
                        });
                    match parsed {
                        // One session per machine in v1.
                        Ok(_) if self.running.keys().any(|k| k != id) => immediate.push(Report {
                            id: id.into(),
                            generation,
                            state: "failed",
                            error: Some("This engine machine is already running a session.".into()),
                            detail: json!({}),
                        }),
                        Ok((spec, secrets, config)) => {
                            if let (Some(f), Some(tools)) = (&self.forks, &config.agents) {
                                f.set_tools(tools.clone());
                            }
                            let (tx, rx) = watch::channel(false);
                            self.running.insert(id.to_owned(), (generation, tx));
                            tokio::spawn(run_session(
                                std::sync::Arc::new(SessionContext {
                                    config,
                                    work: self.work.clone(),
                                    private: self.private.clone(),
                                    controller: self.controller.clone(),
                                    forks: self.forks.clone(),
                                    accounts: self.accounts.clone(),
                                }),
                                id.to_owned(),
                                generation,
                                spec,
                                secrets,
                                rx,
                                self.reports_tx.clone(),
                            ));
                        }
                        // Placed earlier but not running here (this node process
                        // never received or lost its secrets), or not set up.
                        Err(error) => immediate.push(Report {
                            id: id.into(),
                            generation,
                            state: "failed",
                            error: Some(error),
                            detail: json!({}),
                        }),
                    }
                }
                (_, Some((_, stop))) => {
                    let _ = stop.send(true);
                }
                (_, None) => immediate.push(Report {
                    id: id.into(),
                    generation,
                    state: "ended",
                    error: None,
                    detail: json!({}),
                }),
            }
        }
        // Anything no longer listed was ended, lost or fenced: stop it.
        for (id, (_, stop)) in &self.running {
            if !listed.contains(id) {
                let _ = stop.send(true);
            }
        }
        immediate
    }
    /// Reports the session tasks produced since the last call.
    pub fn drain(&mut self) -> Vec<Report> {
        let mut out = Vec::new();
        // `dev` updates ride on `ready` reports of the running generation.
        for d in self
            .forks
            .as_ref()
            .map(|f| f.drain_dev())
            .unwrap_or_default()
        {
            if self
                .running
                .get(&d.session)
                .is_some_and(|(g, _)| *g == d.generation)
            {
                out.push(Report {
                    id: d.session,
                    generation: d.generation,
                    state: "ready",
                    error: None,
                    detail: json!({"dev": d.dev}),
                });
            }
        }
        while let Ok(r) = self.reports_rx.try_recv() {
            if matches!(r.state, "ended" | "failed") {
                self.running
                    .remove(&r.id)
                    .filter(|(g, _)| *g != r.generation)
                    .map(|entry| self.running.insert(r.id.clone(), entry));
            }
            out.push(r);
        }
        out
    }
    /// Stop everything (node shutdown).
    pub fn stop_all(&mut self) {
        for (_, stop) in self.running.values() {
            let _ = stop.send(true);
        }
    }
}
/// `enginebox-node engine-rehearse`: run a session's steps up to Unity
/// activation without a controller, for an owner checking a machine. Checks
/// out `remote` (a local `file://` clone works; no token is sent), prepares the
/// project, starts the licensing client without a token to prove it runs, and
/// proves the session Job Object terminates a process tree. Never activates
/// Unity or publishes a stream.
pub async fn rehearse(config_path: &Path, work: &Path, spec: Value, remote: &str) -> Result<Value> {
    let config = EngineConfig::load(config_path)?;
    let spec: Spec = serde_json::from_value(spec).context("Invalid session spec")?;
    spec.validate()?;
    let unity = config
        .unity
        .get(&spec.engine.version)
        .context("That Unity version is not in engine.json")?
        .clone();
    private_dir(work)?;
    let private = work.join("private");
    private_dir(&private)?;
    let ctx = SessionContext {
        config,
        work: work.to_owned(),
        private: private.clone(),
        controller: None,
        forks: None,
        accounts: None,
    };
    let (_tx, mut stop) = watch::channel(false);
    let access = RepositorySecret {
        remote: remote.to_owned(),
        token: "rehearsal-has-no-token".into(),
        auth: "bearer".into(),
    };
    let t = Instant::now();
    let (checkout, commit) = checkout(&ctx, &spec, &access, &mut stop).await?;
    let checkout_seconds = t.elapsed().as_secs();
    let session_dir = work.join("engine").join("sessions").join("rehearsal");
    private_dir(&session_dir)?;
    let (project, bridge, token_file, port) =
        prepare_project(&ctx, &spec, &checkout, &session_dir)?;
    let manifest: Value = serde_json::from_slice(&std::fs::read(
        project.join("Packages").join("manifest.json"),
    )?)?;
    let pids = PidRecord::new(private.join("engine-pids.json"));
    let mut group = Group::new()?;
    let mut licensing = Command::new(licensing_client(&unity));
    licensing
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let index = group.adopt(licensing.spawn()?, &pids)?;
    let licensing_exit = tokio::time::timeout(Duration::from_secs(60), group.child(index).wait())
        .await
        .ok()
        .and_then(|r| r.ok())
        .and_then(|s| s.code());
    // A long-lived tree in the job ends when the job is terminated.
    let mut sleeper = if cfg!(windows) {
        let mut c = Command::new("cmd.exe");
        c.args(["/c", "ping -n 120 127.0.0.1 >NUL"]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", "sleep 120"]);
        c
    };
    sleeper
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let index = group.adopt(sleeper.spawn()?, &pids)?;
    let t = Instant::now();
    group.terminate().await;
    let terminated = group.child(index).try_wait()?.is_some();
    let swept = sweep(work, &private)?;
    Ok(json!({
        "commit": commit, "checkout": checkout, "checkout_seconds": checkout_seconds,
        "project": project, "bridge_config": bridge, "bridge_token_file": token_file, "bridge_port": port,
        "manifest_bridge": manifest["dependencies"][BRIDGE_PACKAGE],
        "manifest_session": manifest["dependencies"][SESSION_PACKAGE],
        "unity_argv": unity_argv(&project, &bridge, &session_dir.join("unity.log"))
            .iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect::<Vec<_>>(),
        "licensing_client": licensing_client(&unity), "licensing_help_exit": licensing_exit,
        "job_terminated_tree": terminated, "terminate_ms": t.elapsed().as_millis() as u64,
        "sweep": swept,
    }))
}

pub fn report_body(session: &str, epoch: u64, r: &Report) -> Value {
    json!({"session": session, "epoch": epoch, "id": r.id, "generation": r.generation,
        "state": r.state, "error": r.error, "detail": r.detail})
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A loopback HTTP server answering every request with `reply` (status line and body).
    fn serve_http(reply: &'static str) -> std::net::TcpListener {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let server = listener.try_clone().unwrap();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for stream in server.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let request = String::from_utf8_lossy(&buf).to_string();
                let (status, body) = if request.contains(&format!("Bearer {}", "b".repeat(64))) {
                    reply.split_once('|').unwrap()
                } else {
                    ("403 Forbidden", "")
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        listener
    }
    fn bridge_file(dir: &Path, port: u16) -> PathBuf {
        let config = dir.join("bridge.json");
        std::fs::write(
            &config,
            json!({"port": port, "token": "b".repeat(64), "width": 1280}).to_string(),
        )
        .unwrap();
        config
    }

    #[tokio::test]
    async fn a_bridge_port_taken_by_another_program_moves_to_a_free_one() {
        let dir = tempfile::tempdir().unwrap();
        // Another program answers HTTP on the port the bridge was given.
        let foreign = serve_http("404 Not Found|{}");
        let taken = foreign.local_addr().unwrap().port();
        let config = bridge_file(dir.path(), taken);
        let mut bridge = BridgePort::new(taken, &"b".repeat(64), &config);
        for _ in 1..BRIDGE_PORT_STRIKES {
            assert!(bridge.check().await.unwrap().is_none());
            assert_eq!(bridge.port, taken, "a moment's conflict is not a move");
        }
        assert!(bridge.check().await.unwrap().is_none());
        assert_ne!(bridge.port, taken);
        assert_eq!(bridge.moves, 1);
        let written: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
        assert_eq!(written["port"], bridge.port);
        assert_eq!(
            written["token"],
            "b".repeat(64),
            "the rest of the config is kept"
        );
        assert_eq!(written["width"], 1280);
        // Nothing listens on the new port yet (the Editor is still retrying): no strikes.
        for _ in 0..BRIDGE_PORT_STRIKES + 1 {
            assert!(bridge.check().await.unwrap().is_none());
        }
        assert_eq!(bridge.moves, 1);
        drop(foreign);
    }

    #[tokio::test]
    async fn the_bridge_answering_on_its_port_is_never_moved() {
        let dir = tempfile::tempdir().unwrap();
        let bridge_server =
            serve_http(r#"200 OK|{"type":"unity-editor","playing":true,"view":"play"}"#);
        let port = bridge_server.local_addr().unwrap().port();
        let config = bridge_file(dir.path(), port);
        let mut bridge = BridgePort::new(port, &format!("{}\n", "b".repeat(64)), &config);
        for _ in 0..BRIDGE_PORT_STRIKES * 2 {
            let status = bridge.check().await.unwrap().unwrap();
            assert_eq!(status["playing"], true);
        }
        assert_eq!((bridge.port, bridge.moves), (port, 0));
        let (_, mut stop) = watch::channel(false);
        bridge
            .answering(Duration::from_secs(5), &mut stop)
            .await
            .unwrap();
        // A listener that answers 200 but is not the bridge still counts as foreign.
        let impostor = serve_http(r#"200 OK|{"ok":true}"#);
        let held = impostor.local_addr().unwrap().port();
        let mut other = BridgePort::new(held, &"b".repeat(64), &bridge_file(dir.path(), held));
        for _ in 0..BRIDGE_PORT_STRIKES {
            other.check().await.unwrap();
        }
        assert_ne!(other.port, held);
    }

    #[tokio::test]
    async fn a_port_that_keeps_being_taken_fails_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let foreign = serve_http("404 Not Found|{}");
        let taken = foreign.local_addr().unwrap().port();
        let config = bridge_file(dir.path(), taken);
        let mut bridge = BridgePort::new(taken, &"b".repeat(64), &config);
        bridge.moves = BRIDGE_PORT_MOVES;
        for _ in 1..BRIDGE_PORT_STRIKES {
            bridge.check().await.unwrap();
        }
        let error = bridge.check().await.unwrap_err().to_string();
        assert!(error.contains("could not keep a local port"), "{error}");
    }

    #[test]
    fn known_blocking_dialogs_are_recognised() {
        let titles = |t: &[&str]| t.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            blocking_dialog(&titles(&["Opening project...", "Scene Backup Detected"])).as_deref(),
            Some("Scene Backup Detected")
        );
        assert_eq!(
            blocking_dialog(&titles(&["Opening project...", "Hold on..."])),
            None
        );
        assert_eq!(blocking_dialog(&[]), None);
    }

    #[test]
    fn unity_temp_is_cleared_before_launch() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().canonicalize().unwrap().join("unity-project");
        std::fs::create_dir_all(project.join("Temp/__Backupscenes")).unwrap();
        std::fs::write(project.join("Temp/__Backupscenes/0.backup"), "scene").unwrap();
        std::fs::create_dir_all(project.join("Library")).unwrap();
        clear_unity_temp(&project).unwrap();
        assert!(!project.join("Temp").exists());
        assert!(project.join("Library").is_dir());
        clear_unity_temp(&project).unwrap();
    }

    #[test]
    fn editor_prefs_names_match_unity() {
        // Values Unity 6 wrote on a real Windows account.
        assert_eq!(
            editor_prefs_name("Terms-43dc734cac67bb5fb31d4692eb48c6b3"),
            "Terms-43dc734cac67bb5fb31d4692eb48c6b3_h3816273780"
        );
        assert_eq!(
            editor_prefs_name("UnityMcpServer.AutoStart"),
            "UnityMcpServer.AutoStart_h4003269344"
        );
    }

    #[test]
    fn terms_key_comes_from_the_editor_binary() {
        let dir = tempfile::tempdir().unwrap();
        let unity = dir.path().join("Unity.exe");
        std::fs::write(&unity, b"no terms here").unwrap();
        assert_eq!(unity_terms_key(&unity), None);
        let mut dll = b"\0\0SoftwareTermsWndProc\0Terms-".to_vec();
        dll.extend_from_slice(b"a406f2015eeb4822cbb013b27860953b\0tail");
        std::fs::write(dir.path().join("Unity.dll"), dll).unwrap();
        assert_eq!(
            unity_terms_key(&unity).as_deref(),
            Some("Terms-a406f2015eeb4822cbb013b27860953b")
        );
    }

    #[test]
    fn bridge_is_staged_writable_and_restaged_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let source = root.join("bridge");
        std::fs::create_dir_all(source.join("Editor")).unwrap();
        std::fs::create_dir_all(source.join("node_modules/x")).unwrap();
        std::fs::write(
            source.join("package.json"),
            r#"{"name":"dev.unityjam.editor-bridge"}"#,
        )
        .unwrap();
        std::fs::write(source.join("README.md"), "no meta").unwrap();
        std::fs::write(source.join("Editor/Bridge.cs"), "class B {}").unwrap();
        std::fs::write(source.join("node_modules/x/big.js"), "skip").unwrap();
        let cache = root.join("cache");
        let first = stage_bridge(&source, &cache).unwrap();
        assert!(first.join("README.md").is_file() && first.join("Editor/Bridge.cs").is_file());
        assert!(!first.join("node_modules").exists());
        // Unity may write the missing meta into the copy; staging again keeps it.
        std::fs::write(first.join("README.md.meta"), "guid").unwrap();
        assert_eq!(stage_bridge(&source, &cache).unwrap(), first);
        assert!(first.join("README.md.meta").is_file());
        std::fs::write(source.join("Editor/Bridge.cs"), "class B { int x; }").unwrap();
        let second = stage_bridge(&source, &cache).unwrap();
        assert_ne!(second, first);
        assert!(!first.exists(), "the old copy is removed");
        assert_eq!(
            std::fs::read_to_string(second.join("Editor/Bridge.cs")).unwrap(),
            "class B { int x; }"
        );
    }

    #[test]
    fn unity_region_pin_replaces_a_regional_config() {
        assert_eq!(
            serde_json::from_str::<Value>(UNITY_GLOBAL_CONFIG).unwrap()["license"],
            "https://license.unity3d.com"
        );
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config/production.json");
        assert!(pin_unity_region(&file).unwrap());
        std::fs::set_permissions(&file, {
            let mut p = std::fs::metadata(&file).unwrap().permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            p.set_readonly(false);
            p
        })
        .unwrap();
        std::fs::write(&file, r#"{"license":"https://license.unity.cn"}"#).unwrap();
        assert!(pin_unity_region(&file).unwrap());
        assert!(std::fs::metadata(&file).unwrap().permissions().readonly());
        assert!(
            !pin_unity_region(&file).unwrap(),
            "an already global config is left alone"
        );
        let pinned: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(pinned["core"], "https://core.cloud.unity3d.com");
    }

    #[test]
    fn licensing_logs_lose_the_access_token() {
        let dir = tempfile::tempdir().unwrap();
        let token = "unity-access-token-0123456789";
        let log = dir.path().join("Unity.Licensing.Client.log");
        std::fs::write(
            &log,
            format!(
                "args: --activate-all --accessToken {token} --include-personal\nagain {token}\n"
            ),
        )
        .unwrap();
        let other = dir.path().join("Editor.log");
        std::fs::write(&other, token).unwrap();
        assert_eq!(scrub_licensing_logs(dir.path(), token).unwrap(), 1);
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(!text.contains(token));
        assert_eq!(text.matches("<redacted>").count(), 2);
        assert!(text.contains("--include-personal"));
        // Only the licensing client's logs are touched; a clean log stays as it is.
        assert_eq!(std::fs::read_to_string(&other).unwrap(), token);
        assert_eq!(scrub_licensing_logs(dir.path(), token).unwrap(), 0);
        assert_eq!(
            scrub_licensing_logs(&dir.path().join("missing"), token).unwrap(),
            0
        );
    }

    fn fixture() -> (tempfile::TempDir, EngineConfig) {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        let root = canonical.as_path();
        let editor = root.join("Editor");
        let client = editor.join("Data/Resources/Licensing/Client");
        std::fs::create_dir_all(&client).unwrap();
        std::fs::write(editor.join("Unity.exe"), "").unwrap();
        std::fs::write(licensing_client(&editor.join("Unity.exe")), "").unwrap();
        std::fs::write(root.join("enginebox-stream-host.exe"), "").unwrap();
        std::fs::write(root.join("ffmpeg.exe"), "").unwrap();
        let bridge = root.join("unity-editor-bridge");
        std::fs::create_dir_all(&bridge).unwrap();
        std::fs::write(
            bridge.join("package.json"),
            r#"{"name":"dev.unityjam.editor-bridge"}"#,
        )
        .unwrap();
        let config = EngineConfig {
            schema: 1,
            unity: BTreeMap::from([("6000.6.5f1".into(), editor.join("Unity.exe"))]),
            stream_host: root.join("enginebox-stream-host.exe"),
            ffmpeg: root.join("ffmpeg.exe"),
            editor_bridge: bridge,
            git: None,
            startup_timeout_seconds: None,
            unity_region: None,
            world_host: None,
            agents: None,
            editor_account: None,
        };
        (dir, config)
    }
    fn spec() -> Spec {
        serde_json::from_value(json!({"repository":"qoh/queen-of-hearts","ref":"main","project_path":"unity-project",
            "engine":{"kind":"unity","version":"6000.6.5f1"},"width":1280,"height":720,"scene":"Assets/_Project/Scenes/Prototype.unity",
            "namespace":"eb-0123456789abcdef0123456789abcdef","session":"01234567-89ab-4def-8123-456789abcdef","tag":7})).unwrap()
    }
    const REPO: &str = "repo-read-secret-value";
    const UNITY: &str = "unity-access-secret-value";
    const PUBLISH: &str = "aaaa.publish-secret-value.cccc";
    fn secrets() -> Secrets {
        serde_json::from_value(json!({"repository":{"remote":"https://github.com/coop-collective/queen-of-hearts.git","token":REPO,"auth":"github_token","expires_at":""},
            "unity":{"access_token":UNITY},"moq":{"relay_url":"https://draft-16.cloudflare.mediaoverquic.com","publish_token":PUBLISH,"control_key":"f".repeat(64)}})).unwrap()
    }

    #[test]
    fn config_validation_names_what_is_wrong() {
        let (dir, config) = fixture();
        config.validate().unwrap();
        let path = dir.path().canonicalize().unwrap().join(CONFIG_FILE);
        std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
        assert_eq!(EngineConfig::load(&path).unwrap(), config);
        let inv = inventory(&path, 0);
        assert_eq!(
            inv,
            json!({"ready":true,"unity":["6000.6.5f1"],"running":0,"world":false})
        );
        // An optional world host makes the machine report world sessions.
        let host = dir.path().join("gamejam-world-host.exe");
        std::fs::write(&host, "").unwrap();
        let mut world = config.clone();
        world.world_host = Some(host.canonicalize().unwrap());
        world.validate().unwrap();
        std::fs::write(&path, serde_json::to_vec(&world).unwrap()).unwrap();
        assert_eq!(inventory(&path, 1)["world"], true);
        std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
        let mut bad = world.clone();
        bad.world_host = Some(PathBuf::from("gamejam-world-host.exe"));
        assert!(bad.validate().is_err());
        let mut bad = world.clone();
        bad.world_host = Some(config.ffmpeg.clone());
        assert!(bad.validate().is_err());
        let mut bad = config.clone();
        bad.unity = BTreeMap::from([("6000.6.5".into(), config.unity["6000.6.5f1"].clone())]);
        assert!(bad.validate().is_err());
        let mut bad = config.clone();
        bad.stream_host = PathBuf::from("enginebox-stream-host.exe");
        assert!(bad.validate().is_err());
        let mut bad = config.clone();
        bad.ffmpeg = dir
            .path()
            .canonicalize()
            .unwrap()
            .join("missing/ffmpeg.exe");
        assert!(bad.validate().unwrap_err().to_string().contains("ffmpeg"));
        let mut bad = config.clone();
        bad.stream_host = config.ffmpeg.clone();
        assert!(bad.validate().is_err());
        std::fs::write(
            config.editor_bridge.join("package.json"),
            r#"{"name":"other"}"#,
        )
        .unwrap();
        assert!(config.validate().is_err());
        std::fs::write(&path, r#"{"schema":1,"unity":{},"extra":1}"#).unwrap();
        assert!(EngineConfig::load(&path).is_err());
        assert_eq!(inventory(&path, 0)["ready"], false);
        std::fs::remove_file(licensing_client(&config.unity["6000.6.5f1"])).unwrap();
        assert!(config.validate().is_err());
    }

    /// The checkout keeps its repository outside the dev tree, moves an
    /// older in-tree `.git` there once, and keeps ignored caches.
    #[tokio::test]
    async fn checkouts_use_a_node_private_repository() {
        let lfs = std::process::Command::new("git")
            .args(["lfs", "version"])
            .output();
        if !lfs.is_ok_and(|o| o.status.success()) {
            eprintln!("git-lfs is not installed; skipping");
            return;
        }
        let (dir, mut config) = fixture();
        config.git = None;
        let root = dir.path().canonicalize().unwrap();
        let source = root.join("source");
        std::fs::create_dir_all(source.join("unity-project/Assets")).unwrap();
        std::fs::write(source.join("unity-project/Assets/a.txt"), "a").unwrap();
        std::fs::write(source.join(".gitignore"), "unity-project/Library/\n").unwrap();
        let git = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .current_dir(&source)
                    .args(args)
                    .env("GIT_AUTHOR_NAME", "t")
                    .env("GIT_AUTHOR_EMAIL", "t@t")
                    .env("GIT_COMMITTER_NAME", "t")
                    .env("GIT_COMMITTER_EMAIL", "t@t")
                    .status()
                    .unwrap()
                    .success()
            );
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);
        let remote = format!("file://{}", source.display());
        let ctx = SessionContext {
            config,
            work: root.join("work"),
            private: root.join("private"),
            controller: None,
            forks: None,
            accounts: None,
        };
        let access = RepositorySecret {
            remote: remote.clone(),
            token: "rehearsal-has-no-token".into(),
            auth: "bearer".into(),
        };
        let mut spec = spec();
        spec.git_ref = "main".into();
        let (_tx, mut stop) = watch::channel(false);
        // An older node's checkout: .git inside, plus a Library cache.
        let key = checkout_key(&remote);
        let (tree, git_dir, index) = crate::forks::executor::repo_paths(&ctx.work, &key);
        std::fs::create_dir_all(tree.parent().unwrap()).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["clone", "-q", &remote])
                .arg(&tree)
                .status()
                .unwrap()
                .success()
        );
        std::fs::create_dir_all(tree.join("unity-project/Library")).unwrap();
        std::fs::write(tree.join("unity-project/Library/cache"), "c").unwrap();
        let (checkout_dir, commit) = checkout(&ctx, &spec, &access, &mut stop).await.unwrap();
        assert_eq!(checkout_dir, tree);
        assert_eq!(commit.len(), 40);
        assert!(!tree.join(".git").exists());
        assert!(git_dir.join("HEAD").is_file() && index.is_file());
        assert_eq!(
            std::fs::read_to_string(tree.join("unity-project/Assets/a.txt")).unwrap(),
            "a"
        );
        assert!(tree.join("unity-project/Library/cache").is_file());
        // A second session reuses it, and the dev tree is a fork-able work tree.
        let (_, again) = checkout(&ctx, &spec, &access, &mut stop).await.unwrap();
        assert_eq!(again, commit);
        let tree_info = dev_tree(&ctx, &spec, &remote, &"a".repeat(32));
        let p = crate::forks::git::GitTreeProvider::new(
            "git".into(),
            &ctx.work.join("engine/repos/.node"),
            false,
        )
        .unwrap();
        use crate::forks::ForkProvider;
        p.start_dev(&tree_info, &crate::forks::Commit::parse(&commit).unwrap())
            .unwrap();
        std::fs::write(tree.join("unity-project/Assets/b.txt"), "b").unwrap();
        let cap = p.capture_live(&tree_info, &[]).unwrap();
        assert_eq!(cap.changed.len(), 1);
    }

    #[test]
    fn git_gets_the_token_only_through_scoped_config_env() {
        let s = secrets();
        let plan = git_plan(
            Path::new("/work/engine/repos/x.git"),
            Path::new("/work/engine/checkouts/x"),
            &s.repository.remote,
            "main",
            true,
        );
        let argv: Vec<String> = plan
            .iter()
            .flatten()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(
            !argv
                .iter()
                .any(|a| a.contains(REPO) || a.contains("Authorization"))
        );
        assert_eq!(
            plan.iter()
                .map(|a| a
                    .iter()
                    .map(|x| x.to_string_lossy().to_string())
                    .collect::<Vec<_>>()
                    .join(" "))
                .collect::<Vec<_>>(),
            vec![
                "--git-dir=/work/engine/repos/x.git --work-tree=/work/engine/checkouts/x init -q",
                "--git-dir=/work/engine/repos/x.git --work-tree=/work/engine/checkouts/x remote add origin https://github.com/coop-collective/queen-of-hearts.git",
                "--git-dir=/work/engine/repos/x.git --work-tree=/work/engine/checkouts/x lfs install --local --skip-smudge",
                "--git-dir=/work/engine/repos/x.git --work-tree=/work/engine/checkouts/x fetch --no-tags --depth=1 origin main",
                "--git-dir=/work/engine/repos/x.git --work-tree=/work/engine/checkouts/x checkout --detach -f FETCH_HEAD",
                "--git-dir=/work/engine/repos/x.git --work-tree=/work/engine/checkouts/x clean -ffdq",
                "--git-dir=/work/engine/repos/x.git --work-tree=/work/engine/checkouts/x lfs pull origin",
            ]
        );
        let env: BTreeMap<String, String> = git_env(&s.repository).into_iter().collect();
        assert_eq!(
            env["GIT_CONFIG_KEY_1"],
            "http.https://github.com/coop-collective/queen-of-hearts.git.extraHeader"
        );
        assert_eq!(
            env["GIT_CONFIG_VALUE_1"],
            format!(
                "Authorization: Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{REPO}"))
            )
        );
        assert_eq!(env["GIT_CONFIG_KEY_0"], "credential.helper");
        assert_eq!(env["GIT_TERMINAL_PROMPT"], "0");
        let mut bearer = s.repository.clone();
        bearer.auth = "bearer".into();
        assert_eq!(
            git_env(&bearer)[7].1,
            format!("Authorization: Bearer {REPO}")
        );
    }

    #[test]
    fn editor_and_stream_host_argv_carry_no_secrets_and_files_match_their_readers() {
        let s = secrets();
        let spec = spec();
        spec.validate().unwrap();
        let unity = unity_argv(
            Path::new("/w/c/unity-project"),
            Path::new("/w/s/bridge.json"),
            Path::new("/w/s/unity.log"),
        );
        let unity: Vec<String> = unity
            .iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            unity,
            [
                "-acceptSoftwareTermsForThisRunOnly",
                "-projectPath",
                "/w/c/unity-project",
                "-jam-editor-config",
                "/w/s/bridge.json",
                "-logFile",
                "/w/s/unity.log"
            ]
        );
        for secret in [REPO, UNITY, PUBLISH] {
            assert!(!unity.iter().any(|a| a.contains(secret)));
        }
        // The documented exception: Unity's licensing client takes the token as an argument.
        assert_eq!(
            licensing_argv(UNITY),
            [
                "--activate-all",
                "--accessToken",
                UNITY,
                "--include-personal"
            ]
        );
        let session = stream_session(
            &spec,
            s.moq.as_ref().unwrap(),
            4100,
            Path::new("/w/s/bridge.token"),
            Path::new("/ffmpeg.exe"),
        );
        assert_eq!(
            session,
            json!({"schema":1,"token":PUBLISH,"controlKey":"f".repeat(64),"session":"01234567-89ab-4def-8123-456789abcdef",
                "namespace":"eb-0123456789abcdef0123456789abcdef","tag":7,"durationSeconds":600,"width":1280,"height":720,
                "capture":{"kind":"unity-editor","origin":"http://127.0.0.1:4100","tokenFile":"/w/s/bridge.token","ffmpeg":"/ffmpeg.exe","editor":true}})
        );
        assert!(!session.to_string().contains(UNITY) && !session.to_string().contains(REPO));
        let host = WebrtcSecret {
            host_token: format!("ehp_{}", "a".repeat(64)),
        };
        assert_eq!(
            webrtc_block(
                "https://enginebox.example",
                &host,
                &spec,
                "0123456789abcdef0123456789abcdef",
                3
            ),
            Some(
                json!({"controller":"https://enginebox.example","token":host.host_token,"project":spec.repository.split('/').next().unwrap(),
                "engineSession":"0123456789abcdef0123456789abcdef","generation":3})
            )
        );
        let bad = WebrtcSecret {
            host_token: "ehp_short".into(),
        };
        assert!(webrtc_block("https://enginebox.example", &bad, &spec, "x", 1).is_none());
        let bridge = bridge_config(4100, &"a".repeat(64), &spec);
        assert_eq!(
            bridge,
            json!({"port":4100,"token":"a".repeat(64),"scene":"Assets/_Project/Scenes/Prototype.unity","width":1280,"height":720,"playOnly":false})
        );
        let mut bad = spec.clone();
        bad.namespace = "eb/x".into();
        assert!(bad.validate().is_err());
        let mut bad = spec.clone();
        bad.git_ref = "--upload-pack=x".into();
        assert!(bad.validate().is_err());
        let mut bad = spec.clone();
        bad.project_path = "../x".into();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn project_preparation_adds_packages_and_checks_the_editor_version() {
        let (dir, config) = fixture();
        let checkout = dir.path().canonicalize().unwrap().join("checkout");
        let project = checkout.join("unity-project");
        std::fs::create_dir_all(project.join("Packages")).unwrap();
        std::fs::create_dir_all(project.join("ProjectSettings")).unwrap();
        std::fs::write(
            project.join("Packages/manifest.json"),
            r#"{"dependencies":{"com.unity.inputsystem":"1.20.0"}}"#,
        )
        .unwrap();
        std::fs::write(
            project.join("ProjectSettings/ProjectVersion.txt"),
            "m_EditorVersion: 6000.6.5f1\nm_EditorVersionWithRevision: 6000.6.5f1 (3ff58d469c8a)\n",
        )
        .unwrap();
        let ctx = SessionContext {
            config: config.clone(),
            work: dir.path().canonicalize().unwrap().join("work"),
            private: dir.path().canonicalize().unwrap().join("private"),
            controller: None,
            forks: None,
            accounts: None,
        };
        let session = dir.path().canonicalize().unwrap().join("session");
        std::fs::create_dir_all(&session).unwrap();
        let (found, bridge, token, port) =
            prepare_project(&ctx, &spec(), &checkout, &session).unwrap();
        assert_eq!(found, project);
        assert!(port > 0);
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(project.join("Packages/manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["dependencies"]["com.unity.inputsystem"], "1.20.0");
        // The node's own copy of the bridge, never the configured (possibly read-only) one.
        let staged = manifest["dependencies"][BRIDGE_PACKAGE].as_str().unwrap();
        assert!(staged.starts_with(&file_url(&ctx.work.join("engine/packages/editor-bridge-"))));
        assert_ne!(staged, file_url(&config.editor_bridge));
        assert!(
            manifest["dependencies"][SESSION_PACKAGE]
                .as_str()
                .unwrap()
                .starts_with("file:")
        );
        assert!(
            session
                .join("packages")
                .join(SESSION_PACKAGE)
                .join("Editor/AutoPlay.cs")
                .is_file()
        );
        let bridge: Value = serde_json::from_slice(&std::fs::read(bridge).unwrap()).unwrap();
        assert_eq!(
            bridge["token"].as_str().unwrap(),
            std::fs::read_to_string(token).unwrap()
        );
        assert_eq!(bridge["port"], port);
        // A different editor version is refused before Unity would offer an upgrade.
        let mut other = spec();
        other.engine.version = "6000.0.1f1".into();
        assert!(
            prepare_project(&ctx, &other, &checkout, &session)
                .unwrap_err()
                .to_string()
                .contains("6000.6.5f1")
        );
        assert_eq!(file_url(Path::new(r"C:\a\b")), "file:C:/a/b");
    }

    #[test]
    fn sweep_removes_session_secrets_and_keeps_checkouts() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().canonicalize().unwrap().join("work");
        let private = dir.path().canonicalize().unwrap().join("private");
        std::fs::create_dir_all(work.join("engine/sessions/a")).unwrap();
        std::fs::create_dir_all(work.join("engine/checkouts/k/Library")).unwrap();
        std::fs::create_dir_all(&private).unwrap();
        std::fs::write(work.join("engine/sessions/a/bridge.token"), "x").unwrap();
        let record = PidRecord::new(private.join("engine-pids.json"));
        record.add(999_999).unwrap();
        record.add(999_998).unwrap();
        let swept = sweep(&work, &private).unwrap();
        assert_eq!(swept["removed_sessions"], 1);
        assert_eq!(swept["killed"], 0);
        assert!(!work.join("engine/sessions/a").exists());
        assert!(work.join("engine/checkouts/k/Library").is_dir());
        assert!(!private.join("engine-pids.json").exists());
        assert_eq!(redact(&format!("x {REPO} y"), &[REPO]), "x [redacted] y");
    }

    #[tokio::test]
    async fn executor_starts_once_per_generation_and_stops_unlisted_sessions() {
        let (dir, config) = fixture();
        let path = dir.path().canonicalize().unwrap().join(CONFIG_FILE);
        std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
        let mut ex = Executor::new(
            path,
            dir.path().canonicalize().unwrap().join("work"),
            dir.path().canonicalize().unwrap().join("private"),
        );
        let id = "a".repeat(32);
        // A stop for an unknown session confirms it ended.
        let r = ex.apply(&json!({"sessions":[{"id":id,"generation":1,"desired":"stop"}]}));
        assert_eq!(r[0].state, "ended");
        // A run without secrets cannot start here.
        let r = ex.apply(&json!({"sessions":[{"id":id,"generation":1,"desired":"run","spec":{}}]}));
        assert_eq!(r[0].state, "failed");
        assert_eq!(ex.running(), 0);
        let row = json!({"id":id,"generation":2,"desired":"run",
            "spec":{"repository":"qoh/queen-of-hearts","ref":"main","project_path":"unity-project","engine":{"kind":"unity","version":"6000.6.5f1"},"width":1280,"height":720,"namespace":"eb-x","session":"01234567-89ab-4def-8123-456789abcdef","tag":1},
            "secrets":{"repository":{"remote":"https://example.invalid/r.git","token":"t","auth":"bearer"},"unity":{"access_token":"u"},"moq":{"publish_token":"p","control_key":"k"}}});
        assert!(ex.apply(&json!({"sessions":[row.clone()]})).is_empty());
        assert_eq!(ex.running(), 1);
        // A repeat poll does not start it twice; a second session is refused.
        assert!(ex.apply(&json!({"sessions":[row.clone()]})).is_empty());
        let mut other = row.clone();
        other["id"] = json!("b".repeat(32));
        let r = ex.apply(&json!({"sessions":[row.clone(), other]}));
        assert_eq!(r[0].state, "failed");
        // Unlisted: stopped; the task reports ended (git is never reached).
        ex.apply(&json!({"sessions":[]}));
        let mut reports = Vec::new();
        for _ in 0..100 {
            reports.extend(ex.drain());
            if reports
                .iter()
                .any(|r| matches!(r.state, "ended" | "failed"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(reports.iter().any(|r| r.state == "preparing"));
        assert!(
            reports
                .iter()
                .any(|r| matches!(r.state, "ended" | "failed"))
        );
        assert_eq!(ex.running(), 0);
        let body = report_body("s", 3, &reports[0]);
        assert_eq!(body["epoch"], 3);
    }

    const ROOM: &str = "https://gamejam.coopcollective.dev";
    const CODE: &str = "enroll_0123456789abcdefghijklmnopqrstuvwxyz";
    fn world_spec() -> Spec {
        let mut v = json!({"repository":"queen-of-hearts/gamejam","ref":"main","project_path":"unity-examples/multiplayer-jam",
            "engine":{"kind":"unity","version":"6000.6.5f1"},"width":1280,"height":720,"scene":null,
            "namespace":"eb-0123456789abcdef0123456789abcdef","session":"01234567-89ab-4def-8123-456789abcdef","tag":7,
            "mode":"world","world":{"room_origin":ROOM}});
        v["mode"] = json!("world");
        serde_json::from_value(v).unwrap()
    }
    fn world_secrets() -> Secrets {
        serde_json::from_value(json!({"repository":{"remote":"https://github.com/coop-collective/gamejam.git","token":REPO,"auth":"github_token","expires_at":""},
            "unity":{"access_token":UNITY},"world":{"enroll_code":CODE}})).unwrap()
    }

    #[test]
    fn world_specs_name_their_room_and_editor_specs_stay_editor() {
        let spec = world_spec();
        spec.validate().unwrap();
        assert!(spec.is_world());
        assert!(!self::spec().is_world());
        let mut editor = self::spec();
        editor.mode = Some("editor".into());
        editor.validate().unwrap();
        for (mode, origin) in [
            ("world", None),
            ("editor", Some(ROOM)),
            ("player", None),
            ("world", Some("http://gamejam.coopcollective.dev")),
            ("world", Some("https://gamejam.coopcollective.dev/")),
            ("world", Some("https://gamejam.coopcollective.dev/engine")),
            ("world", Some("https://x@gamejam.coopcollective.dev")),
            ("world", Some("https://gamejam.coopcollective.dev:x")),
        ] {
            let mut bad = world_spec();
            bad.mode = Some(mode.into());
            bad.world = origin.map(|o| WorldSpec {
                room_origin: o.into(),
            });
            assert!(bad.validate().is_err(), "{mode} {origin:?}");
        }
        let mut port = world_spec();
        port.world = Some(WorldSpec {
            room_origin: "https://room.example:8443".into(),
        });
        port.validate().unwrap();
        // A world session's secrets carry no relay token; redaction covers the code.
        let s = world_secrets();
        assert!(s.moq.is_none());
        assert!(s.values().contains(&CODE));
        assert_eq!(
            redact(&format!("failed with {CODE}"), &s.values()),
            "failed with [redacted]"
        );
    }

    #[test]
    fn world_host_gets_its_secrets_only_in_the_session_file() {
        let spec = world_spec();
        let s = world_secrets();
        let session = world_session(
            spec.world.as_ref().unwrap(),
            s.world.as_ref().unwrap(),
            "0123456789abcdef0123456789abcdef",
            4,
            Path::new("/u/Unity.exe"),
            Path::new("/w/c/unity-examples/multiplayer-jam"),
            Path::new("/w/s/world"),
        );
        assert_eq!(
            session,
            json!({"schema":1,"room_origin":ROOM,"enroll_code":CODE,
                "engine_session":"0123456789abcdef0123456789abcdef","generation":4,
                "unity_exe":"/u/Unity.exe","project_dir":"/w/c/unity-examples/multiplayer-jam","work_dir":"/w/s/world"})
        );
        for secret in [REPO, UNITY] {
            assert!(!session.to_string().contains(secret));
        }
        let argv: Vec<String> = world_host_argv(Path::new("/w/s/world-session.json"))
            .iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(argv, ["--session", "/w/s/world-session.json"]);
        for secret in [REPO, UNITY, CODE] {
            assert!(!argv.iter().any(|a| a.contains(secret)));
        }
    }

    #[test]
    fn world_host_readiness_is_its_connected_event_line() {
        assert!(world_host_ready(r#"{"event":"world-host-connected"}"#));
        assert!(world_host_ready("{\"event\": \"world-host-connected\"}\r"));
        for line in [
            "",
            "world-host-connected",
            r#"{"event":"world-host-connecting"}"#,
            r#"{"message":"world-host-connected"}"#,
            r#"["world-host-connected"]"#,
            r#"{"event":"build-finished","note":"world-host-connected"}"#,
        ] {
            assert!(!world_host_ready(line), "{line}");
        }
    }

    #[test]
    fn the_world_build_cache_is_lent_to_a_session_and_survives_it_and_the_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().canonicalize().unwrap();
        let session = work.join("engine/sessions/abc");
        let work_dir = session.join("world");
        private_dir(&work_dir).unwrap();
        // First session: a fresh cache.
        lend_world_builds(&work, &work_dir).unwrap();
        std::fs::write(work_dir.join(WORLD_BUILDS).join("player.zip"), "built").unwrap();
        assert!(!world_builds(&work).exists());
        assert!(return_world_builds(&work, &session));
        assert_eq!(
            std::fs::read_to_string(world_builds(&work).join("player.zip")).unwrap(),
            "built"
        );
        // The next session borrows it; a node that dies mid-session gets it
        // back in the startup sweep, which removes the session directory.
        let next = work.join("engine/sessions/def");
        private_dir(&next.join("world")).unwrap();
        lend_world_builds(&work, &next.join("world")).unwrap();
        assert!(
            next.join("world")
                .join(WORLD_BUILDS)
                .join("player.zip")
                .is_file()
        );
        let private = work.join("private");
        private_dir(&private).unwrap();
        sweep(&work, &private).unwrap();
        assert!(!next.exists());
        assert!(world_builds(&work).join("player.zip").is_file());
    }
}
