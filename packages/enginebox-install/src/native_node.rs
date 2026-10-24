//! Outbound-only native GPU host enrollment and supervision. Controller identity
//! is pinned to one HTTPS origin. No remotely supplied commands are executed.
use crate::release::{atomic_json, private_dir, reject_symlink_components, sha256_file};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::OpenOptions,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Profile {
    AppleVideotoolbox,
    NvidiaNvenc,
}

pub fn controller_origin(value: &str, test_http: bool) -> Result<Url> {
    let url = Url::parse(value).context("Controller must be an HTTPS origin")?;
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "localhost"));
    ensure!(
        url.scheme() == "https" || (test_http && loopback && url.scheme() == "http"),
        "Controller requires HTTPS (HTTP is restricted to explicit loopback tests)"
    );
    ensure!(
        url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none(),
        "Controller must be a bare origin without userinfo, path, query or fragment"
    );
    Ok(url)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Worker {
    pub executable: PathBuf,
    pub sha256: String,
    pub args: Vec<String>,
    pub directory: PathBuf,
}
impl Worker {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.executable.is_absolute() && self.directory.is_absolute(),
            "Worker paths must be absolute"
        );
        reject_symlink_components(&self.executable)?;
        reject_symlink_components(&self.directory)?;
        ensure!(
            self.directory.is_dir()
                && self.args.len() <= 128
                && self
                    .args
                    .iter()
                    .all(|a| a.len() <= 4096 && !a.contains('\0')),
            "Invalid worker configuration"
        );
        ensure!(
            sha256_file(&self.executable)? == self.sha256,
            "Local worker executable changed; review and repin its SHA-256"
        );
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema: u32,
    pub controller: String,
    pub id: String,
    pub token: String,
    pub enrolled: bool,
    pub profile: Profile,
    pub ffmpeg: PathBuf,
    pub worker: Option<Worker>,
}

fn client() -> Result<Client> {
    Ok(Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(8))
        .build()?)
}
async fn post(
    client: &Client,
    origin: &Url,
    path: &str,
    token: &str,
    session: Option<(&str, u64)>,
    body: Value,
) -> Result<Value> {
    let mut request = client
        .post(origin.join(path)?)
        .bearer_auth(token)
        .json(&body);
    if let Some((session, epoch)) = session {
        request = request
            .header("x-enginebox-session", session)
            .header("x-enginebox-epoch", epoch);
    }
    let mut response = request.send().await.context("Controller request failed")?;
    // Never echo remote text: it may contain secrets or attacker-controlled logs.
    ensure!(
        response.status().is_success(),
        "Controller returned HTTP {}",
        response.status().as_u16()
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            bytes.len() + chunk.len() <= 256 * 1024,
            "Controller response exceeds limit"
        );
        bytes.extend(chunk);
    }
    serde_json::from_slice(&bytes).context("Invalid controller response")
}

pub async fn enroll(
    path: &Path,
    controller: &str,
    enrollment: &str,
    profile: Profile,
    ffmpeg: PathBuf,
    worker: Option<Worker>,
    test_http: bool,
) -> Result<Value> {
    let origin = controller_origin(controller, test_http)?;
    let parts: Vec<_> = enrollment.split('.').collect();
    ensure!(
        parts.len() == 3
            && parts[1] == "enroll"
            && parts[2].len() == 64
            && parts[2].bytes().all(|c| c.is_ascii_hexdigit()),
        "Invalid enrollment credential"
    );
    let id = parts[0];
    ensure!(
        !id.is_empty()
            && id.len() <= 48
            && id
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-'),
        "Invalid node identifier"
    );
    if let Some(w) = &worker {
        w.validate()?;
    }
    ensure!(
        ffmpeg.is_absolute() && ffmpeg.is_file(),
        "FFmpeg must be an installed absolute executable path"
    );
    let parent = path
        .parent()
        .context("Configuration needs a parent directory")?;
    private_dir(parent)?;
    reject_symlink_components(path)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(parent.join("enroll.lock"))?;
    lock.try_lock_exclusive()
        .context("Another enrollment is running")?;
    let mut config = if path.exists() {
        let c: Config = serde_json::from_slice(&std::fs::read(path)?)?;
        ensure!(
            !c.enrolled && c.id == id && c.controller == origin.as_str(),
            "Configuration already belongs to an enrolled node or another controller"
        );
        c
    } else {
        Config {
            schema: 1,
            controller: origin.to_string(),
            id: id.to_string(),
            token: format!(
                "{id}.{}{}",
                Uuid::new_v4().simple(),
                Uuid::new_v4().simple()
            ),
            enrolled: false,
            profile,
            ffmpeg,
            worker,
        }
    };
    // Persist identity BEFORE exchanging the one-use credential so retries cannot
    // create a different identity after a lost HTTP response. Invitation is never saved.
    atomic_json(path, &config)?;
    let result = post(
        &client()?,
        &origin,
        "/v1/node/enroll",
        enrollment,
        None,
        json!({"nodeSecretHash": hex::encode(Sha256::digest(config.token.as_bytes()))}),
    )
    .await?;
    ensure!(
        result["node"]["id"] == id && result["node"]["role"] == "native",
        "Controller returned an unexpected node identity"
    );
    config.enrolled = true;
    atomic_json(path, &config)?;
    Ok(json!({"id": id, "controller": config.controller, "enrolled": true}))
}

pub fn encoder_args(profile: Profile) -> Vec<&'static str> {
    match profile {
        Profile::AppleVideotoolbox => vec![
            "-c:v",
            "h264_videotoolbox",
            "-allow_sw",
            "0",
            "-realtime",
            "1",
        ],
        Profile::NvidiaNvenc => vec!["-c:v", "h264_nvenc", "-preset", "p4", "-tune", "ull"],
    }
}

async fn bounded_command(command: &mut Command, seconds: u64) -> Result<std::process::Output> {
    command
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // These are fixed local probes with bounded frame counts, never arbitrary commands.
    Ok(
        tokio::time::timeout(Duration::from_secs(seconds), command.output())
            .await
            .context("Prerequisite probe timed out")??,
    )
}

pub async fn inspect(profile: Profile, ffmpeg: &Path, qualify: bool) -> Result<Value> {
    ensure!(
        ffmpeg.is_absolute() && ffmpeg.is_file(),
        "FFmpeg executable is missing; install FFmpeg and pass its absolute path"
    );
    match profile {
        Profile::AppleVideotoolbox => ensure!(
            cfg!(target_os = "macos"),
            "VideoToolbox requires native macOS"
        ),
        Profile::NvidiaNvenc => ensure!(
            cfg!(target_os = "windows") || cfg!(target_os = "linux"),
            "NVENC profile requires Windows or Linux"
        ),
    }
    let listing =
        bounded_command(Command::new(ffmpeg).args(["-hide_banner", "-encoders"]), 15).await?;
    let encoder = match profile {
        Profile::AppleVideotoolbox => "h264_videotoolbox",
        Profile::NvidiaNvenc => "h264_nvenc",
    };
    ensure!(
        listing.status.success() && String::from_utf8_lossy(&listing.stdout).contains(encoder),
        "This FFmpeg build lacks the selected hardware encoder"
    );
    let mut report = json!({"schema":1,"os":std::env::consts::OS,"architecture":std::env::consts::ARCH,"profile":profile,"encoder":encoder,"encoderListed":true,"hardwareEncodePassed":false,"decodePassed":false,"streamingQualified":false,"virtualMachineRequired":false});
    if matches!(profile, Profile::NvidiaNvenc) {
        let gpu = bounded_command(
            Command::new("nvidia-smi")
                .args(["--query-gpu=name,driver_version", "--format=csv,noheader"]),
            15,
        )
        .await
        .context("NVIDIA driver or nvidia-smi missing")?;
        ensure!(gpu.status.success(), "NVIDIA driver inspection failed");
        report["gpu"] = String::from_utf8_lossy(&gpu.stdout).trim().into();
    }
    if qualify {
        let scratch = tempfile::tempdir()?;
        let output = scratch.path().join("hardware.h264");
        let mut encode = Command::new(ffmpeg);
        encode
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=1280x720:rate=30",
                "-frames:v",
                "60",
                "-an",
            ])
            .args(encoder_args(profile))
            .args([
                "-pix_fmt", "yuv420p", "-bf", "0", "-g", "30", "-b:v", "3M", "-f", "h264",
            ])
            .arg(&output);
        let begin = Instant::now();
        let encoded = bounded_command(&mut encode, 45).await?;
        ensure!(
            encoded.status.success() && output.metadata().is_ok_and(|m| m.len() > 0),
            "Hardware H.264 encode failed (software fallback is disabled)"
        );
        report["encodeElapsedMs"] = (begin.elapsed().as_secs_f64() * 1000.0).into();
        report["encodedBytes"] = output.metadata()?.len().into();
        report["hardwareEncodePassed"] = true.into();
        let decoded = bounded_command(
            Command::new(ffmpeg)
                .args(["-hide_banner", "-v", "error", "-xerror", "-i"])
                .arg(&output)
                .args(["-f", "framemd5", "-"]),
            45,
        )
        .await?;
        let frames = String::from_utf8_lossy(&decoded.stdout)
            .lines()
            .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
            .count();
        ensure!(
            decoded.status.success() && frames == 60,
            "Hardware output failed the 60-frame decode check"
        );
        report["decodedFrames"] = frames.into();
        report["decodePassed"] = true.into();
    }
    Ok(report)
}

async fn stop_worker(child: &mut Option<Child>) {
    if let Some(mut process) = child.take() {
        #[cfg(unix)]
        if let Some(pid) = process.id() {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGTERM);
            }
            let _ = tokio::time::timeout(Duration::from_secs(3), process.wait()).await;
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        let _ = process.kill().await;
        let _ = process.wait().await;
    }
}

/// Windows owns one job for this supervisor and all its descendants. Keeping the
/// handle open until process exit also cleans workers when Task Scheduler kills
/// the supervisor (where Rust destructors cannot run). No breakaway is allowed.
#[cfg(windows)]
fn contain_windows_process_tree() -> Result<()> {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::{
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            },
            Threading::GetCurrentProcess,
        },
    };
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        ensure!(!job.is_null(), "Cannot create Windows node job");
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of_val(&limits) as u32,
        );
        if configured == 0 || AssignProcessToJobObject(job, GetCurrentProcess()) == 0 {
            CloseHandle(job);
            bail!("Cannot contain Windows node process tree; no worker was started");
        }
        // Deliberately held by the OS until this process exits.
    }
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut term) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

pub async fn run(path: &Path, once: bool, test_http: bool) -> Result<()> {
    #[cfg(windows)]
    contain_windows_process_tree()?;
    reject_symlink_components(path)?;
    let config: Config = serde_json::from_slice(&std::fs::read(path)?)?;
    ensure!(
        config.schema == 1
            && config.enrolled
            && config.token.starts_with(&format!("{}.", config.id)),
        "Node enrollment incomplete"
    );
    let parent = path.parent().context("Configuration parent missing")?;
    private_dir(parent)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(parent.join("node.lock"))?;
    lock.try_lock_exclusive()
        .context("This node already has a running supervisor")?;
    let origin = controller_origin(&config.controller, test_http)?;
    let client = client()?;
    let session = Uuid::new_v4().to_string();
    let qualification = inspect(config.profile, &config.ffmpeg, true).await?;
    atomic_json(&parent.join("qualification.json"), &qualification)?;
    let mut child = None;
    let result: Result<()> = async {
        let identity = post(&client, &origin, "/v1/node/hello", &config.token, None, json!({"session":session})).await?;
        let epoch = identity["epoch"].as_u64().context("Invalid controller epoch")?;
        ensure!(epoch > 0 && identity["session"] == session, "Controller session mismatch");
        if let Some(worker) = &config.worker {
            worker.validate()?;
            let mut command = Command::new(&worker.executable);
            command.args(&worker.args).current_dir(&worker.directory)
                .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true);
            #[cfg(unix)]
            command.process_group(0);
            child = Some(command.spawn().context("Native worker failed to start")?);
        }
        println!("{}", json!({"event":"node-online","id":config.id,"profile":config.profile,"epoch":epoch}));
        loop {
            if let Some(process) = child.as_mut() { ensure!(process.try_wait()?.is_none(), "Native worker exited"); }
            post(&client, &origin, "/v1/node/heartbeat", &config.token, Some((&session, epoch)), json!({"stats":{"nativeSupervisor":1,"hardwareEncodePassed":1,"streamingQualified":0,"workerRunning":u8::from(child.is_some())}})).await?;
            atomic_json(&parent.join("status.json"), &json!({"online":true,"id":config.id,"epoch":epoch,"workerRunning":child.is_some(),"lastHeartbeatUnixSeconds":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs()}))?;
            if once { return Ok(()); }
            tokio::select! {
                _ = shutdown_signal() => return Ok(()),
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
        }
    }.await;
    // Fail closed on revocation, a stale epoch, timeout or lost controller.
    stop_worker(&mut child).await;
    atomic_json(
        &parent.join("status.json"),
        &json!({"online":false,"id":config.id,"workerRunning":false,"cleanExit":result.is_ok()}),
    )?;
    result
}

pub async fn read_enrollment_stdin() -> Result<String> {
    let mut bytes = Vec::new();
    tokio::io::stdin().take(512).read_to_end(&mut bytes).await?;
    if bytes.len() >= 512 {
        bail!("Enrollment input exceeds limit");
    }
    Ok(String::from_utf8(bytes)?.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn controller_is_an_exact_secure_origin() {
        assert!(controller_origin("https://controller.example", false).is_ok());
        for value in [
            "http://controller.example",
            "https://user:secret@controller.example",
            "https://controller.example/path",
            "https://controller.example?token=secret",
            "https://controller.example/#x",
        ] {
            assert!(controller_origin(value, false).is_err(), "{value}");
        }
        assert!(controller_origin("http://127.0.0.1:9999", true).is_ok());
        assert!(controller_origin("http://example.com", true).is_err());
    }
    #[test]
    fn hardware_profiles_never_fallback_to_software() {
        assert!(
            encoder_args(Profile::AppleVideotoolbox)
                .windows(2)
                .any(|v| v == ["-allow_sw", "0"])
        );
        assert!(encoder_args(Profile::NvidiaNvenc).contains(&"h264_nvenc"));
    }
    #[test]
    fn worker_digest_is_rechecked_before_execution() {
        let dir = tempfile::tempdir().unwrap();
        let directory = dir.path().canonicalize().unwrap();
        let executable = directory.join("worker");
        std::fs::write(&executable, "first").unwrap();
        let worker = Worker {
            sha256: sha256_file(&executable).unwrap(),
            executable: executable.clone(),
            args: vec![],
            directory,
        };
        assert!(worker.validate().is_ok());
        std::fs::write(executable, "changed").unwrap();
        assert!(worker.validate().is_err());
    }

    #[tokio::test]
    async fn enrollment_persists_identity_before_network_and_retries_without_changing_it() {
        use axum::{Json, Router, routing::post};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/v1/node/enroll",
                    post(|| async { Json(json!({"node":{"id":"fixture","role":"native"}})) }),
                ),
            )
            .await
            .unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let config = root.join("private/node.json");
        let ffmpeg = root.join("ffmpeg");
        std::fs::write(&ffmpeg, "fixture").unwrap();
        let invitation = format!("fixture.enroll.{}", "a".repeat(64));
        enroll(
            &config,
            &origin,
            &invitation,
            Profile::NvidiaNvenc,
            ffmpeg.clone(),
            None,
            true,
        )
        .await
        .unwrap();
        let first: Config = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
        assert!(first.enrolled);
        assert!(
            !std::fs::read_to_string(&config)
                .unwrap()
                .contains(&invitation)
        );
        // Simulate a lost HTTP response after the controller committed enrollment.
        let mut pending = first;
        pending.enrolled = false;
        atomic_json(&config, &pending).unwrap();
        enroll(
            &config,
            &origin,
            &invitation,
            Profile::NvidiaNvenc,
            ffmpeg.clone(),
            None,
            true,
        )
        .await
        .unwrap();
        let retried: Config = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
        assert_eq!(pending.token, retried.token);
        assert!(
            enroll(
                &config,
                &origin,
                &invitation,
                Profile::NvidiaNvenc,
                ffmpeg,
                None,
                true
            )
            .await
            .is_err()
        );
        server.abort();
    }

    #[tokio::test]
    async fn controller_redirects_and_large_responses_are_rejected() {
        use axum::{Router, response::Redirect, routing::post};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route(
                        "/redirect",
                        post(|| async { Redirect::temporary("/must-not-follow") }),
                    )
                    .route("/large", post(|| async { "x".repeat(300 * 1024) })),
            )
            .await
            .unwrap();
        });
        for path in ["/redirect", "/large"] {
            assert!(
                super::post(
                    &client().unwrap(),
                    &origin,
                    path,
                    "fixture-secret",
                    None,
                    json!({})
                )
                .await
                .is_err()
            );
        }
        server.abort();
    }
}
