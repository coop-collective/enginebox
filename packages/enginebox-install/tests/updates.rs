use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use enginebox_install::{
    release::{Installation, Manifest, atomic_json, current_target, sha256_file},
    supervisor::{Candidate, Supervisor, SupervisorConfig},
};
use semver::Version;
use std::{
    fs::{self, File},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Mutex;

fn temp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}
fn installation(tmp: &tempfile::TempDir) -> Installation {
    let root = tmp.path().canonicalize().unwrap().join("enginebox");
    let install = Installation::inspect(root).unwrap();
    install.initialize().unwrap();
    install
}
fn manifest(version: &str, schema: u32) -> Manifest {
    Manifest {
        version: Version::parse(version).unwrap(),
        target: current_target().into(),
        archive_sha256: "0".repeat(64),
        state_schema: schema,
        read_schema_min: schema,
        read_schema_max: schema,
        dashboard: false,
    }
}
fn fixture_release(install: &Installation, version: &str, schema: u32) {
    let dir = install.release_dir(&Version::parse(version).unwrap());
    fs::create_dir_all(&dir).unwrap();
    atomic_json(&dir.join("manifest.json"), &manifest(version, schema)).unwrap();
}
#[test]
fn supervisor_state_selection_refuses_missing_or_overlapping_roots() {
    let tmp = temp();
    let install = installation(&tmp);
    let root = tmp.path().canonicalize().unwrap();
    let mut config = SupervisorConfig {
        install: install.clone(),
        state_dir: None,
        fleet_config: root.join("fleet.toml"),
        api_listen: "127.0.0.1:8765".parse().unwrap(),
        dashboard_listen: None,
        token: "private-fixture-token-longer-than-32-characters".into(),
        drain_timeout: Duration::from_secs(1),
    };
    assert_eq!(config.state_root().unwrap(), install.root.join("state"));
    for path in [
        root.join("missing-state"),
        install.root.clone(),
        install.root.join("releases"),
        root.clone(),
        "relative-state".into(),
    ] {
        config.state_dir = Some(path);
        assert!(config.state_root().is_err());
    }
    assert!(!root.join("missing-state").exists());
    let file = root.join("regular-file");
    fs::write(&file, "preserve me").unwrap();
    config.state_dir = Some(file.clone());
    assert!(config.state_root().is_err());
    assert_eq!(fs::read_to_string(file).unwrap(), "preserve me");
}

#[cfg(unix)]
#[test]
fn external_state_preserves_homes_and_refuses_symlinks_or_shared_permissions() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let tmp = temp();
    let install = installation(&tmp);
    let root = tmp.path().canonicalize().unwrap();
    let state = root.join("legacy-state");
    enginebox_install::release::private_dir(&state).unwrap();
    fs::write(state.join("memory"), "same private memory").unwrap();
    let mut config = SupervisorConfig {
        install,
        state_dir: Some(state.clone()),
        fleet_config: root.join("fleet.toml"),
        api_listen: "127.0.0.1:8765".parse().unwrap(),
        dashboard_listen: None,
        token: "private-fixture-token-longer-than-32-characters".into(),
        drain_timeout: Duration::from_secs(1),
    };
    assert_eq!(config.state_root().unwrap(), state);
    assert_eq!(
        fs::read_to_string(state.join("memory")).unwrap(),
        "same private memory"
    );
    fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(config.state_root().is_err());
    assert_eq!(
        fs::metadata(&state).unwrap().permissions().mode() & 0o777,
        0o755
    );
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    let alias = root.join("state-alias");
    symlink(&state, &alias).unwrap();
    config.state_dir = Some(alias);
    assert!(config.state_root().is_err());
    config.state_dir = Some(state.join("../legacy-state"));
    assert!(config.state_root().is_err());
}
fn archive(tmp: &Path, entries: &[(&str, &[u8], tar::EntryType)]) -> std::path::PathBuf {
    let path = tmp.join(format!("{}.tar.gz", uuid::Uuid::new_v4()));
    let encoder =
        flate2::write::GzEncoder::new(File::create(&path).unwrap(), flate2::Compression::default());
    let mut tar = tar::Builder::new(encoder);
    for (name, content, kind) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o755);
        header.set_entry_type(*kind);
        let bytes = header.as_mut_bytes();
        bytes[..100].fill(0);
        bytes[..name.len()].copy_from_slice(name.as_bytes());
        header.set_cksum();
        tar.append(&header, *content).unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap();
    path
}
fn good_archive(tmp: &Path) -> std::path::PathBuf {
    let names = if cfg!(windows) {
        vec![
            "bin/enginebox-controller.exe",
            "bin/enginebox-supervisor.exe",
            "bin/enginebox-install.exe",
            "bin/enginebox-host.exe",
            "bin/ebox.exe",
            "bin/enginebox-api.exe",
            "bin/enginebox-fleet-mcp.exe",
            "bin/enginebox-guest.exe",
            "bin/enginebox-mcp.exe",
            "bin/enginebox-browser.exe",
            "bin/enginebox-agent-clients.exe",
            "LICENSE",
            "NOTICE",
            "LICENSE-APACHE-2.0",
        ]
    } else {
        vec![
            "bin/enginebox-controller",
            "bin/enginebox-supervisor",
            "bin/enginebox-install",
            "bin/enginebox-host",
            "bin/ebox",
            "bin/enginebox-api",
            "bin/enginebox-fleet-mcp",
            "bin/enginebox-guest",
            "bin/enginebox-mcp",
            "bin/enginebox-browser",
            "bin/enginebox-agent-clients",
            "LICENSE",
            "NOTICE",
            "LICENSE-APACHE-2.0",
        ]
    };
    let entries: Vec<_> = names
        .iter()
        .map(|s| (*s, b"fixture" as &[u8], tar::EntryType::Regular))
        .collect();
    archive(tmp, &entries)
}
#[test]
fn staged_release_is_integrity_checked_and_immutable() {
    let tmp = temp();
    let install = installation(&tmp);
    let archive = good_archive(tmp.path());
    let mut value = manifest("0.3.0", 1);
    value.archive_sha256 = sha256_file(&archive).unwrap();
    let path = tmp.path().join("manifest.json");
    atomic_json(&path, &value).unwrap();
    let hash = sha256_file(&path).unwrap();
    let _lock = install.lock().unwrap();
    assert!(
        install
            .stage(&Version::new(0, 3, 0), &path, &"f".repeat(64), &archive)
            .is_err()
    );
    assert!(install.active().unwrap().is_none());
    install
        .stage(&Version::new(0, 3, 0), &path, &hash, &archive)
        .unwrap();
    assert!(
        install
            .stage(&Version::new(0, 3, 0), &path, &hash, &archive)
            .is_err()
    );
    let activated = install.activate(&Version::new(0, 3, 0)).unwrap();
    assert_eq!(activated.current, Version::new(0, 3, 0));
    assert!(activated.previous.is_none());
}
#[test]
fn archive_hash_and_target_pin_cannot_be_substituted() {
    let tmp = temp();
    let install = installation(&tmp);
    let archive = good_archive(tmp.path());
    let path = tmp.path().join("manifest.json");
    let mut value = manifest("0.3.0", 1);
    atomic_json(&path, &value).unwrap();
    assert!(
        install
            .stage(
                &value.version,
                &path,
                &sha256_file(&path).unwrap(),
                &archive
            )
            .unwrap_err()
            .to_string()
            .contains("archive SHA256")
    );
    value.archive_sha256 = sha256_file(&archive).unwrap();
    atomic_json(&path, &value).unwrap();
    assert!(
        install
            .stage(
                &Version::new(0, 4, 0),
                &path,
                &sha256_file(&path).unwrap(),
                &archive
            )
            .is_err()
    );
    value.target = "different-architecture".into();
    atomic_json(&path, &value).unwrap();
    assert!(
        install
            .stage(
                &Version::new(0, 3, 0),
                &path,
                &sha256_file(&path).unwrap(),
                &archive
            )
            .is_err()
    );
    assert!(Version::parse("../../outside").is_err());
    assert!(Version::parse("latest").is_err());
}
#[test]
fn traversal_links_reserved_names_and_duplicate_entries_are_rejected() {
    for entries in [
        vec![("../escape", b"oops" as &[u8], tar::EntryType::Regular)],
        vec![("/absolute", b"oops" as &[u8], tar::EntryType::Regular)],
        vec![("bin/link", b"" as &[u8], tar::EntryType::Symlink)],
        vec![("bin/CON", b"x" as &[u8], tar::EntryType::Regular)],
        vec![
            ("bin/A", b"x" as &[u8], tar::EntryType::Regular),
            ("bin/a", b"x" as &[u8], tar::EntryType::Regular),
        ],
        vec![("bin/..\\escape", b"x" as &[u8], tar::EntryType::Regular)],
        vec![("manifest.json", b"x" as &[u8], tar::EntryType::Regular)],
    ] {
        let tmp = temp();
        let source = archive(tmp.path(), &entries);
        let dest = tmp.path().join("out");
        fs::create_dir(&dest).unwrap();
        assert!(
            enginebox_install::release::extract_archive(&source, &dest).is_err(),
            "{entries:?}"
        );
        assert!(!tmp.path().join("escape").exists());
    }
}
#[test]
fn unrelated_prefix_and_concurrent_update_are_rejected() {
    let tmp = temp();
    let root = tmp.path().canonicalize().unwrap();
    assert!(Installation::inspect(root).is_err());
    let install = installation(&tmp);
    let first = install.lock().unwrap();
    assert!(install.lock().is_err());
    drop(first);
    assert!(install.lock().is_ok());
}
#[test]
fn path_validation_rejects_a_non_directory_ancestor() {
    let tmp = temp();
    let file = tmp.path().canonicalize().unwrap().join("file");
    fs::write(&file, "not a directory").unwrap();
    enginebox_install::release::reject_symlink_components(&file).unwrap();
    assert!(enginebox_install::release::reject_symlink_components(&file.join("state")).is_err());
}
#[test]
fn rollback_requires_state_compatibility() {
    let tmp = temp();
    let install = installation(&tmp);
    fixture_release(&install, "0.3.0", 1);
    fixture_release(&install, "0.4.0", 1);
    fixture_release(&install, "0.5.0", 2);
    install.activate(&Version::new(0, 3, 0)).unwrap();
    let state = install.activate(&Version::new(0, 4, 0)).unwrap();
    assert_eq!(state.previous, Some(Version::new(0, 3, 0)));
    assert!(install.activate(&Version::new(0, 5, 0)).is_err());
    assert_eq!(
        install.active().unwrap().unwrap().current,
        Version::new(0, 4, 0)
    );
    let state = install.activate(&Version::new(0, 3, 0)).unwrap();
    assert_eq!(state.previous, Some(Version::new(0, 4, 0)));
}
#[cfg(unix)]
#[test]
fn symlink_prefix_is_rejected() {
    use std::os::unix::fs::symlink;
    let tmp = temp();
    let target = tmp.path().join("target");
    fs::create_dir(&target).unwrap();
    let link = tmp.path().canonicalize().unwrap().join("link");
    symlink(target, &link).unwrap();
    assert!(Installation::inspect(link).is_err());
}

#[derive(Clone)]
struct Mock {
    version: String,
    mode: Arc<Mutex<String>>,
    ready: Arc<AtomicBool>,
    jobs: Arc<AtomicUsize>,
    mutations: Arc<AtomicUsize>,
    fail_activation: Arc<AtomicBool>,
    drain_calls: Arc<AtomicUsize>,
}
impl Mock {
    fn new(version: &str, mode: &str) -> Self {
        Self {
            version: version.into(),
            mode: Arc::new(Mutex::new(mode.into())),
            ready: Arc::new(AtomicBool::new(true)),
            jobs: Arc::new(AtomicUsize::new(0)),
            mutations: Arc::new(AtomicUsize::new(0)),
            fail_activation: Arc::new(AtomicBool::new(false)),
            drain_calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}
async fn mock_ready(State(s): State<Mock>) -> Json<serde_json::Value> {
    Json(
        serde_json::json!({"ready":s.ready.load(Ordering::SeqCst),"version":s.version,"state_schema":1,"mode":s.mode.lock().await.clone()}),
    )
}
async fn mock_drain(State(s): State<Mock>) -> Json<serde_json::Value> {
    s.drain_calls.fetch_add(1, Ordering::SeqCst);
    let drained = s.jobs.load(Ordering::SeqCst) == 0;
    *s.mode.lock().await = if drained { "drained" } else { "draining" }.into();
    Json(serde_json::json!({"drained":drained}))
}
async fn mock_activate(State(s): State<Mock>) -> (StatusCode, Json<serde_json::Value>) {
    if s.fail_activation.load(Ordering::SeqCst) {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error":"injected activation failure"})),
        );
    }
    *s.mode.lock().await = "active".into();
    (StatusCode::OK, Json(serde_json::json!({"mode":"active"})))
}
async fn mock_get(State(s): State<Mock>) -> String {
    s.version
}
async fn mock_mutation(State(s): State<Mock>) -> (StatusCode, String) {
    if *s.mode.lock().await != "active" {
        return (StatusCode::SERVICE_UNAVAILABLE, "drained".into());
    }
    s.mutations.fetch_add(1, Ordering::SeqCst);
    (StatusCode::OK, s.version)
}
async fn mock_observation(State(s): State<Mock>) -> (StatusCode, String) {
    if *s.mode.lock().await != "active" {
        return (StatusCode::SERVICE_UNAVAILABLE, "drained".into());
    }
    (StatusCode::OK, s.version)
}
async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, task)
}
async fn mock_server(state: Mock) -> (String, tokio::task::JoinHandle<()>) {
    serve(
        Router::new()
            .route("/readyz", get(mock_ready))
            .route("/internal/drain", post(mock_drain))
            .route("/internal/activate", post(mock_activate))
            .route("/internal/undrain", post(mock_activate))
            .route("/value", get(mock_get).post(mock_mutation))
            .route("/v1/agents", get(mock_observation))
            .route("/v1/node/heartbeat", post(mock_node_heartbeat))
            .route("/v1/node/result", post(mock_node_result))
            .with_state(state),
    )
    .await
}
struct Fixture {
    _tmp: tempfile::TempDir,
    supervisor: Supervisor,
    old: Mock,
    new: Mock,
    new_url: String,
    url: String,
    handles: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for h in &self.handles {
            h.abort();
        }
    }
}
async fn fixture() -> Fixture {
    let tmp = temp();
    let install = installation(&tmp);
    fixture_release(&install, "0.3.0", 1);
    fixture_release(&install, "0.4.0", 1);
    install.activate(&Version::new(0, 3, 0)).unwrap();
    let old = Mock::new("0.3.0", "active");
    let new = Mock::new("0.4.0", "standby");
    let (old_url, old_task) = mock_server(old.clone()).await;
    let (new_url, new_task) = mock_server(new.clone()).await;
    let config = SupervisorConfig {
        install,
        state_dir: None,
        fleet_config: tmp.path().join("fleet.toml"),
        api_listen: "127.0.0.1:8765".parse().unwrap(),
        dashboard_listen: None,
        token: "test-supervisor-token-at-least-32-characters".into(),
        drain_timeout: Duration::from_secs(1),
    };
    let supervisor = Supervisor::new(
        config,
        Candidate::external(Version::new(0, 3, 0), 1, old_url, None),
    )
    .unwrap();
    let (url, proxy_task) = serve(supervisor.api_router()).await;
    Fixture {
        _tmp: tmp,
        supervisor,
        old,
        new,
        new_url,
        url,
        handles: vec![old_task, new_task, proxy_task],
    }
}
fn candidate(f: &Fixture) -> Candidate {
    Candidate::external(Version::new(0, 4, 0), 1, f.new_url.clone(), None)
}
#[tokio::test]
async fn failed_candidate_health_leaves_old_serving() {
    let f = fixture().await;
    f.new.ready.store(false, Ordering::SeqCst);
    assert!(f.supervisor.handoff(candidate(&f)).await.is_err());
    assert_eq!(f.old.drain_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        reqwest::get(format!("{}/value", f.url))
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "0.3.0"
    );
}
#[tokio::test]
async fn drain_timeout_reactivates_old_without_killing_jobs() {
    let f = fixture().await;
    f.old.jobs.store(1, Ordering::SeqCst);
    assert!(f.supervisor.handoff(candidate(&f)).await.is_err());
    assert_eq!(*f.old.mode.lock().await, "active");
    assert_eq!(f.old.jobs.load(Ordering::SeqCst), 1);
    assert_eq!(f.supervisor.current_version().await, Version::new(0, 3, 0));
}
#[tokio::test]
async fn activation_failure_restores_old_and_does_not_change_version() {
    let f = fixture().await;
    f.new.fail_activation.store(true, Ordering::SeqCst);
    assert!(f.supervisor.handoff(candidate(&f)).await.is_err());
    assert_eq!(*f.old.mode.lock().await, "active");
    assert_eq!(
        f.supervisor
            .config
            .install
            .active()
            .unwrap()
            .unwrap()
            .current,
        Version::new(0, 3, 0)
    );
}
#[tokio::test]
async fn requests_continue_and_mutations_wait_for_single_owner_cutover() {
    let f = fixture().await;
    f.old.jobs.store(1, Ordering::SeqCst);
    let sup = f.supervisor.clone();
    let next = candidate(&f);
    let handoff = tokio::spawn(async move { sup.handoff(next).await });
    while f.old.drain_calls.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    let client = reqwest::Client::new();
    let post_url = format!("{}/value", f.url);
    let token = f.supervisor.config.token.clone();
    let write = tokio::spawn(async move {
        client
            .post(post_url)
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
    });
    let client = reqwest::Client::new();
    let observation_url = format!("{}/v1/agents", f.url);
    let token = f.supervisor.config.token.clone();
    let observation = tokio::spawn(async move {
        client
            .get(observation_url)
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
    });
    // Unauthenticated callers cannot enter the bounded ownership queue.
    let unauthorized = reqwest::Client::new()
        .post(format!("{}/value", f.url))
        .timeout(Duration::from_secs(1))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    for _ in 0..20 {
        let response = reqwest::get(format!("{}/value", f.url)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "0.3.0");
    }
    assert!(!write.is_finished());
    assert!(!observation.is_finished());
    assert_eq!(f.old.mutations.load(Ordering::SeqCst), 0);
    assert!(f.supervisor.handoff(candidate(&f)).await.is_err());
    f.old.jobs.store(0, Ordering::SeqCst);
    handoff.await.unwrap().unwrap();
    let response = write.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), "0.4.0");
    let response = observation.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), "0.4.0");
    assert_eq!(f.old.mutations.load(Ordering::SeqCst), 0);
    assert_eq!(f.new.mutations.load(Ordering::SeqCst), 1);
    assert_eq!(
        reqwest::get(format!("{}/value", f.url))
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "0.4.0"
    );
}
#[tokio::test]
async fn ownership_routes_are_not_exposed_and_management_requires_authentication() {
    let f = fixture().await;
    let client = reqwest::Client::new();
    assert_eq!(
        client
            .post(format!("{}/internal/activate", f.url))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        client
            .get(format!("{}/__enginebox/status", f.url))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .get(format!("{}/__enginebox/status", f.url))
            .bearer_auth(&f.supervisor.config.token)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn old_browser_static_assets_survive_release_cutover_without_exposing_private_files() {
    let f = fixture().await;
    let folder = f
        .supervisor
        .config
        .install
        .release_dir(&Version::new(0, 3, 0))
        .join("dashboard/dist/client/_next/static");
    fs::create_dir_all(folder.join("chunks")).unwrap();
    fs::create_dir_all(folder.join("css")).unwrap();
    fs::write(
        folder.join("chunks/framework-ChI052Ff.js"),
        "old framework chunk",
    )
    .unwrap();
    fs::write(folder.join("css/index.D9gOPbR-.css"), "old css chunk").unwrap();
    let uuid = uuid::Uuid::new_v4().to_string();
    fs::create_dir_all(folder.join(&uuid)).unwrap();
    fs::write(
        folder.join(&uuid).join("_buildManifest.js"),
        "old build manifest",
    )
    .unwrap();
    fs::write(folder.join("chunks/secret.json"), "not public fallback").unwrap();
    let (missing_url, missing_task) = serve(Router::new()).await;
    let active = Candidate::external(
        Version::new(0, 4, 0),
        1,
        f.new_url.clone(),
        Some(missing_url),
    );
    let supervisor = Supervisor::new(f.supervisor.config.clone(), active).unwrap();
    let (url, task) = serve(supervisor.dashboard_router()).await;
    for (path, expected) in [
        (
            "chunks/framework-ChI052Ff.js".to_string(),
            "old framework chunk",
        ),
        ("css/index.D9gOPbR-.css".to_string(), "old css chunk"),
        (format!("{uuid}/_buildManifest.js"), "old build manifest"),
    ] {
        let response = reqwest::get(format!("{url}/_next/static/{path}"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("cache-control").unwrap(),
            "public,max-age=31536000,immutable"
        );
        assert_eq!(response.text().await.unwrap(), expected);
    }
    for path in [
        "/_next/static/chunks/secret.json",
        "/_next/static/chunks/%2e%2e/secret.json",
        "/state/private.json",
        "/_next/static/chunks/index.js",
    ] {
        assert_eq!(
            reqwest::get(format!("{url}{path}")).await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
    }
    task.abort();
    missing_task.abort();
}

#[test]
fn bootstrap_inspection_never_claims_native_compute_on_macos_or_windows() {
    let plan =
        enginebox_install::bootstrap::plan(enginebox_install::bootstrap::Role::Operator).unwrap();
    assert!(plan.dependencies.is_empty());
    assert!(!plan.changes_disks);
    assert!(!plan.changes_firewall);
    if std::env::consts::OS != "linux" {
        assert!(
            enginebox_install::bootstrap::plan(enginebox_install::bootstrap::Role::IncusHost)
                .is_err()
        );
        assert_eq!(plan.compute_backend, "remote-linux-incus-only");
    }
}

#[test]
fn native_release_builder_and_installer_round_trip_with_deterministic_archives() {
    let tmp = temp();
    let directory = tmp.path().canonicalize().unwrap();
    let bins = directory.join("bin");
    fs::create_dir(&bins).unwrap();
    for binary in [
        "ebox",
        "enginebox-controller",
        "enginebox-ci-worker",
        "enginebox-api",
        "enginebox-fleet-mcp",
        "enginebox-host",
        "enginebox-guest",
        "enginebox-mcp",
        "enginebox-browser",
        "enginebox-agent-clients",
        "enginebox-install",
        "enginebox-supervisor",
    ] {
        let name = if cfg!(windows) {
            format!("{binary}.exe")
        } else {
            binary.into()
        };
        fs::write(bins.join(name), format!("fixture {binary}")).unwrap();
    }
    for name in [
        "LICENSE",
        "NOTICE",
        "LICENSE-APACHE-2.0",
        "LICENSE-SUSTAINABLE-USE-1.0",
    ] {
        fs::write(directory.join(name), format!("public {name}")).unwrap();
    }
    let outputs = [directory.join("one"), directory.join("two")];
    for output in &outputs {
        let result = std::process::Command::new(env!("CARGO_BIN_EXE_enginebox-release"))
            .args([
                "--version",
                "0.3.0",
                "--target",
                current_target(),
                "--bin-dir",
            ])
            .arg(&bins)
            .arg("--license")
            .arg(directory.join("LICENSE"))
            .arg("--notice")
            .arg(directory.join("NOTICE"))
            .arg("--output")
            .arg(output)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let archive_name = format!("enginebox-0.3.0-{}.tar.gz", current_target());
    let manifest_name = format!("enginebox-0.3.0-{}.json", current_target());
    assert_eq!(
        sha256_file(&outputs[0].join(&archive_name)).unwrap(),
        sha256_file(&outputs[1].join(&archive_name)).unwrap()
    );
    let install = directory.join("installed");
    let manifest_path = outputs[0].join(manifest_name);
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_enginebox-install"))
        .arg("--prefix")
        .arg(&install)
        .args(["stage", "--version", "0.3.0", "--manifest"])
        .arg(&manifest_path)
        .args([
            "--manifest-sha256",
            &sha256_file(&manifest_path).unwrap(),
            "--archive",
        ])
        .arg(outputs[0].join(archive_name))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_enginebox-install"))
        .arg("--prefix")
        .arg(&install)
        .args(["activate", "--version", "0.3.0"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        Installation::inspect(install)
            .unwrap()
            .active()
            .unwrap()
            .unwrap()
            .current,
        Version::new(0, 3, 0)
    );
}

#[cfg(windows)]
#[test]
fn windows_canonical_drive_paths_are_checked_after_the_root_is_complete() {
    let tmp = temp();
    let canonical = tmp.path().canonicalize().unwrap();
    assert!(matches!(
        canonical.components().next(),
        Some(std::path::Component::Prefix(_))
    ));
    enginebox_install::release::reject_symlink_components(&canonical).unwrap();
    enginebox_install::release::reject_symlink_components(&canonical.join("future/state")).unwrap();
}

#[cfg(windows)]
#[test]
fn windows_junction_ancestors_are_still_rejected() {
    let tmp = temp();
    let target = tmp.path().join("target");
    let link = tmp.path().join("junction");
    fs::create_dir(&target).unwrap();
    let result = std::process::Command::new("powershell.exe")
        // A pwsh 7 parent can export module paths incompatible with Windows
        // PowerShell 5. Let the child reconstruct its own default module path.
        .env_remove("PSModulePath")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$ErrorActionPreference='Stop'; New-Item -ItemType Junction -Path $env:ENGINEBOX_JUNCTION_LINK -Target $env:ENGINEBOX_JUNCTION_TARGET | Out-Null",
        ])
        .env("ENGINEBOX_JUNCTION_LINK", &link)
        .env("ENGINEBOX_JUNCTION_TARGET", &target)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let canonical_parent = tmp.path().canonicalize().unwrap();
    assert!(
        enginebox_install::release::reject_symlink_components(
            &canonical_parent.join("junction/state")
        )
        .is_err()
    );
    fs::remove_dir(&link).unwrap();
    assert!(target.is_dir());
}

#[cfg(windows)]
#[test]
fn windows_private_directory_replaces_explicit_everyone_permissions() {
    let tmp = temp();
    let directory = tmp.path().canonicalize().unwrap().join("private");
    fs::create_dir(&directory).unwrap();
    let grant = std::process::Command::new("icacls")
        .arg(&directory)
        .args(["/grant", "*S-1-1-0:(OI)(CI)F"])
        .output()
        .unwrap();
    assert!(
        grant.status.success(),
        "icacls grant failed: {}",
        String::from_utf8_lossy(&grant.stderr)
    );
    enginebox_install::release::private_dir(&directory).unwrap();
    fs::create_dir(directory.join("child")).unwrap();
    fs::write(directory.join("child/secret.txt"), "retained private state").unwrap();
    // Native calls accept the canonical verbatim path. Independently inspect
    // the same directory through .NET using its ordinary filesystem path.
    // Direct framework APIs avoid a dependency on PowerShell module loading.
    let check = r#"$ErrorActionPreference='Stop'; $sid=[System.Security.Principal.WindowsIdentity]::GetCurrent().User; $acl=[System.IO.Directory]::GetAccessControl($env:ENGINEBOX_PRIVATE_DIRECTORY); if(-not $acl.AreAccessRulesProtected){throw 'DACL is not protected'}; if($acl.GetOwner([System.Security.Principal.SecurityIdentifier]) -ne $sid){throw 'Owner is not current user'}; $rules=$acl.GetAccessRules($true,$true,[System.Security.Principal.SecurityIdentifier]); if($rules.Count -ne 1){throw 'Expected exactly one owner ACE'}; foreach($rule in $rules){if($rule.IdentityReference -ne $sid -or $rule.AccessControlType -ne 'Allow' -or $rule.FileSystemRights -ne 'FullControl' -or $rule.InheritanceFlags -ne 'ContainerInherit,ObjectInherit'){throw 'Unexpected owner ACE'}}; $child=[System.IO.File]::GetAccessControl([System.IO.Path]::Combine($env:ENGINEBOX_PRIVATE_DIRECTORY,'child/secret.txt')); $childRules=$child.GetAccessRules($true,$true,[System.Security.Principal.SecurityIdentifier]); if($childRules.Count -ne 1 -or $childRules[0].IdentityReference -ne $sid -or -not $childRules[0].IsInherited){throw 'Child file did not inherit private owner access'}"#;
    let result = std::process::Command::new("powershell.exe")
        .env_remove("PSModulePath")
        .args(["-NoProfile", "-NonInteractive", "-Command", check])
        .env("ENGINEBOX_PRIVATE_DIRECTORY", tmp.path().join("private"))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "Windows directory DACL check failed ({:?}): {}",
        result.status.code(),
        String::from_utf8_lossy(&result.stderr)
    );
    enginebox_install::release::private_dir(&directory).unwrap();
    assert_eq!(
        fs::read_to_string(directory.join("child/secret.txt")).unwrap(),
        "retained private state"
    );
}

async fn mock_node_heartbeat(
    State(s): State<Mock>,
    headers: axum::http::HeaderMap,
) -> (StatusCode, String) {
    if headers.get("authorization").and_then(|v| v.to_str().ok())
        != Some("Bearer fixture-node-token")
    {
        return (StatusCode::UNAUTHORIZED, "node auth".into());
    }
    (StatusCode::OK, s.version)
}
async fn mock_node_result(
    State(s): State<Mock>,
    headers: axum::http::HeaderMap,
) -> (StatusCode, String) {
    let result = mock_node_heartbeat(State(s.clone()), headers).await;
    if result.0 == StatusCode::OK {
        s.jobs.store(0, Ordering::SeqCst);
    }
    result
}
#[tokio::test]
async fn scoped_node_results_pass_drain_gate_without_authorizing_operator_writes() {
    let f = fixture().await;
    let client = reqwest::Client::new();
    for path in ["/value", "/v1/node/arbitrary", "/v1/node/heartbeat"] {
        let response = client
            .post(format!("{}{path}", f.url))
            .bearer_auth("invalid-node-token")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    f.old.jobs.store(1, Ordering::SeqCst);
    let supervisor = f.supervisor.clone();
    let next = candidate(&f);
    let upgrade = tokio::spawn(async move { supervisor.handoff(next).await });
    tokio::time::timeout(Duration::from_millis(500), async {
        while f.old.drain_calls.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    for action in ["heartbeat", "result"] {
        let response = client
            .post(format!("{}/v1/node/{action}", f.url))
            .bearer_auth("fixture-node-token")
            .timeout(Duration::from_millis(500))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "0.3.0");
    }
    upgrade.await.unwrap().unwrap();
    assert_eq!(f.supervisor.current_version().await, Version::new(0, 4, 0));
}
