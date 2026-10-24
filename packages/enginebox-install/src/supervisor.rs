use crate::release::{Activation, Installation, Manifest};
use anyhow::{Context, Result, bail, ensure};
use axum::{
    Json, Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::StreamExt;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::{
    fs::OpenOptions,
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::{
    process::{Child, Command},
    sync::{Mutex, RwLock, Semaphore},
    time::{Instant, sleep, timeout},
};

#[derive(Clone)]
pub struct SupervisorConfig {
    pub install: Installation,
    /// Existing private controller state, when migrating without moving native homes.
    /// Omitted installations retain PREFIX/state.
    pub state_dir: Option<PathBuf>,
    pub fleet_config: PathBuf,
    pub api_listen: SocketAddr,
    pub dashboard_listen: Option<SocketAddr>,
    pub token: String,
    pub drain_timeout: Duration,
}
impl SupervisorConfig {
    pub fn state_root(&self) -> Result<PathBuf> {
        let default = self.install.root.join("state");
        let Some(path) = &self.state_dir else {
            return Ok(default);
        };
        crate::release::reject_symlink_components(path)?;
        ensure!(
            path == &default
                || (!path.starts_with(&self.install.root) && !self.install.root.starts_with(path)),
            "external controller state must not overlap the installation"
        );
        let meta = std::fs::metadata(path)
            .context("configured controller state must already exist; refusing to create an empty replacement")?;
        ensure!(meta.is_dir(), "controller state must be a directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            ensure!(
                meta.uid() == unsafe { libc::geteuid() } && meta.permissions().mode() & 0o077 == 0,
                "configured controller state must be owned by the service user with owner-only permissions"
            );
        }
        #[cfg(not(unix))]
        ensure!(
            path == &default,
            "external controller state is currently qualified only on Unix; use the installer-owned state directory"
        );
        Ok(path.clone())
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Ready {
    pub ready: bool,
    pub state_schema: u32,
    pub version: String,
    pub mode: String,
}
#[derive(Debug)]
pub struct Candidate {
    pub version: Version,
    pub schema: u32,
    pub api: String,
    pub dashboard: Option<String>,
    _children: Vec<Child>,
    _lifelines: Vec<Lifeline>,
    requests: AtomicUsize,
}
impl Candidate {
    /// An external candidate is useful for embedding and integration tests. The caller owns its lifetime.
    pub fn external(version: Version, schema: u32, api: String, dashboard: Option<String>) -> Self {
        Self {
            version,
            schema,
            api,
            dashboard,
            _children: vec![],
            _lifelines: vec![],
            requests: AtomicUsize::new(0),
        }
    }
}
struct RequestLease(Arc<Candidate>);
impl RequestLease {
    fn new(candidate: Arc<Candidate>) -> Self {
        candidate.requests.fetch_add(1, Ordering::SeqCst);
        Self(candidate)
    }
}
impl Drop for RequestLease {
    fn drop(&mut self) {
        self.0.requests.fetch_sub(1, Ordering::SeqCst);
    }
}
#[derive(Clone)]
pub struct Supervisor {
    pub config: SupervisorConfig,
    client: reqwest::Client,
    active: Arc<RwLock<Arc<Candidate>>>,
    mutation_gate: Arc<RwLock<()>>,
    waiting: Arc<Semaphore>,
    transaction: Arc<Mutex<()>>,
}
impl Supervisor {
    pub fn new(config: SupervisorConfig, active: Candidate) -> Result<Self> {
        ensure!(
            config.token.len() >= 32,
            "API token must have at least 32 characters"
        );
        ensure!(
            config.api_listen.ip().is_loopback()
                && config.dashboard_listen.is_none_or(|a| a.ip().is_loopback()),
            "supervisor listeners must use loopback; put an authenticated TLS proxy in front for external access"
        );
        ensure!(
            (1..=900).contains(&config.drain_timeout.as_secs()),
            "drain timeout must be 1..900 seconds"
        );
        Ok(Self {
            config,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(5))
                .build()?,
            active: Arc::new(RwLock::new(Arc::new(active))),
            mutation_gate: Arc::new(RwLock::new(())),
            waiting: Arc::new(Semaphore::new(256)),
            transaction: Arc::new(Mutex::new(())),
        })
    }
    pub fn api_router(&self) -> Router {
        Router::new()
            .route("/__enginebox/status", get(status))
            .route("/__enginebox/update", post(update))
            .route("/__enginebox/rollback", post(rollback))
            .fallback(proxy_api)
            .with_state(self.clone())
    }
    pub fn dashboard_router(&self) -> Router {
        Router::new()
            .fallback(proxy_dashboard)
            .with_state(self.clone())
    }
    fn authorized(&self, headers: &HeaderMap) -> bool {
        let Some(value) = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
        else {
            return false;
        };
        bool::from(value.as_bytes().ct_eq(self.config.token.as_bytes()))
    }
    async fn control(&self, candidate: &Candidate, path: &str) -> Result<serde_json::Value> {
        let response = self
            .client
            .post(format!("{}{path}", candidate.api))
            .bearer_auth(&self.config.token)
            .timeout(Duration::from_secs(10))
            .send()
            .await?;
        ensure!(
            response.status().is_success(),
            "controller refused {path} with {}",
            response.status()
        );
        Ok(response.json().await?)
    }
    async fn ready(&self, candidate: &Candidate) -> Result<Ready> {
        let ready: Ready = self
            .client
            .get(format!("{}/readyz", candidate.api))
            .timeout(Duration::from_secs(3))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            ready.ready
                && ready.version == candidate.version.to_string()
                && ready.state_schema == candidate.schema,
            "candidate readiness version/schema mismatch"
        );
        Ok(ready)
    }
    async fn drain(&self, candidate: &Candidate) -> Result<()> {
        let deadline = Instant::now() + self.config.drain_timeout;
        loop {
            let state = self.control(candidate, "/internal/drain").await?;
            if state.get("drained").and_then(|v| v.as_bool()) == Some(true) {
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "controller drain deadline exceeded; running jobs were not terminated"
            );
            sleep(Duration::from_millis(100)).await;
        }
    }
    pub async fn current_version(&self) -> Version {
        self.active.read().await.version.clone()
    }
    /// Check ready before taking the mutation gate; standby never touches provider or job state.
    /// Requests already forwarding finish before draining; subsequent mutations queue at the gate.
    pub async fn handoff(&self, candidate: Candidate) -> Result<Activation> {
        let _transaction = self
            .transaction
            .try_lock()
            .context("another upgrade is already running")?;
        let _disk_lock = self.config.install.lock()?;
        let manifest = self.config.install.manifest(&candidate.version)?;
        let old = self.active.read().await.clone();
        ensure!(
            manifest.compatible_with(old.schema),
            "incompatible state schema: offline migration required"
        );
        ensure!(
            candidate.schema == manifest.state_schema,
            "candidate schema mismatch"
        );
        let ready = self.ready(&candidate).await?;
        ensure!(
            ready.mode == "standby",
            "new candidate must start in standby"
        );
        if let Some(url) = &candidate.dashboard {
            ensure!(
                self.client
                    .get(format!("{url}/healthz"))
                    .timeout(Duration::from_secs(5))
                    .send()
                    .await?
                    .status()
                    .is_success(),
                "candidate dashboard health check failed"
            );
        }
        let _gate = timeout(self.config.drain_timeout, self.mutation_gate.write())
            .await
            .context("existing HTTP mutation did not finish before drain deadline")?;
        if let Err(error) = self.drain(&old).await {
            self.control(&old, "/internal/undrain")
                .await
                .context("failed to reactivate old controller after drain failure")?;
            return Err(error);
        }
        if let Err(error) = self.control(&candidate, "/internal/activate").await {
            // An activate request can succeed server-side before the response is lost.
            self.drain(&candidate)
                .await
                .context("candidate activation was uncertain and could not be safely drained")?;
            self.control(&old, "/internal/undrain")
                .await
                .context("failed to restore old controller ownership")?;
            return Err(error);
        }
        let activated = self.ready(&candidate).await;
        if !matches!(activated,Ok(ref state) if state.mode=="active") {
            self.drain(&candidate).await?;
            self.control(&old, "/internal/undrain").await?;
            bail!("candidate did not become active; old controller restored");
        }
        let activation = match self.config.install.activate(&candidate.version) {
            Ok(value) => value,
            Err(error) => {
                self.drain(&candidate).await?;
                self.control(&old, "/internal/undrain").await?;
                return Err(error);
            }
        };
        *self.active.write().await = Arc::new(candidate);
        // Outstanding streamed responses retain old process ownership through RequestLease.
        drop(old);
        Ok(activation)
    }
    pub async fn upgrade(&self, version: Version) -> Result<Activation> {
        ensure!(
            self.current_version().await != version,
            "requested version is already active"
        );
        let manifest = self.config.install.manifest(&version)?;
        let candidate = spawn_candidate(&self.config, &manifest).await?;
        self.handoff(candidate).await
    }
}
fn failure(status: StatusCode, error: impl std::fmt::Display) -> Response {
    (status, Json(serde_json::json!({"error":error.to_string()}))).into_response()
}
async fn status(State(supervisor): State<Supervisor>, headers: HeaderMap) -> Response {
    if !supervisor.authorized(&headers) {
        return failure(StatusCode::UNAUTHORIZED, "authentication required");
    }
    Json(serde_json::json!({"current":supervisor.current_version().await,"pending_mutations":256-supervisor.waiting.available_permits(),"node_protocol":1})).into_response()
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    version: Version,
}
async fn update(
    State(supervisor): State<Supervisor>,
    headers: HeaderMap,
    Json(target): Json<Target>,
) -> Response {
    if !supervisor.authorized(&headers) {
        return failure(StatusCode::UNAUTHORIZED, "authentication required");
    }
    // Detached task ensures a disconnected administrator does not cancel the ownership handoff.
    match tokio::spawn(async move { supervisor.upgrade(target.version).await }).await {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(e)) => failure(StatusCode::CONFLICT, e),
        Err(_) => failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "upgrade task failed; inspect private service logs",
        ),
    }
}
async fn rollback(State(supervisor): State<Supervisor>, headers: HeaderMap) -> Response {
    if !supervisor.authorized(&headers) {
        return failure(StatusCode::UNAUTHORIZED, "authentication required");
    }
    let target = match supervisor.config.install.active() {
        Ok(Some(state)) => state.previous,
        _ => None,
    };
    let Some(version) = target else {
        return failure(StatusCode::CONFLICT, "no previous release");
    };
    update(State(supervisor), headers, Json(Target { version })).await
}
async fn proxy_api(State(supervisor): State<Supervisor>, request: Request) -> Response {
    proxy(supervisor, request, false).await
}
async fn proxy_dashboard(State(supervisor): State<Supervisor>, request: Request) -> Response {
    proxy(supervisor, request, true).await
}
fn hop_header(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}
async fn proxy(supervisor: Supervisor, request: Request, dashboard: bool) -> Response {
    let path = request.uri().path();
    if path.starts_with("/internal/") || path.starts_with("/__enginebox/") {
        return failure(StatusCode::NOT_FOUND, "not found");
    }
    if request.headers().contains_key("upgrade") {
        return failure(
            StatusCode::NOT_IMPLEMENTED,
            "WebSocket upgrades are not supported by this API transport",
        );
    }
    // A dashboard POST calls the API through this same front door. Gate only
    // its actual API request, avoiding a recursive read-lock deadlock when an
    // upgrade writer queues between the dashboard request and its API call.
    // Native observations can refresh credentials and need the same ownership
    // as writes. Pure reads continue against the draining controller.
    // Scoped node authentication belongs to the active controller. Completion
    // traffic must pass the upgrade gate so a draining owner can finish its jobs.
    let node_action = (request.method() == Method::POST && !dashboard)
        .then(|| path.strip_prefix("/v1/node/"))
        .flatten()
        .filter(|action| ["enroll", "hello", "heartbeat", "poll", "result"].contains(action));
    let finishing_node =
        node_action.is_some_and(|action| ["heartbeat", "result"].contains(&action));
    let owned_request = !dashboard
        && !finishing_node
        && (!matches!(
            *request.method(),
            Method::GET | Method::HEAD | Method::OPTIONS
        ) || (matches!(*request.method(), Method::GET | Method::HEAD)
            && (path == "/v1/agents"
                || (path.starts_with("/v1/agents/")
                    && (path.ends_with("/usage") || path.ends_with("/accounts"))))));
    if owned_request && node_action.is_none() && !supervisor.authorized(request.headers()) {
        return failure(StatusCode::UNAUTHORIZED, "authentication required");
    }
    let permit = if owned_request {
        match supervisor.waiting.clone().try_acquire_owned() {
            Ok(value) => Some(value),
            Err(_) => {
                return failure(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "upgrade request queue is full; retry",
                );
            }
        }
    } else {
        None
    };
    let gate = if owned_request {
        match timeout(
            supervisor.config.drain_timeout + Duration::from_secs(15),
            supervisor.mutation_gate.read(),
        )
        .await
        {
            Ok(value) => Some(value),
            Err(_) => {
                return failure(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "upgrade wait deadline exceeded; retry",
                );
            }
        }
    } else {
        None
    };
    let active = supervisor.active.read().await.clone();
    let lease = RequestLease::new(active.clone());
    let upstream = if dashboard {
        match &active.dashboard {
            Some(url) => url,
            None => {
                return failure(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "dashboard not included in active release",
                );
            }
        }
    } else {
        &active.api
    };
    let uri = request
        .uri()
        .path_and_query()
        .map(|v| v.as_str())
        .unwrap_or("/")
        .to_owned();
    let (parts, body) = request.into_parts();
    let body = match axum::body::to_bytes(body, 16 * 1024 * 1024).await {
        Ok(value) => value,
        Err(_) => return failure(StatusCode::PAYLOAD_TOO_LARGE, "request exceeds 16 MiB"),
    };
    let mut headers = parts.headers;
    let nominated: Vec<String> = headers
        .get("connection")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(',')
        .map(|v| v.trim().to_ascii_lowercase())
        .collect();
    let to_remove: Vec<_> = headers
        .keys()
        .filter(|key| {
            hop_header(key.as_str())
                || nominated.iter().any(|v| v == key.as_str())
                || key.as_str().starts_with("x-forwarded-")
                || key.as_str() == "forwarded"
        })
        .cloned()
        .collect();
    for key in to_remove {
        headers.remove(key);
    }
    // Preserve Host for dashboard origin/CSRF checking; no client-supplied forwarding headers survive.
    let response = match supervisor
        .client
        .request(parts.method, format!("{upstream}{uri}"))
        .headers(headers)
        .body(body)
        .send()
        .await
    {
        Ok(value) => value,
        Err(_) => {
            return failure(
                StatusCode::BAD_GATEWAY,
                "active controller connection failed",
            );
        }
    };
    drop(gate);
    drop(permit);
    if dashboard
        && response.status() == StatusCode::NOT_FOUND
        && let Some(response) = historical_asset(&supervisor.config.install, &uri).await
    {
        return response;
    }
    let mut result = Response::builder().status(response.status());
    for (key, value) in response.headers() {
        if !hop_header(key.as_str()) {
            result = result.header(key, value);
        }
    }
    let stream = response.bytes_stream().map(move |value| {
        let _ = &lease;
        value
    });
    result
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| failure(StatusCode::BAD_GATEWAY, "invalid upstream response"))
}
async fn historical_asset(install: &Installation, uri: &str) -> Option<Response> {
    // Only immutable Vite hash-named assets, never HTML, API or arbitrary files.
    let path = uri.split('?').next()?;
    let relative = path.strip_prefix("/_next/static/")?;
    let parts: Vec<_> = relative.split('/').collect();
    if parts.len() != 2
        || parts.iter().any(|v| {
            v.is_empty()
                || *v == ".."
                || *v == "."
                || !v
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        })
    {
        return None;
    }
    let filename = parts[1];
    let (stem, extension) = filename.rsplit_once('.')?;
    let build_manifest = uuid::Uuid::parse_str(parts[0]).is_ok()
        && ["_buildManifest.js", "_ssgManifest.js"].contains(&filename);
    let hashed_asset = ["chunks", "css", "media"].contains(&parts[0])
        && [stem.split_once('-'), stem.rsplit_once('.')]
            .iter()
            .any(|v| v.is_some_and(|(_, hash)| hash.len() >= 8))
        && ["js", "css", "woff", "woff2", "svg", "png", "jpg", "webp"].contains(&extension);
    if !build_manifest && !hashed_asset {
        return None;
    }
    let mut directories = tokio::fs::read_dir(install.root.join("releases"))
        .await
        .ok()?;
    while let Ok(Some(directory)) = directories.next_entry().await {
        if Version::parse(&directory.file_name().to_string_lossy()).is_err() {
            continue;
        }
        let file = directory
            .path()
            .join("dashboard/dist/client/_next/static")
            .join(relative);
        if crate::release::reject_symlink_components(&file).is_err() {
            continue;
        }
        if let Ok(content) = tokio::fs::read(file).await {
            let kind = match extension {
                "js" => "text/javascript",
                "css" => "text/css",
                "woff" => "font/woff",
                "woff2" => "font/woff2",
                "svg" => "image/svg+xml",
                "png" => "image/png",
                "jpg" => "image/jpeg",
                _ => "image/webp",
            };
            return Some(
                (
                    [
                        ("content-type", kind),
                        ("cache-control", "public,max-age=31536000,immutable"),
                        ("x-content-type-options", "nosniff"),
                    ],
                    content,
                )
                    .into_response(),
            );
        }
    }
    None
}
fn unused_port() -> Result<u16> {
    let socket = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    Ok(socket.local_addr()?.port())
}
#[derive(Debug)]
struct Lifeline {
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Lifeline {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn lifeline() -> Result<(String, String, Lifeline)> {
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?.to_string();
    let token = uuid::Uuid::new_v4().simple().to_string();
    let expected = token.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, peer)) = listener.accept().await else {
                return;
            };
            if !peer.ip().is_loopback() {
                continue;
            }
            let received = timeout(Duration::from_secs(3), async {
                let mut received = Vec::new();
                let mut byte = [0u8; 1];
                while received.len() < 128 {
                    if socket.read(&mut byte).await.ok()? == 0 {
                        return None;
                    }
                    if byte[0] == b'\n' {
                        return Some(received);
                    }
                    received.push(byte[0]);
                }
                None
            })
            .await;
            if !matches!(received,Ok(Some(ref value)) if bool::from(value.as_slice().ct_eq(expected.as_bytes())))
            {
                continue;
            }
            // No payload is exchanged. Either process dying closes this authenticated lifetime socket.
            let mut byte = [0u8; 1];
            let _ = socket.read(&mut byte).await;
            return;
        }
    });
    Ok((address, token, Lifeline { task }))
}
fn log_file(
    config: &SupervisorConfig,
    version: &Version,
    component: &str,
) -> Result<std::fs::File> {
    let path = config
        .install
        .root
        .join("logs")
        .join(format!("{version}-{component}.log"));
    crate::release::reject_symlink_components(&path)?;
    let file = OpenOptions::new().append(true).create(true).open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}
pub async fn spawn_candidate(config: &SupervisorConfig, manifest: &Manifest) -> Result<Candidate> {
    let state = config.state_root()?;
    let directory = config.install.release_dir(&manifest.version);
    let port = unused_port()?;
    let binary = directory.join("bin").join(if cfg!(windows) {
        "enginebox-controller.exe"
    } else {
        "enginebox-controller"
    });
    let output = log_file(config, &manifest.version, "controller")?;
    let (address, token, controller_lifeline) = lifeline().await?;
    let controller = Command::new(binary)
        .args(["--config"])
        .arg(&config.fleet_config)
        .args(["--listen", &format!("127.0.0.1:{port}")])
        .env("ENGINEBOX_STATE_DIR", state)
        .env("ENGINEBOX_START_STANDBY", "1")
        .env("ENGINEBOX_API_TOKEN", &config.token)
        .env_remove("ENGINEBOX_API_TOKEN_FILE")
        .env("ENGINEBOX_SUPERVISOR_LIFELINE", address)
        .env("ENGINEBOX_SUPERVISOR_LIFELINE_TOKEN", token)
        .stdin(std::process::Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(output)
        .kill_on_drop(true)
        .spawn()
        .context("start candidate controller")?;
    let mut children = vec![controller];
    let mut lifelines = vec![controller_lifeline];
    let mut dashboard = None;
    if manifest.dashboard && config.dashboard_listen.is_some() {
        let dashboard_port = unused_port()?;
        let output = log_file(config, &manifest.version, "dashboard")?;
        let (address, token, dashboard_lifeline) = lifeline().await?;
        let child = Command::new("node")
            .arg("scripts/start.mjs")
            .current_dir(directory.join("dashboard"))
            .env("HOST", "127.0.0.1")
            .env("PORT", dashboard_port.to_string())
            .env("ENGINEBOX_API_URL", format!("http://{}", config.api_listen))
            .env(
                "ENGINEBOX_DASHBOARD_ORIGIN",
                std::env::var("ENGINEBOX_DASHBOARD_ORIGIN").unwrap_or_else(|_| {
                    format!(
                        "http://{}",
                        config
                            .dashboard_listen
                            .expect("dashboard listener was checked")
                    )
                }),
            )
            .env("ENGINEBOX_API_TOKEN", &config.token)
            .env("ENGINEBOX_SUPERVISOR_LIFELINE", address)
            .env("ENGINEBOX_SUPERVISOR_LIFELINE_TOKEN", token)
            .stdin(std::process::Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output)
            .kill_on_drop(true)
            .spawn()
            .context("start dashboard; install Node.js 22.12+ or omit dashboard listener")?;
        children.push(child);
        lifelines.push(dashboard_lifeline);
        dashboard = Some(format!("http://127.0.0.1:{dashboard_port}"));
    }
    let candidate = Candidate {
        version: manifest.version.clone(),
        schema: manifest.state_schema,
        api: format!("http://127.0.0.1:{port}"),
        dashboard,
        _children: children,
        _lifelines: lifelines,
        requests: AtomicUsize::new(0),
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?;
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let api_ready = match client.get(format!("{}/readyz", candidate.api)).send().await {
            Ok(response) => response.status().is_success(),
            Err(_) => false,
        };
        let ui_ready = match &candidate.dashboard {
            Some(url) => client
                .get(format!("{url}/healthz"))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success()),
            None => true,
        };
        if api_ready && ui_ready {
            return Ok(candidate);
        }
        ensure!(
            Instant::now() < deadline,
            "candidate health deadline exceeded; inspect private release logs"
        );
        sleep(Duration::from_millis(100)).await;
    }
}
pub async fn run(config: SupervisorConfig) -> Result<()> {
    config.install.initialize()?;
    let _lifetime_lock = config.install.named_lock("supervisor.lock")?;
    let active = config
        .install
        .active()?
        .context("no active release; run enginebox-install activate before starting supervisor")?;
    let manifest = config.install.manifest(&active.current)?;
    ensure!(
        manifest.compatible_with(active.state_schema),
        "installed release cannot read state schema"
    );
    // Bind stable sockets before activating a controller, preventing duplicate public instances.
    let listener = tokio::net::TcpListener::bind(config.api_listen).await?;
    let dashboard_listener = match config.dashboard_listen {
        Some(address) => Some(tokio::net::TcpListener::bind(address).await?),
        None => None,
    };
    let candidate = spawn_candidate(&config, &manifest).await?;
    let supervisor = Supervisor::new(config, candidate)?;
    let old = supervisor.active.read().await.clone();
    let deadline = Instant::now() + supervisor.config.drain_timeout;
    loop {
        let response = supervisor
            .client
            .post(format!("{}/internal/activate", old.api))
            .bearer_auth(&supervisor.config.token)
            .timeout(Duration::from_secs(10))
            .send()
            .await?;
        if response.status().is_success() {
            break;
        }
        ensure!(
            [StatusCode::CONFLICT, StatusCode::LOCKED].contains(&response.status())
                && Instant::now() < deadline,
            "controller activation refused ({}); an old owner may still be draining after supervisor restart",
            response.status()
        );
        sleep(Duration::from_millis(200)).await;
    }
    ensure!(
        supervisor.ready(&old).await?.mode == "active",
        "controller did not activate"
    );
    if let Some(listener) = dashboard_listener {
        let app = supervisor.dashboard_router();
        tokio::spawn(async move { axum::serve(listener, app).await });
    }
    axum::serve(listener, supervisor.api_router())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
