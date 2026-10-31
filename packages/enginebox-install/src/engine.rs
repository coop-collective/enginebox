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
        Ok(())
    }
    fn git(&self) -> Result<PathBuf> {
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
            json!({"ready": true, "unity": c.unity.keys().collect::<Vec<_>>(), "running": running})
        }
        Err(e) => {
            let reason: String = format!("{e:#}").chars().take(300).collect();
            json!({"ready": false, "unity": [], "running": running, "error": reason})
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
    pub moq: MoqSecret,
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
    pub fn validate(&self) -> Result<()> {
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
pub fn git_plan(checkout: &Path, remote: &str, git_ref: &str, fresh: bool) -> Vec<Vec<OsString>> {
    let c = |args: &[&str]| -> Vec<OsString> {
        let mut v: Vec<OsString> = vec!["-C".into(), checkout.as_os_str().to_owned()];
        v.extend(args.iter().map(OsString::from));
        v
    };
    let mut plan = Vec::new();
    if fresh {
        plan.push(vec![
            "init".into(),
            "-q".into(),
            checkout.as_os_str().to_owned(),
        ]);
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
pub fn unity_argv(project: &Path, bridge_config: &Path, log: &Path) -> Vec<OsString> {
    vec![
        "-projectPath".into(),
        project.as_os_str().to_owned(),
        "-jam-editor-config".into(),
        bridge_config.as_os_str().to_owned(),
        "-enginebox-autoplay".into(),
        "-logFile".into(),
        log.as_os_str().to_owned(),
    ]
}
pub fn bridge_config(port: u16, token: &str, spec: &Spec) -> Value {
    json!({"port": port, "token": token, "scene": spec.scene.clone().unwrap_or_default(),
        "width": spec.width, "height": spec.height, "playOnly": true})
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
            "tokenFile": token_file, "ffmpeg": ffmpeg},
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
            if std::fs::remove_dir_all(&path).is_ok() {
                removed += 1;
            }
        }
    }
    Ok(json!({"killed": killed, "removed_sessions": removed}))
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
/// (checkout, commit).
pub async fn checkout(
    ctx: &SessionContext,
    spec: &Spec,
    access: &RepositorySecret,
    stop: &mut watch::Receiver<bool>,
) -> Result<(PathBuf, String)> {
    let git = ctx.config.git()?;
    let checkouts = ctx.work.join("engine").join("checkouts");
    private_dir(&checkouts)?;
    let dir = checkouts.join(checkout_key(&access.remote));
    let fresh = !dir.join(".git").is_dir();
    if fresh && dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    let env = git_env(access);
    let mut commit = String::new();
    for args in git_plan(&dir, &access.remote, &spec.git_ref, fresh) {
        let mut env = env.clone();
        if args.iter().any(|a| a == "checkout") {
            env.push(("GIT_LFS_SKIP_SMUDGE".into(), "1".into()));
        }
        run_git(&git, &args, &env, &access.token, stop).await?;
        if args.iter().any(|a| a == "fetch") {
            let mut rev: Vec<OsString> = vec!["-C".into(), dir.as_os_str().to_owned()];
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

/// Prepare the project: version check, packages, bridge configuration.
/// Returns (project, bridge config path, bridge token file, port).
pub fn prepare_project(
    ctx: &SessionContext,
    spec: &Spec,
    checkout: &Path,
    session_dir: &Path,
) -> Result<(PathBuf, PathBuf, PathBuf, u16)> {
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
    let package = session_dir.join("packages").join(SESSION_PACKAGE);
    write_session_package(&package)?;
    add_packages(
        &project.join("Packages").join("manifest.json"),
        &ctx.config.editor_bridge,
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

/// The bridge's `/status`, when it answers.
async fn bridge_status(port: u16, token: &str) -> Option<Value> {
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
        report("preparing", None, json!({"commit": commit}));
        let (project, bridge_json, token_file, port) =
            prepare_project(&ctx, &spec, &checkout, &session_dir)?;
        let bridge_token = std::fs::read_to_string(&token_file)?;
        let pids = PidRecord::new(ctx.private.join("engine-pids.json"));
        let mut g = Group::new()?;
        // Activation: the access token is the licensing client's only input.
        let t = Instant::now();
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
        ensure!(
            status.success(),
            "Unity could not activate a license with your Unity account (licensing client exit {}). Check the account on the Agents page.",
            status.code().unwrap_or(-1)
        );
        receipt["activation_seconds"] = json!(t.elapsed().as_secs());
        // The Editor, in this node's interactive session.
        let log = session_dir.join("unity.log");
        let mut editor = Command::new(&unity);
        editor
            .args(unity_argv(&project, &bridge_json, &log))
            .current_dir(&project)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let g = group.as_mut().unwrap();
        let editor_index = g.adopt(editor.spawn().context("Cannot start the Unity Editor")?, &pids)?;
        report("starting", None, json!({}));
        let t = Instant::now();
        let limit = Duration::from_secs(ctx.config.startup_timeout_seconds.unwrap_or(DEFAULT_STARTUP_SECONDS));
        loop {
            if let Some(status) = bridge_status(port, bridge_token.trim()).await
                && status["playing"] == true
                && status["view"] == "play"
            {
                break;
            }
            if let Some(exit) = g.child(editor_index).try_wait()? {
                bail!("The Unity Editor exited during startup (exit {}); see unity.log on the engine machine", exit.code().unwrap_or(-1));
            }
            ensure!(t.elapsed() < limit, "The Unity Editor did not enter Play mode in time");
            tokio::select! {
                _ = cancelled(&mut stop) => bail!("Stopped"),
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
        }
        receipt["editor_ready_seconds"] = json!(t.elapsed().as_secs());
        // The stream host, restarted while the session runs (its permit is fixed).
        let session_file = session_dir.join("moq-session.json");
        let mut ready = false;
        let mut quick_failures = 0;
        loop {
            std::fs::write(
                &session_file,
                serde_json::to_vec(&stream_session(&spec, &secrets.moq, port, &token_file, &ctx.config.ffmpeg))?,
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
            let _ = g.child(index).wait().await;
            if g.child(editor_index).try_wait()?.is_some() {
                bail!("The Unity Editor exited");
            }
            if launched.elapsed() < Duration::from_secs(30) {
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
    if let Some(g) = group.as_mut() {
        g.terminate().await;
    }
    // Keep the Editor log (its last MiB) beside the receipt for the owner;
    // secrets on disk go with the session directory.
    let receipts = ctx.private.join("engine-receipts");
    if private_dir(&receipts).is_ok()
        && let Ok(log) = std::fs::read(session_dir.join("unity.log"))
    {
        let tail = &log[log.len().saturating_sub(1 << 20)..];
        let _ = std::fs::write(receipts.join(format!("{id}.unity.log")), tail);
    }
    let _ = std::fs::remove_dir_all(&session_dir);
    let stopped = *stop.borrow();
    let error = match &outcome {
        Err(e) if !stopped => Some(redact(
            &format!("{e:#}"),
            &[
                &secrets.repository.token,
                &secrets.unity.access_token,
                &secrets.moq.publish_token,
                &secrets.moq.control_key,
            ],
        )),
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

/// The node side of the engine channel: running sessions and pending reports.
pub struct Executor {
    config_path: PathBuf,
    work: PathBuf,
    private: PathBuf,
    running: BTreeMap<String, (u64, watch::Sender<bool>)>,
    reports_tx: mpsc::UnboundedSender<Report>,
    reports_rx: mpsc::UnboundedReceiver<Report>,
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
        }
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
                ("run", Some((g, _))) if *g == generation => {}
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
                            let (tx, rx) = watch::channel(false);
                            self.running.insert(id.to_owned(), (generation, tx));
                            tokio::spawn(run_session(
                                std::sync::Arc::new(SessionContext {
                                    config,
                                    work: self.work.clone(),
                                    private: self.private.clone(),
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
            json!({"ready":true,"unity":["6000.6.5f1"],"running":0})
        );
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

    #[test]
    fn git_gets_the_token_only_through_scoped_config_env() {
        let s = secrets();
        let plan = git_plan(
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
                "init -q /work/engine/checkouts/x",
                "-C /work/engine/checkouts/x remote add origin https://github.com/coop-collective/queen-of-hearts.git",
                "-C /work/engine/checkouts/x lfs install --local --skip-smudge",
                "-C /work/engine/checkouts/x fetch --no-tags --depth=1 origin main",
                "-C /work/engine/checkouts/x checkout --detach -f FETCH_HEAD",
                "-C /work/engine/checkouts/x clean -ffdq",
                "-C /work/engine/checkouts/x lfs pull origin",
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
                "-projectPath",
                "/w/c/unity-project",
                "-jam-editor-config",
                "/w/s/bridge.json",
                "-enginebox-autoplay",
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
            &s.moq,
            4100,
            Path::new("/w/s/bridge.token"),
            Path::new("/ffmpeg.exe"),
        );
        assert_eq!(
            session,
            json!({"schema":1,"token":PUBLISH,"controlKey":"f".repeat(64),"session":"01234567-89ab-4def-8123-456789abcdef",
                "namespace":"eb-0123456789abcdef0123456789abcdef","tag":7,"durationSeconds":600,"width":1280,"height":720,
                "capture":{"kind":"unity-editor","origin":"http://127.0.0.1:4100","tokenFile":"/w/s/bridge.token","ffmpeg":"/ffmpeg.exe"}})
        );
        assert!(!session.to_string().contains(UNITY) && !session.to_string().contains(REPO));
        let bridge = bridge_config(4100, &"a".repeat(64), &spec);
        assert_eq!(
            bridge,
            json!({"port":4100,"token":"a".repeat(64),"scene":"Assets/_Project/Scenes/Prototype.unity","width":1280,"height":720,"playOnly":true})
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
        assert_eq!(
            manifest["dependencies"][BRIDGE_PACKAGE],
            file_url(&config.editor_bridge)
        );
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
}
