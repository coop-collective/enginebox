//! Explicit process-level qualification, excluded from routine offline tests.
//! First build enginebox-controller and enginebox-fleet-mcp; see docs/UPDATES.md.
//! cargo test -p enginebox-install --test real_process_updates -- --ignored --nocapture
//! Uses only temporary private state and fake local SSH/provider commands; no fleet host is contacted.
#![cfg(unix)]
use anyhow::{Context, Result, ensure};
use enginebox_install::{
    release::{Installation, Manifest, atomic_json, current_target, private_dir},
    supervisor::{Candidate, Supervisor, SupervisorConfig},
};
use fs2::FileExt;
use semver::Version;
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::time::{Instant, sleep};
const TOKEN: &str = "local-qualification-api-token-32-characters-only";
const PASSWORD: &str = "local-qualification-password-only";
const OLD: &str = "0.3.0";
const NEW: &str = "0.3.1-test.1";
const JOB: &str = "1234567890abcdef1234567890abcdef";
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn executable(path: &Path, content: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, content)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
fn port() -> Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}
fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}
fn copy_source(source: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        if ["target", ".state", "node_modules", ".git"].contains(&name.to_string_lossy().as_ref()) {
            continue;
        }
        ensure!(!path.is_symlink(), "unexpected source symlink");
        if path.is_dir() {
            copy_source(&path, &dest.join(name))?;
        } else {
            fs::copy(path, dest.join(name))?;
        }
    }
    Ok(())
}
fn second_controller(repo: &Path) -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("ENGINEBOX_TEST_CONTROLLER_NEW") {
        return Ok(path.into());
    }
    let source = repo.join(".state/update-fixture-source");
    // Recreate only the exact task-owned snapshot, never the root workspace.
    if source.exists() {
        ensure!(
            source.join(".enginebox-update-fixture").is_file(),
            "fixture snapshot is not task-owned"
        );
        fs::remove_dir_all(&source)?;
    }
    fs::create_dir_all(source.join("packages"))?;
    fs::write(
        source.join(".enginebox-update-fixture"),
        "isolated update qualification\n",
    )?;
    let root_manifest = fs::read_to_string(repo.join("Cargo.toml"))?;
    let changed = root_manifest.replacen(
        &format!("version = \"{OLD}\""),
        &format!("version = \"{NEW}\""),
        1,
    );
    ensure!(
        changed != root_manifest,
        "could not set isolated fixture version"
    );
    fs::write(source.join("Cargo.toml"), changed)?;
    for file in [
        "Cargo.lock",
        "LICENSE",
        "NOTICE",
        "LICENSE-APACHE-2.0",
        "LICENSE-SUSTAINABLE-USE-1.0",
    ] {
        fs::copy(repo.join(file), source.join(file))?;
    }
    for package in ["enginebox-core", "enginebox-agents", "enginebox-controller"] {
        copy_source(
            &repo.join("packages").join(package),
            &source.join("packages").join(package),
        )?;
    }
    let vendor = repo.join(".pnpm/crates/crates-io");
    if vendor.exists() {
        fs::create_dir(source.join(".cargo"))?;
        fs::write(
            source.join(".cargo/config.toml"),
            format!(
                "[source.crates-io]\nreplace-with = \"fixture-pnpm\"\n[source.fixture-pnpm]\ndirectory = {}\n",
                serde_json::to_string(&vendor.to_string_lossy())?
            ),
        )?;
    }
    let target = repo.join(".state/update-fixture-target");
    let output = Command::new("cargo")
        .current_dir(&source)
        .args([
            "build",
            "--offline",
            "-p",
            "enginebox-controller",
            "--bin",
            "enginebox-controller",
            "--target-dir",
        ])
        .arg(&target)
        .output()?;
    fs::write(
        repo.join(".state/update-fixture-build.log"),
        [output.stdout, output.stderr].concat(),
    )?;
    ensure!(
        output.status.success(),
        "fixture build failed; inspect .state/update-fixture-build.log"
    );
    Ok(target.join("debug/enginebox-controller"))
}
fn fixture_install(root: &Path) -> Result<Installation> {
    let install = Installation::inspect(root.to_path_buf())?;
    install.initialize()?;
    Ok(install)
}
fn release(
    install: &Installation,
    version: &str,
    binary: &Path,
    dashboard: Option<&Path>,
    repo: &Path,
) -> Result<()> {
    let version = Version::parse(version)?;
    let directory = install.release_dir(&version);
    fs::create_dir_all(directory.join("bin"))?;
    fs::copy(binary, directory.join("bin/enginebox-controller"))?;
    if let Some(source) = dashboard {
        let destination = directory.join("dashboard");
        fs::create_dir(&destination)?;
        let result = Command::new("cp")
            .arg("-R")
            .arg(source.join("."))
            .arg(&destination)
            .status()?;
        ensure!(result.success(), "copy fixture dashboard");
        fs::create_dir_all(destination.join("scripts"))?;
        fs::copy(
            repo.join("packages/dashboard/scripts/start.mjs"),
            destination.join("scripts/start.mjs"),
        )?;
    }
    atomic_json(
        &directory.join("manifest.json"),
        &Manifest {
            version,
            target: current_target().into(),
            archive_sha256: "0".repeat(64),
            state_schema: 1,
            read_schema_min: 1,
            read_schema_max: 1,
            dashboard: dashboard.is_some(),
        },
    )?;
    Ok(())
}
fn seed_state(state: &Path) -> Result<()> {
    private_dir(&state.join("jobs"))?;
    atomic_json(
        &state.join(format!("jobs/{JOB}.json")),
        &json!({"id":JOB,"status":"succeeded","host_id":"fixture","operation":"qualification","name":"ebox-fixture","result":{"proof":"durable-result-before-upgrade"}}),
    )?;
    for directory in [
        "agents",
        "agents/homes",
        "agents/homes/claude",
        "agents/homes/claude/.claude",
    ] {
        private_dir(&state.join(directory))?;
    }
    atomic_json(
        &state.join("agents/homes/claude/.claude/scratchnotes.json"),
        &json!({"tenant":"fixture-owner","memory":"preserve across versions"}),
    )?;
    Ok(())
}
fn fake_path(root: &Path) -> Result<String> {
    let bin = root.join("fake-bin");
    fs::create_dir_all(&bin)?;
    executable(&bin.join("ssh"), "#!/bin/sh\nexit 70\n")?;
    for (name, version) in [
        ("codex", "codex-cli 0.163.0"),
        ("claude", "2.1.302 (Claude Code)"),
        ("agent", "2026.10.16-e3cb207"),
    ] {
        executable(
            &bin.join(name),
            &format!(
                "#!/bin/sh\nif [ \"$1\" = --version ]; then printf '%s\\n' '{version}'; else exit 71; fi\n"
            ),
        )?;
    }
    Ok(format!("{}:{}", bin.display(), std::env::var("PATH")?))
}
fn fleet_config(root: &Path) -> Result<PathBuf> {
    let config = root.join("fleet.toml");
    fs::write(
        &config,
        "schema_version=1\n[[hosts]]\nid='fixture'\ntarget='enginebox-upgrade-fixture.invalid'\n",
    )?;
    Ok(config)
}
fn token_file(root: &Path) -> Result<PathBuf> {
    let path = root.join("api-token");
    fs::write(&path, TOKEN)?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    Ok(path)
}
fn spawn_supervisor(config: &Path, path: &str, log: &Path) -> Result<ChildGuard> {
    let log = File::create(log)?;
    Ok(ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_enginebox-supervisor"))
            .arg("--config")
            .arg(config)
            .env("PATH", path)
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()?,
    ))
}
fn spawn_controller(
    binary: &Path,
    config: &Path,
    state: &Path,
    port: u16,
    path: &str,
    log: &Path,
) -> Result<ChildGuard> {
    let log = File::create(log)?;
    Ok(ChildGuard(
        Command::new(binary)
            .args(["--config"])
            .arg(config)
            .args(["--listen", &format!("127.0.0.1:{port}")])
            .env("ENGINEBOX_STATE_DIR", state)
            .env("ENGINEBOX_START_STANDBY", "1")
            .env("ENGINEBOX_API_TOKEN", TOKEN)
            .env_remove("ENGINEBOX_API_TOKEN_FILE")
            .env_remove("ENGINEBOX_SUPERVISOR_LIFELINE")
            .env_remove("ENGINEBOX_SUPERVISOR_LIFELINE_TOKEN")
            .env("PATH", path)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()?,
    ))
}
async fn ready(client: &reqwest::Client, url: &str, version: &str) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(response) = client
            .get(format!("{url}/readyz"))
            .timeout(Duration::from_secs(2))
            .send()
            .await
            && let Ok(value) = response.json::<Value>().await
            && value["ready"] == true
            && value["version"] == version
        {
            return Ok(value);
        }
        ensure!(
            Instant::now() < deadline,
            "readiness timed out at {url}, expected {version}"
        );
        sleep(Duration::from_millis(50)).await;
    }
}
fn template() -> Value {
    json!({"schema_version":1,"id":"upgrade-fixture","revision":1,"label":"Disposable upgrade fixture","minimum":{"cpu":2,"memory_gib":2,"disk_gib":30},"recommended":{"cpu":2,"memory_gib":2,"disk_gib":30}})
}
async fn control(
    client: &reqwest::Client,
    url: &str,
    action: &str,
    body: Value,
) -> Result<reqwest::Response> {
    Ok(client
        .post(format!("{url}/__enginebox/{action}"))
        .bearer_auth(TOKEN)
        .json(&body)
        .send()
        .await?)
}
async fn state_proofs(client: &reqwest::Client, url: &str) -> Result<()> {
    let job: Value = client
        .get(format!("{url}/v1/jobs/{JOB}"))
        .bearer_auth(TOKEN)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    ensure!(
        job["job"]["status"] == "succeeded"
            && job["job"]["result"]["proof"] == "durable-result-before-upgrade",
        "durable completed job changed"
    );
    let catalog: Value = client
        .get(format!("{url}/v1/templates"))
        .bearer_auth(TOKEN)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    ensure!(
        catalog["templates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["id"] == "upgrade-fixture"),
        "template did not survive handoff"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "builds an isolated second controller version and launches disposable real processes"]
async fn real_versioned_controller_updates_and_rollback() -> Result<()> {
    let repo = repository();
    fs::create_dir_all(repo.join(".state"))?;
    let old = std::env::var_os("ENGINEBOX_TEST_CONTROLLER_OLD")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo.join("target/debug/enginebox-controller"));
    ensure!(
        old.is_file(),
        "build enginebox-controller before this qualification"
    );
    let new = tokio::task::spawn_blocking({
        let repo = repo.clone();
        move || second_controller(&repo)
    })
    .await??;
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let install = fixture_install(&root.join("installation"))?;
    let state = root.join("preserved-controller-state");
    private_dir(&state)?;
    seed_state(&state)?;
    let path = fake_path(&root)?;
    let fleet = fleet_config(&root)?;
    let token = token_file(&root)?;
    let dashboard = repo.join("packages/dashboard/dist/standalone");
    let with_ui = dashboard.join("dist/server/index.js").is_file();
    release(
        &install,
        OLD,
        &old,
        with_ui.then_some(dashboard.as_path()),
        &repo,
    )?;
    release(
        &install,
        NEW,
        &new,
        with_ui.then_some(dashboard.as_path()),
        &repo,
    )?;
    release(&install, "0.3.2-test.1", &old, None, &repo)?;
    // Genuine old hash-named public asset only in the prior dashboard artifact.
    if with_ui {
        let asset = install
            .release_dir(&Version::parse(OLD)?)
            .join("dashboard/dist/client/_next/static/chunks/qualification-abcdefgh.js");
        fs::write(asset, "window.oldReleaseQualification=true;")?;
    }
    install.activate(&Version::parse(OLD)?)?;
    let api_port = port()?;
    let ui_port = port()?;
    let url = format!("http://127.0.0.1:{api_port}");
    let ui = format!("http://127.0.0.1:{ui_port}");
    let environment = root.join("environment.json");
    atomic_json(
        &environment,
        &json!({"ENGINEBOX_DASHBOARD_PASSWORD":PASSWORD,"ENGINEBOX_SESSION_SECRET":"local-qualification-browser-session-secret-32-characters","ENGINEBOX_DASHBOARD_ORIGIN":ui,"PATH":path}),
    )?;
    let config = root.join("supervisor.json");
    let mut value = json!({"prefix":install.root,"state_dir":state,"fleet_config":fleet,"token_file":token,"environment_file":environment,"listen":format!("127.0.0.1:{api_port}"),"drain_timeout_seconds":10});
    if with_ui {
        value["dashboard_listen"] = json!(format!("127.0.0.1:{ui_port}"));
    }
    atomic_json(&config, &value)?;
    let mut service = spawn_supervisor(&config, &path, &root.join("supervisor.log"))?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    ensure!(
        ready(&client, &url, OLD).await?["mode"] == "active",
        "initial controller not active"
    );
    let owner = File::options()
        .read(true)
        .write(true)
        .open(state.join("controller-owner.lock"))?;
    ensure!(
        !install.root.join("state/controller-owner.lock").exists(),
        "controller unexpectedly used an empty installation state root"
    );
    ensure!(
        owner.try_lock_exclusive().is_err(),
        "controller did not hold exclusive ownership"
    );
    client
        .post(format!("{url}/v1/templates"))
        .bearer_auth(TOKEN)
        .json(&json!({"manifest":template()}))
        .send()
        .await?
        .error_for_status()?;
    let mut mcp = McpSession::start(&repo, &url, &token).await?;
    let cookie = if with_ui {
        let response = client
            .post(format!("{ui}/api/session"))
            .header("origin", &ui)
            .json(&json!({"password":PASSWORD}))
            .send()
            .await?
            .error_for_status()?;
        Some(
            response
                .headers()
                .get("set-cookie")
                .context("missing session cookie")?
                .to_str()?
                .split(';')
                .next()
                .unwrap()
                .to_owned(),
        )
    } else {
        None
    };
    let failed = control(&client, &url, "update", json!({"version":"0.3.2-test.1"})).await?;
    ensure!(
        failed.status() == reqwest::StatusCode::CONFLICT,
        "mismatched candidate version was accepted"
    );
    ensure!(
        ready(&client, &url, OLD).await?["mode"] == "active",
        "old controller lost ownership after failed health check"
    );
    let running = Arc::new(AtomicBool::new(true));
    let reads = Arc::new(AtomicUsize::new(0));
    let writes = Arc::new(AtomicUsize::new(0));
    let errors = Arc::new(MutexErrors::default());
    let mut probes = vec![];
    for is_write in [false, true] {
        let (client, url, running, reads, writes, errors) = (
            client.clone(),
            url.clone(),
            running.clone(),
            reads.clone(),
            writes.clone(),
            errors.clone(),
        );
        probes.push(tokio::spawn(async move {
            while running.load(Ordering::SeqCst) {
                let request = if is_write {
                    client
                        .post(format!("{url}/v1/templates"))
                        .bearer_auth(TOKEN)
                        .json(&json!({"manifest":template()}))
                } else {
                    client.get(format!("{url}/v1/templates")).bearer_auth(TOKEN)
                };
                match request.send().await {
                    Ok(response) if response.status().is_success() => {
                        if is_write {
                            writes.fetch_add(1, Ordering::SeqCst);
                        } else {
                            reads.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    Ok(response) => errors.add(format!("HTTP {}", response.status())),
                    Err(error) => errors.add(error.to_string()),
                }
                sleep(Duration::from_millis(5)).await;
            }
        }));
    }
    sleep(Duration::from_millis(100)).await;
    let updated = control(&client, &url, "update", json!({"version":NEW})).await?;
    let status = updated.status();
    let body: Value = updated.json().await?;
    ensure!(status.is_success(), "actual version cutover failed: {body}");
    ensure!(
        ready(&client, &url, NEW).await?["mode"] == "active",
        "new controller not active"
    );
    state_proofs(&client, &url).await?;
    mcp.catalog_proof(6).await?;
    ensure!(
        owner.try_lock_exclusive().is_err(),
        "new controller lacks exclusive state ownership"
    );
    if let Some(cookie) = &cookie {
        let session: Value = client
            .get(format!("{ui}/api/session"))
            .header("cookie", cookie)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            session["authenticated"] == true,
            "browser session lost during update"
        );
        let asset = client
            .get(format!(
                "{ui}/_next/static/chunks/qualification-abcdefgh.js"
            ))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        ensure!(
            asset == "window.oldReleaseQualification=true;",
            "old browser asset was lost"
        );
    }
    let rolled = control(&client, &url, "rollback", json!({})).await?;
    let status = rolled.status();
    let body: Value = rolled.json().await?;
    ensure!(status.is_success(), "rollback failed: {body}");
    ensure!(
        ready(&client, &url, OLD).await?["mode"] == "active",
        "rollback did not activate original version"
    );
    state_proofs(&client, &url).await?;
    mcp.catalog_proof(7).await?;
    running.store(false, Ordering::SeqCst);
    for probe in probes {
        probe.await?;
    }
    ensure!(
        errors.0.lock().unwrap().is_empty(),
        "requests failed during cutover: {:?}",
        errors.0.lock().unwrap()
    );
    ensure!(
        reads.load(Ordering::SeqCst) > 10 && writes.load(Ordering::SeqCst) > 10,
        "not enough continuity requests"
    );
    let scratch: Value = serde_json::from_slice(&fs::read(
        state.join("agents/homes/claude/.claude/scratchnotes.json"),
    )?)?;
    ensure!(
        scratch["memory"] == "preserve across versions",
        "private provider memory changed"
    );
    // Crash/restart is separately measured recovery, not part of the no-downtime assertion.
    service.0.kill()?;
    service.0.wait()?;
    service = spawn_supervisor(&config, &path, &root.join("supervisor-restart.log"))?;
    ensure!(
        ready(&client, &url, OLD).await?["mode"] == "active",
        "supervisor restart could not recover exclusive ownership"
    );
    state_proofs(&client, &url).await?;
    mcp.catalog_proof(8).await?;
    mcp.finish().await?;
    let receipt = json!({"old_version":OLD,"new_version":NEW,"get_requests":reads.load(Ordering::SeqCst),"post_requests":writes.load(Ordering::SeqCst),"request_errors":0,"dashboard_session_and_assets":with_ui,"failed_version_health_preserved_old":true,"durable_jobs_templates_memory_preserved":true,"external_state_root_preserved":true,"rollback":true,"supervisor_crash_recovery":true,"native_stdio_mcp_initialize_tools_catalog_register_and_resume":true});
    atomic_json(
        &repo.join(".state/rust-update-qualification.json"),
        &receipt,
    )?;
    println!("{}", serde_json::to_string_pretty(&receipt)?);
    drop(service);
    // Separate actual-controller activation failure: corrupt only the candidate's
    // private state, so the real handoff can restore the valid old owner's state.
    real_activation_failure(&root, &old, &new, &repo, &path, &client).await?;
    Ok(())
}
#[derive(Default)]
struct MutexErrors(std::sync::Mutex<Vec<String>>);
impl MutexErrors {
    fn add(&self, value: String) {
        self.0.lock().unwrap().push(value)
    }
}
async fn real_activation_failure(
    root: &Path,
    old: &Path,
    new: &Path,
    repo: &Path,
    path: &str,
    client: &reqwest::Client,
) -> Result<()> {
    let install = fixture_install(&root.join("failure-install"))?;
    let invalid = fixture_install(&root.join("failure-candidate"))?;
    let fleet = fleet_config(&install.root)?;
    release(&install, OLD, old, None, repo)?;
    release(&install, NEW, new, None, repo)?;
    install.activate(&Version::parse(OLD)?)?;
    atomic_json(
        &invalid.root.join("state/schema.json"),
        &json!({"state_schema":999}),
    )?;
    let old_port = port()?;
    let new_port = port()?;
    let old_url = format!("http://127.0.0.1:{old_port}");
    let new_url = format!("http://127.0.0.1:{new_port}");
    let _old = spawn_controller(
        old,
        &fleet,
        &install.root.join("state"),
        old_port,
        path,
        &root.join("failure-old.log"),
    )?;
    let _new = spawn_controller(
        new,
        &fleet,
        &invalid.root.join("state"),
        new_port,
        path,
        &root.join("failure-new.log"),
    )?;
    ready(client, &old_url, OLD).await?;
    ready(client, &new_url, NEW).await?;
    client
        .post(format!("{old_url}/internal/activate"))
        .bearer_auth(TOKEN)
        .send()
        .await?
        .error_for_status()?;
    let supervisor = Supervisor::new(
        SupervisorConfig {
            install: install.clone(),
            state_dir: None,
            fleet_config: fleet,
            api_listen: "127.0.0.1:8765".parse()?,
            dashboard_listen: None,
            token: TOKEN.into(),
            drain_timeout: Duration::from_secs(10),
        },
        Candidate::external(Version::parse(OLD)?, 1, old_url.clone(), None),
    )?;
    ensure!(
        supervisor
            .handoff(Candidate::external(Version::parse(NEW)?, 1, new_url, None))
            .await
            .is_err(),
        "invalid actual controller state activated"
    );
    ensure!(
        ready(client, &old_url, OLD).await?["mode"] == "active",
        "old actual controller did not recover after candidate activation failure"
    );
    ensure!(
        install.active()?.unwrap().current == Version::parse(OLD)?,
        "failed activation changed durable version"
    );
    println!("Actual candidate state-schema activation failure restored the old controller.");
    Ok(())
}

struct McpSession {
    child: tokio::process::Child,
    input: tokio::process::ChildStdin,
    output: tokio::io::BufReader<tokio::process::ChildStdout>,
}
impl McpSession {
    async fn start(repo: &Path, url: &str, token_file: &Path) -> Result<Self> {
        let binary = std::env::var_os("ENGINEBOX_TEST_FLEET_MCP")
            .map(PathBuf::from)
            .unwrap_or_else(|| repo.join("target/debug/enginebox-fleet-mcp"));
        ensure!(
            binary.is_file(),
            "build enginebox-fleet-mcp before real MCP qualification"
        );
        let mut child = tokio::process::Command::new(binary)
            .env("ENGINEBOX_API_URL", url)
            .env("ENGINEBOX_API_TOKEN_FILE", token_file)
            .env_remove("ENGINEBOX_GUEST")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let input = child.stdin.take().unwrap();
        let output = tokio::io::BufReader::new(child.stdout.take().unwrap());
        let mut session = Self {
            child,
            input,
            output,
        };
        let initialized=session.rpc(1,"initialize",json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"enginebox-real-qualification","version":"1"}})).await?;
        ensure!(
            initialized["protocolVersion"] == "2025-11-25"
                && initialized["capabilities"]["tools"].is_object(),
            "native MCP initialize negotiation failed"
        );
        session
            .send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .await?;
        let tools = session.rpc(2, "tools/list", json!({})).await?;
        for name in ["template_catalog", "template_register", "agent_launch"] {
            ensure!(
                tools["tools"]
                    .as_array()
                    .context("tools/list response")?
                    .iter()
                    .any(|t| t["name"] == name),
                "native MCP omitted {name}"
            );
        }
        let first = session
            .rpc(
                3,
                "tools/call",
                json!({"name":"template_catalog","arguments":{}}),
            )
            .await?;
        ensure!(
            first["isError"] == false && first["structuredContent"]["templates"].is_array(),
            "MCP authenticated catalog read failed"
        );
        let mut manifest = template();
        manifest["id"] = json!("mcp-upgrade-fixture");
        manifest["label"] = json!("Disposable MCP transport fixture");
        let registered = session
            .rpc(
                4,
                "tools/call",
                json!({"name":"template_register","arguments":{"manifest":manifest}}),
            )
            .await?;
        ensure!(
            registered["isError"] == false
                && registered["structuredContent"]["template"]["id"] == "mcp-upgrade-fixture",
            "MCP template registration did not reach controller state"
        );
        session.catalog_proof(5).await?;
        Ok(session)
    }
    async fn send(&mut self, value: Value) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let mut data = serde_json::to_vec(&value)?;
        data.push(b'\n');
        self.input.write_all(&data).await?;
        self.input.flush().await?;
        Ok(())
    }
    async fn rpc(&mut self, id: u64, method: &str, params: Value) -> Result<Value> {
        use tokio::io::AsyncBufReadExt;
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        let mut line = String::new();
        let length =
            tokio::time::timeout(Duration::from_secs(10), self.output.read_line(&mut line))
                .await??;
        ensure!(
            length > 0 && length < 1024 * 1024 && !line.contains(TOKEN),
            "invalid or unsafe native MCP stdio response"
        );
        let reply: Value = serde_json::from_str(&line)?;
        ensure!(
            reply["jsonrpc"] == "2.0" && reply["id"] == id && reply.get("error").is_none(),
            "native MCP transport response did not match request"
        );
        Ok(reply["result"].clone())
    }
    async fn catalog_proof(&mut self, id: u64) -> Result<()> {
        let result = self
            .rpc(
                id,
                "tools/call",
                json!({"name":"template_catalog","arguments":{}}),
            )
            .await?;
        ensure!(
            result["isError"] == false
                && result["structuredContent"]["templates"]
                    .as_array()
                    .context("MCP catalog response")?
                    .iter()
                    .any(|t| t["id"] == "mcp-upgrade-fixture"),
            "MCP registered template missing after version transition"
        );
        Ok(())
    }
    async fn finish(mut self) -> Result<()> {
        drop(self.input);
        let status = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await??;
        ensure!(status.success(), "native MCP did not close cleanly");
        Ok(())
    }
}
