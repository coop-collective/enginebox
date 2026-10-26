//! Authenticated fleet API, durable jobs and exclusive controller ownership.
pub mod callbacks;
pub mod ci;
mod ci_templates;
pub mod contributors;
mod environment;
pub mod jobs;
pub mod profiles;
pub mod repositories;
pub mod service_tokens;
pub mod storage;

use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::{Method, Request, StatusCode, header},
    response::{IntoResponse, Response},
    routing::any,
};
use enginebox_agents::AgentService;
use enginebox_core::{Fleet, identifier};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use subtle::ConstantTimeEq;

pub const STATE_SCHEMA: u32 = 1;
#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Standby,
    Active,
    Draining,
    Drained,
}
impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::Standby => "standby",
            Self::Active => "active",
            Self::Draining => "draining",
            Self::Drained => "drained",
        }
    }
}
#[derive(Clone)]
struct Runtime {
    agents: AgentService,
    jobs: jobs::Jobs,
    ci_jobs: jobs::Jobs,
    storage: storage::Storage,
    github: enginebox_github::GitHub,
    ci: ci::Ci,
    contributors: contributors::Contributors,
    service_tokens: service_tokens::ServiceTokens,
    profiles: profiles::Profiles,
    repositories: repositories::Repositories,
}
impl Runtime {
    fn pending_jobs(&self) -> usize {
        self.jobs.pending() + self.ci_jobs.pending() + self.github.pending()
    }
}
struct Gate {
    mode: Mode,
    requests: usize,
    background: usize,
    owner: Option<enginebox_core::state::StateLock>,
    runtime: Option<Runtime>,
}
pub struct Controller {
    fleet: Fleet,
    root: PathBuf,
    token: String,
    gate: Mutex<Gate>,
    observations: Mutex<std::collections::BTreeMap<String, Value>>,
    // Last background reconcile failure (message, RFC 3339 time); cleared on success.
    reconcile_error: Mutex<Option<(String, String)>>,
    /// Test harnesses stand in for the repository backend's API.
    repository_transport: Mutex<Option<(String, Arc<dyn repositories::Transport>)>>,
}

struct Admission(Arc<Controller>);
impl Drop for Admission {
    fn drop(&mut self) {
        self.0.gate.lock().unwrap().requests -= 1;
    }
}

impl Controller {
    pub fn new(config: &Path, root: &Path, token: String) -> Result<Arc<Self>> {
        ensure!(
            token.len() >= 32 && token.bytes().all(|c| (33..=126).contains(&c)),
            "API token must contain at least 32 printable non-space characters"
        );
        jobs::private_dir(root)?;
        // Resolve platform aliases such as macOS /var before setting the
        // private credential boundary. Descendants remain symlink-rejected.
        let root = root.canonicalize()?;
        let fleet = Fleet::load(config, &root)?;
        Ok(Arc::new(Self {
            fleet,
            root,
            token,
            gate: Mutex::new(Gate {
                mode: Mode::Standby,
                requests: 0,
                background: 0,
                owner: None,
                runtime: None,
            }),
            observations: Mutex::new(std::collections::BTreeMap::new()),
            reconcile_error: Mutex::new(None),
            repository_transport: Mutex::new(None),
        }))
    }
    pub fn activate(self: &Arc<Self>) -> Result<Value> {
        let mut gate = self.gate.lock().unwrap();
        if gate.mode == Mode::Active {
            return Ok(json!({"active":true,"state_schema":STATE_SCHEMA}));
        }
        if gate.mode == Mode::Draining && gate.owner.is_some() {
            gate.mode = Mode::Active;
            if let Some(runtime) = &gate.runtime {
                runtime.agents.resume_admission();
            }
            return Ok(json!({"active":true,"state_schema":STATE_SCHEMA}));
        }
        ensure!(
            gate.requests == 0
                && gate.background == 0
                && gate
                    .runtime
                    .as_ref()
                    .is_none_or(|r| r.pending_jobs() == 0 && r.agents.pending() == 0),
            "Controller has not drained"
        );
        let owner = enginebox_core::state::lock(&self.root.join("controller-owner.lock"), false)
            .context("Another controller owns this state; drain it before activation")?;
        let schema_path = self.root.join("schema.json");
        if schema_path.exists() {
            let meta = std::fs::symlink_metadata(&schema_path)?;
            ensure!(
                meta.is_file() && !meta.file_type().is_symlink(),
                "Invalid state schema record"
            );
            let schema = enginebox_core::state::read_json(&schema_path)?;
            ensure!(
                schema["state_schema"] == STATE_SCHEMA,
                "State schema is incompatible with this controller"
            );
        } else {
            jobs::write_private(&schema_path, &json!({"state_schema":STATE_SCHEMA}))?;
        }
        // Reload under exclusive ownership on every activation, including rollback.
        // A standby process must never recover another process's active jobs.
        let github = enginebox_github::GitHub::open(&self.root)?;
        let runtime = Runtime {
            agents: AgentService::new(&self.root, self.fleet.clone())?,
            jobs: jobs::Jobs::open(&self.root.join("jobs"))?,
            // Older controller releases prune their ordinary job history. Keep
            // CI deduplication records outside that directory across rollback.
            ci_jobs: jobs::Jobs::open(&self.root.join("ci-jobs"))?,
            storage: storage::Storage::open(&self.root, self.fleet.clone())?
                .with_github(github.clone()),
            github,
            contributors: contributors::Contributors::open(&self.root)?,
            service_tokens: service_tokens::ServiceTokens::open(&self.root)?,
            profiles: profiles::Profiles::open(&self.root)?,
            repositories: match self.repository_transport.lock().unwrap().clone() {
                Some((api_base, transport)) => {
                    repositories::Repositories::with_transport(&self.root, &api_base, transport)?
                }
                None => repositories::Repositories::open(&self.root)?,
            },
            ci: ci::Ci::open(
                &self.root,
                &self
                    .fleet
                    .hosts()
                    .into_iter()
                    .map(|host| host.target)
                    .collect::<Vec<_>>(),
            )?,
        };
        let resolver = runtime.profiles.clone();
        runtime
            .agents
            .set_profile_resolver(Arc::new(move |name, provider| {
                resolver.resolve(name, provider)
            }));
        callbacks::install(&runtime.agents);
        let minter = runtime.repositories.clone();
        let revoker = runtime.repositories.clone();
        runtime.agents.set_repository_hooks(
            Arc::new(move |reference, ttl| minter.run_credential(reference, ttl)),
            Arc::new(move |reference, credential| {
                revoker.revoke_run_credential(reference, credential)
            }),
        );
        let owner_guard = Arc::downgrade(self);
        self.fleet.set_lifecycle_guard(Arc::new(move |host, name| {
            let owner = owner_guard
                .upgrade()
                .context("Controller is shutting down")?;
            let agent_guard = owner.runtime()?.agents;
            ensure!(
                !agent_guard.active(host, name),
                "Wait for the native agent session to finish before changing this workspace"
            );
            Ok(())
        }));
        let owner_attach = Arc::downgrade(self);
        self.fleet.set_post_start(Arc::new(move |host, name| {
            let owner = owner_attach
                .upgrade()
                .context("Controller is shutting down")?;
            owner.runtime()?.agents.attach_after_start(host, name)
        }));
        runtime.agents.start_scheduler();
        gate.runtime = Some(runtime);
        gate.owner = Some(owner);
        gate.mode = Mode::Active;
        Ok(json!({"active":true,"state_schema":STATE_SCHEMA}))
    }
    /// The agent service of an active controller, for development harnesses
    /// that install a stand-in run executor (see
    /// `examples/dev_fake_executor.rs`). Not part of the HTTP API.
    /// Use another API base and transport for the repository backend (test
    /// harnesses). Applies at the next activation. Not part of the HTTP API.
    #[doc(hidden)]
    pub fn set_repository_transport(
        &self,
        api_base: &str,
        transport: Arc<dyn repositories::Transport>,
    ) {
        *self.repository_transport.lock().unwrap() = Some((api_base.to_owned(), transport));
    }
    #[doc(hidden)]
    pub fn dev_agent_service(&self) -> Result<enginebox_agents::AgentService> {
        Ok(self.runtime()?.agents)
    }
    fn runtime(&self) -> Result<Runtime> {
        self.gate
            .lock()
            .unwrap()
            .runtime
            .clone()
            .context("Controller is not active")
    }
    fn admit(self: &Arc<Self>) -> Result<Admission> {
        let mut g = self.gate.lock().unwrap();
        ensure!(
            g.mode == Mode::Active,
            "Controller is draining; retry through the supervisor"
        );
        g.requests += 1;
        Ok(Admission(self.clone()))
    }
    pub fn drain(&self) -> Value {
        let mut g = self.gate.lock().unwrap();
        if g.mode == Mode::Active {
            g.mode = Mode::Draining;
            // Queued runs stay durable for the next owner; only running ones
            // hold the drain.
            if let Some(runtime) = &g.runtime {
                runtime.agents.pause_admission();
            }
        }
        let pending = g
            .runtime
            .as_ref()
            .map_or(0, |r| r.pending_jobs() + r.agents.pending());
        if g.requests == 0 && g.background == 0 && pending == 0 {
            g.owner.take();
            g.mode = Mode::Drained;
        }
        json!({"drained":g.mode==Mode::Drained,"mode":g.mode.name(),"active_jobs":pending,"active_requests":g.requests,"background_tasks":g.background,"state_schema":STATE_SCHEMA})
    }
    pub fn status(&self) -> Value {
        let g = self.gate.lock().unwrap();
        json!({"accepting_mutations":g.mode==Mode::Active,"mode":g.mode.name(),"active_jobs":g.runtime.as_ref().map_or(0,|r|r.pending_jobs()),"active_auth_sessions":g.runtime.as_ref().map_or(0,|r|r.agents.pending()),"active_requests":g.requests,"background_tasks":g.background,"state_schema":STATE_SCHEMA,"version":env!("CARGO_PKG_VERSION"),"background_reconcile_error":self.reconcile_error.lock().unwrap().as_ref().map(|(message,at)|json!({"message":message,"at":at}))})
    }
    fn record_reconcile(&self, errors: Vec<String>) {
        let mut last = self.reconcile_error.lock().unwrap();
        if errors.is_empty() {
            *last = None;
            return;
        }
        let message = errors.join("; ");
        eprintln!("Background reconcile failed: {message}");
        *last = Some((message, chrono::Utc::now().to_rfc3339()));
    }
    pub fn start_background(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(15)).await;
                let runtime = {
                    let mut g = this.gate.lock().unwrap();
                    if g.mode != Mode::Active {
                        continue;
                    }
                    g.background += 1;
                    g.runtime.clone()
                };
                let controller = this.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    let mut errors = Vec::new();
                    if let Some(r) = &runtime
                        && let Err(e) = r.github.tick()
                    {
                        errors.push(format!("github: {e:#}"));
                    }
                    if let Some(r) = &runtime
                        && let Err(e) = r.ci.reconcile(&r.ci_jobs)
                    {
                        // Installers run in the CI pool; report queue/state failures.
                        errors.push(format!("ci: {e:#}"));
                    }
                    if let Err(e) = controller.fleet.reconcile() {
                        errors.push(format!("fleet: {e:#}"));
                    }
                    if let Some(r) = runtime
                        && let Err(e) = r.agents.reconcile()
                    {
                        errors.push(format!("agents: {e:#}"));
                    }
                    controller.record_reconcile(errors);
                })
                .await;
                this.gate.lock().unwrap().background -= 1;
            }
        })
    }
    fn authorized(&self, request: &Request<Body>) -> bool {
        let expected = format!("Bearer {}", self.token);
        let supplied = request
            .headers()
            .get(header::AUTHORIZATION)
            .map(|v| v.as_bytes())
            .unwrap_or_default();
        expected.as_bytes().ct_eq(supplied).into()
    }
    /// The owner token keeps its original check; anything else must be an
    /// active scoped service token held by the active controller.
    fn principal(
        &self,
        request: &Request<Body>,
    ) -> std::result::Result<Principal, (StatusCode, &'static str)> {
        if self.authorized(request) {
            return Ok(Principal::Owner);
        }
        let presented = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .filter(|v| v.starts_with(service_tokens::PREFIX) && v.len() <= 256);
        let Some(presented) = presented else {
            return Err((StatusCode::UNAUTHORIZED, "Authentication required."));
        };
        let Ok(runtime) = self.runtime() else {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "Controller is not active; retry through the supervisor.",
            ));
        };
        runtime
            .service_tokens
            .authenticate(presented)
            .map(|token| Principal::Service(Box::new(token)))
            .map_err(|_| {
                (
                    StatusCode::UNAUTHORIZED,
                    "Service token is unknown, expired or revoked.",
                )
            })
    }
}

/// Who is calling: the controller owner, or a scoped service token.
#[derive(Clone)]
enum Principal {
    Owner,
    Service(Box<service_tokens::Token>),
}
impl Principal {
    fn started_by(&self) -> String {
        match self {
            Self::Owner => enginebox_agents::run_details::OWNER.into(),
            Self::Service(token) => token.principal(),
        }
    }
    /// A run is visible by its workspace once placed, or by its pool.
    fn sees_run(
        &self,
        host: Option<&str>,
        name: Option<&str>,
        pool: Option<&str>,
        project_of: &dyn Fn(&str) -> Option<String>,
    ) -> bool {
        match self {
            Self::Owner => true,
            Self::Service(token) => {
                host.zip(name).is_some_and(|(h, n)| token.sees(h, n))
                    || pool.is_some_and(|p| token.sees_pool(p, project_of(p).as_deref()))
            }
        }
    }
}

/// A service token may reach only the agent-run routes it was granted, on its
/// own workspaces. Every other route, including token minting, is refused.
fn service_allows(
    token: &service_tokens::Token,
    method: &Method,
    path: &str,
    project_of: &dyn Fn(&str) -> Option<String>,
) -> bool {
    use service_tokens::Operation;
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let pool_ok =
        |operation, pool: &str| token.allows_pool(operation, pool, project_of(pool).as_deref());
    match (method, parts.as_slice()) {
        (&Method::GET, ["v1", "agent-runs"]) => token.operations.contains(&Operation::List),
        (&Method::GET, ["v1", "hosts", host, "instances", name, "agent-runs", _]) => {
            token.allows(Operation::Get, host, name)
        }
        (&Method::POST, ["v1", "hosts", host, "instances", name, "agent-runs"]) => {
            token.allows(Operation::Start, host, name)
        }
        (
            &Method::POST,
            [
                "v1",
                "hosts",
                host,
                "instances",
                name,
                "agent-runs",
                _,
                "cancel",
            ],
        ) => token.allows(Operation::Cancel, host, name),
        (&Method::POST, ["v1", "pools", pool, "agent-runs"]) => pool_ok(Operation::Start, pool),
        (&Method::GET, ["v1", "pools", pool, "agent-runs", _]) => pool_ok(Operation::Get, pool),
        (&Method::POST, ["v1", "pools", pool, "agent-runs", _, "cancel"]) => {
            pool_ok(Operation::Cancel, pool)
        }
        // Repositories: any grant on the project lists, reads and mints (a
        // read grant only read tokens, checked by the handler); creating
        // needs a write grant.
        (&Method::GET, ["v1", "projects", project, "repositories"])
        | (&Method::GET, ["v1", "projects", project, "repositories", _])
        | (&Method::POST, ["v1", "projects", project, "repositories", _, "tokens"]) => {
            token.repository_access(project).is_some()
        }
        (&Method::POST, ["v1", "projects", project, "repositories"]) => {
            token.repository_access(project) == Some(repositories::Access::Write)
        }
        _ => false,
    }
}

fn reply(code: StatusCode, value: Value) -> Response {
    (
        code,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        value.to_string(),
    )
        .into_response()
}
fn error(code: StatusCode, message: &str) -> Response {
    reply(code, json!({"error":message}))
}
pub fn router(controller: Arc<Controller>) -> Router {
    Router::new().fallback(any(handle)).with_state(controller)
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .context("Controller operation interrupted")?
}

// Dropping an HTTP request must not release ownership while its operation
// continues in a blocking task or has already been admitted to a native client.
async fn admitted<T: Send + 'static>(
    admission: Admission,
    work: impl std::future::Future<Output = Result<T>> + Send + 'static,
) -> Result<T> {
    tokio::spawn(async move {
        let _admission = admission;
        work.await
    })
    .await
    .context("Admitted operation task failed")?
}

async fn handle(State(controller): State<Arc<Controller>>, request: Request<Body>) -> Response {
    let path = request.uri().path().to_owned();
    let method = request.method().clone();
    if method == Method::GET && path == "/healthz" {
        return reply(StatusCode::OK, json!({"ok":true}));
    }
    if method == Method::GET && path == "/readyz" {
        let status = controller.status();
        return reply(
            StatusCode::OK,
            json!({"ready":true,"state_schema":STATE_SCHEMA,"version":env!("CARGO_PKG_VERSION"),"mode":status["mode"]}),
        );
    }
    if request.headers().contains_key(header::ORIGIN) {
        return error(
            StatusCode::FORBIDDEN,
            "Browser requests must use the authenticated dashboard proxy.",
        );
    }
    if path.starts_with("/v1/node/") {
        return node_request(controller, request).await;
    }
    // Only this narrow route accepts a GitHub signature instead of owner bearer auth.
    if method == Method::POST && path.starts_with("/v1/github/webhook/") {
        return github_webhook(controller, request).await;
    }
    let principal = match controller.principal(&request) {
        Ok(principal) => principal,
        Err((code, message)) => return error(code, message),
    };
    let project_of = |pool: &str| {
        controller
            .runtime()
            .ok()
            .and_then(|r| r.agents.pools().project_of(pool))
    };
    if let Principal::Service(token) = &principal
        && !service_allows(token, &method, &path, &project_of)
    {
        return error(
            StatusCode::FORBIDDEN,
            "This service token is not granted that operation on that workspace.",
        );
    }
    if request.uri().query().is_some() {
        return error(
            StatusCode::BAD_REQUEST,
            "Query parameters are not supported.",
        );
    }
    if method == Method::GET && path == "/internal/status" {
        return reply(StatusCode::OK, controller.status());
    }
    if method == Method::POST && path.starts_with("/internal/") {
        match path.as_str() {
            "/internal/drain" => {
                let result = controller.drain();
                return reply(
                    if result["drained"] == true {
                        StatusCode::OK
                    } else {
                        StatusCode::ACCEPTED
                    },
                    result,
                );
            }
            "/internal/activate" | "/internal/undrain" => {
                return match blocking(move || controller.activate()).await {
                    Ok(v) => reply(StatusCode::OK, v),
                    Err(_) => error(
                        StatusCode::CONFLICT,
                        "Controller activation failed; state remains owned by its current controller.",
                    ),
                };
            }
            _ => return error(StatusCode::NOT_FOUND, "Unknown endpoint."),
        }
    }
    let parts: Vec<String> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    if format!("/{}", parts.join("/")) != path {
        return error(StatusCode::NOT_FOUND, "Unknown endpoint.");
    }
    if method == Method::GET {
        // Native auth/status programs may refresh their own home. Keep them in
        // the ownership barrier; serve the last observation during handoff.
        let native_observation = path == "/v1/agents"
            || path.ends_with("/usage")
            || path.ends_with("/accounts")
            || path.starts_with("/v1/github");
        let admission = if native_observation {
            match controller.admit() {
                Ok(a) => Some(a),
                Err(_) => {
                    return match controller.observations.lock().unwrap().get(&path).cloned() {
                        Some(v) => reply(
                            StatusCode::OK,
                            if path.ends_with("/usage") {
                                enginebox_agents::usage::handoff_snapshot(&v)
                            } else {
                                v
                            },
                        ),
                        None => error(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "Agent observation is pending controller activation.",
                        ),
                    };
                }
            }
        } else {
            None
        };
        let operation = controller.clone();
        let reader = principal.clone();
        let result = blocking(move || {
            let _admission = admission;
            read(&operation, &parts, &reader)
        })
        .await;
        if let Ok(value) = &result
            && native_observation
        {
            controller
                .observations
                .lock()
                .unwrap()
                .insert(path, value.clone());
        }
        return match result {
            Ok(v) => reply(StatusCode::OK, v),
            Err(_) => error(
                StatusCode::NOT_FOUND,
                "Resource unavailable or unknown endpoint.",
            ),
        };
    }
    // The owner-only backend configuration is the one PUT/DELETE resource.
    let backend_route = path == "/v1/repository-backends/artifacts";
    if method == Method::DELETE && backend_route {
        let admission = match controller.admit() {
            Ok(a) => a,
            Err(_) => {
                return error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Controller is draining; retry through the supervisor.",
                );
            }
        };
        let operation = controller.clone();
        return match blocking(move || {
            let _admission = admission;
            operation.runtime()?.repositories.delete_artifacts()
        })
        .await
        {
            Ok(v) => reply(StatusCode::OK, v),
            Err(_) => error(
                StatusCode::CONFLICT,
                "Backend configuration could not be removed.",
            ),
        };
    }
    if method != Method::POST && !(method == Method::PUT && backend_route) {
        return error(StatusCode::METHOD_NOT_ALLOWED, "Use GET or POST.");
    }
    if request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        != Some("application/json")
    {
        return error(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Use application/json.");
    }
    if request.headers().contains_key(header::TRANSFER_ENCODING) {
        return error(
            StatusCode::BAD_REQUEST,
            "Chunked requests are not supported.",
        );
    }
    let maximum = if path == "/v1/templates" {
        1024 * 1024
    } else if path == "/v1/profile-files" {
        // One file of up to 1 MiB, base64-encoded.
        1536 * 1024
    } else if path == "/v1/profiles" {
        256 * 1024
    } else if path == "/v1/ci/templates" || path == "/v1/pools" {
        65536
    } else {
        16384
    };
    let body = match to_bytes(request.into_body(), maximum).await {
        Ok(b) if !b.is_empty() => b,
        _ => {
            return error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Request body is missing or too large.",
            );
        }
    };
    let body = match enginebox_core::templates::decode_json(&body) {
        Ok(v) if v.is_object() => v,
        _ => return error(StatusCode::BAD_REQUEST, "Body must be a JSON object."),
    };
    let admission = match controller.admit() {
        Ok(a) => a,
        Err(_) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Controller is draining; retry through the supervisor.",
            );
        }
    };
    let result = if method == Method::PUT {
        let operation = controller.clone();
        admitted(admission, async move {
            let repositories = operation.runtime()?.repositories;
            Ok(repository_reply(
                blocking(move || repositories.put_artifacts(&body)).await,
                StatusCode::OK,
            ))
        })
        .await
    } else {
        admitted(admission, write(controller, parts, body, principal)).await
    };
    match result {
        Ok((StatusCode::TOO_MANY_REQUESTS, v)) => {
            let mut response = reply(StatusCode::TOO_MANY_REQUESTS, v.clone());
            if let Some(seconds) = v["retry_after_seconds"].as_u64() {
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, header::HeaderValue::from(seconds));
            }
            response
        }
        Ok((code, v)) => reply(code, v),
        Err(_) => error(
            StatusCode::BAD_REQUEST,
            "Request could not be admitted. Check its fields, workspace state and configured capacity.",
        ),
    }
}

async fn node_request(controller: Arc<Controller>, request: Request<Body>) -> Response {
    let action = request
        .uri()
        .path()
        .strip_prefix("/v1/node/")
        .unwrap_or("")
        .to_owned();
    if request.method() != Method::POST
        || request.uri().query().is_some()
        || !["enroll", "hello", "heartbeat", "poll", "result"].contains(&action.as_str())
    {
        return error(StatusCode::NOT_FOUND, "Unknown node operation.");
    }
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_owned();
    if token.len() > 200 || token.is_empty() {
        return error(StatusCode::UNAUTHORIZED, "Node credential required.");
    }
    let admission = {
        let mut gate = controller.gate.lock().unwrap();
        // Already admitted work must keep its lease and finish during a drain.
        let finishing = ["heartbeat", "result"].contains(&action.as_str());
        if gate.mode != Mode::Active && !(finishing && gate.mode == Mode::Draining) {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Controller is not accepting node work.",
            );
        }
        gate.requests += 1;
        Admission(controller.clone())
    };
    let runtime = match controller.runtime() {
        Ok(r) => r,
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "Controller unavailable."),
    };
    if action != "enroll" && !runtime.contributors.authenticated(&token) {
        return error(
            StatusCode::UNAUTHORIZED,
            "Node credential expired or revoked.",
        );
    }
    let bytes = match to_bytes(request.into_body(), 350_000).await {
        Ok(b) => b,
        Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "Node request too large."),
    };
    let body = match enginebox_core::templates::decode_json(&bytes) {
        Ok(v) if v.is_object() => v,
        _ => return error(StatusCode::BAD_REQUEST, "Invalid node request."),
    };
    let result = admitted(admission, async move {
        blocking(move || {
            if action == "enroll" {
                runtime
                    .contributors
                    .enroll(&token, serde_json::from_value(body)?)
            } else {
                runtime.contributors.node_call(&token, &action, body)
            }
        })
        .await
    })
    .await;
    match result {
        Ok(v) => reply(StatusCode::OK, v),
        Err(_) => error(
            StatusCode::CONFLICT,
            "Node request rejected. Check registration expiry, resource allowance, session or reconciliation state.",
        ),
    }
}

fn read(c: &Controller, p: &[String], principal: &Principal) -> Result<Value> {
    let s: Vec<_> = p.iter().map(String::as_str).collect();
    match s.as_slice() {
        ["v1", "github"] => Ok(c.runtime()?.github.status()),
        ["v1", "github", kind, "installations"] => c.runtime()?.github.list_installations(kind),
        ["v1", "github", "installations", id, "repositories"] => {
            c.runtime()?.github.repositories(id.parse()?)
        }
        ["v1", "fleet"] => c.fleet.summary(),
        ["v1", "templates"] => c.fleet.catalog(),
        ["v1", "storage"] => Ok(c.runtime()?.storage.catalog()),
        ["v1", "ci"] => Ok(c.runtime()?.ci.catalog()),
        ["v1", "ci", "templates"] => Ok(c.runtime()?.ci.template_catalog()),
        ["v1", "contributors"] => Ok(c.runtime()?.contributors.catalog()),
        ["v1", "ci", "github", owner, repo, "runs", run] => {
            enginebox_ci::github::Github::from_env()?
                .status(&format!("{owner}/{repo}"), run.parse()?)
        }
        ["v1", "agents"] => c.runtime()?.agents.status(),
        ["v1", "agent-runs"] => {
            let agents = c.runtime()?.agents;
            let project_of = |pool: &str| agents.pools().project_of(pool);
            Ok(agents.list_runs_visible(|host, name, pool| {
                principal.sees_run(host, name, pool, &project_of)
            }))
        }
        ["v1", "pools"] => Ok(c.runtime()?.agents.list_pools()),
        ["v1", "pools", pool] => c.runtime()?.agents.pool_status(pool),
        ["v1", "projects", project, "pools"] => c.runtime()?.agents.list_project_pools(project),
        ["v1", "launch-events"] => Ok(c
            .runtime()?
            .agents
            .launch_events(&enginebox_agents::launch_events::Filter::default())),
        ["v1", "pools", pool, "launch-events"] => {
            Ok(c.runtime()?
                .agents
                .launch_events(&enginebox_agents::launch_events::Filter {
                    pool: Some(pool.to_string()),
                    ..Default::default()
                }))
        }
        ["v1", "projects", project, "launch-events"] => {
            Ok(c.runtime()?
                .agents
                .launch_events(&enginebox_agents::launch_events::Filter {
                    project: Some(project.to_string()),
                    ..Default::default()
                }))
        }
        ["v1", "pools", pool, "agent-runs", id] => {
            Ok(json!({"run":c.runtime()?.agents.get_pool_run(pool,id)?}))
        }
        ["v1", "service-tokens"] => {
            let runtime = c.runtime()?;
            let agents = runtime.agents.clone();
            Ok(runtime
                .service_tokens
                .list_grouped(|pool| agents.pools().project_of(pool)))
        }
        ["v1", "repository-backends", "artifacts"] => Ok(c.runtime()?.repositories.get_artifacts()),
        ["v1", "repositories"] => Ok(c.runtime()?.repositories.list_all()),
        ["v1", "projects", project, "repositories"] => c.runtime()?.repositories.list(project),
        ["v1", "projects", project, "repositories", name] => {
            c.runtime()?.repositories.get(project, name)
        }
        ["v1", "profiles"] => Ok(c.runtime()?.profiles.list()),
        ["v1", "profiles", name] => c.runtime()?.profiles.get(name),
        ["v1", "agents", provider, "usage"] => {
            Ok(json!({"usage":c.runtime()?.agents.usage(provider)?}))
        }
        ["v1", "agents", provider, "sessions", session] => {
            Ok(json!({"session":c.runtime()?.agents.session(provider,session)?}))
        }
        ["v1", "jobs", id] => {
            let runtime = c.runtime()?;
            Ok(json!({"job":runtime.jobs.get(id).or_else(|_| runtime.ci_jobs.get(id))?}))
        }
        ["v1", "hosts", host, "instances", name, "agent-runs", id] => {
            Ok(json!({"run":c.runtime()?.agents.get_run(host,name,id)?}))
        }
        ["v1", "hosts", host, "instances", name, "accounts"] => {
            c.runtime()?.agents.workspace_status(host, name)
        }
        _ => anyhow::bail!("Unknown endpoint"),
    }
}
fn empty(body: &Value) -> Result<()> {
    ensure!(
        body.as_object().is_some_and(|m| m.is_empty()),
        "Request accepts an empty object"
    );
    Ok(())
}
fn keys(body: &Value, allowed: &[&str], required: &[&str]) -> Result<()> {
    let object = body.as_object().context("Expected object")?;
    ensure!(
        object.keys().all(|k| allowed.contains(&k.as_str()))
            && required.iter().all(|k| object.contains_key(*k)),
        "Unsupported fields"
    );
    Ok(())
}
fn string<'a>(body: &'a Value, key: &str) -> Result<&'a str> {
    body[key].as_str().context("Expected string field")
}

async fn write(
    c: Arc<Controller>,
    parts: Vec<String>,
    body: Value,
    principal: Principal,
) -> Result<(StatusCode, Value)> {
    if parts.get(1).is_some_and(|p| p == "github") {
        let github = c.runtime()?.github;
        let value = blocking(move || -> Result<Value> {
            let p: Vec<_> = parts.iter().map(String::as_str).collect();
            match p.as_slice() {
                ["v1", "github", kind, "connect"] => {
                    empty(&body)?;
                    github.connect(kind)
                }
                ["v1", "github", kind, "disconnect"] => {
                    empty(&body)?;
                    github.disconnect(kind)
                }
                ["v1", "github", "callback"] => {
                    keys(&body, &["state", "code"], &["state", "code"])?;
                    github.callback(string(&body, "state")?, string(&body, "code")?)
                }
                ["v1", "github", "select"] => {
                    keys(
                        &body,
                        &["installation_id", "repository_id", "branch"],
                        &["installation_id", "repository_id", "branch"],
                    )?;
                    github.select(
                        enginebox_github::number(&body, "installation_id")?,
                        enginebox_github::number(&body, "repository_id")?,
                        string(&body, "branch")?,
                    )
                }
                ["v1", "github", "remove"] => {
                    keys(&body, &["repository_id"], &["repository_id"])?;
                    github.remove(enginebox_github::number(&body, "repository_id")?)
                }
                ["v1", "github", "drain"] => {
                    keys(&body, &["draining"], &["draining"])?;
                    github.set_draining(
                        body["draining"]
                            .as_bool()
                            .context("draining must be boolean")?,
                    )
                }
                _ => anyhow::bail!("Unknown GitHub operation"),
            }
        })
        .await?;
        return Ok((StatusCode::OK, value));
    }

    let p: Vec<_> = parts.iter().map(String::as_str).collect();
    let runtime = c.runtime()?;
    let mut code = StatusCode::OK;
    let v = match p.as_slice() {
        ["v1", "ci", "nodes"] => {
            blocking(move || {
                let result = runtime.ci.register(body)?;
                let _ = runtime.ci.reconcile(&runtime.ci_jobs);
                Ok(result)
            })
            .await?
        }
        ["v1", "ci", "templates"] => {
            keys(&body, &["manifest"], &["manifest"])?;
            blocking(move || {
                runtime
                    .ci
                    .register_template(serde_json::from_value(body["manifest"].clone())?)?;
                let _ = runtime.ci.reconcile(&runtime.ci_jobs);
                Ok(runtime.ci.template_catalog())
            })
            .await?
        }
        [
            "v1",
            "ci",
            "templates",
            id,
            action @ ("enable" | "disable" | "retry"),
        ] => {
            let id = id.to_string();
            let action = action.to_string();
            blocking(move || {
                if action == "retry" {
                    keys(&body, &["node"], &["node"])?;
                    runtime.ci.retry_template(&id, string(&body, "node")?)?;
                } else {
                    empty(&body)?;
                    runtime.ci.enable_template(&id, action == "enable")?;
                }
                let _ = runtime.ci.reconcile(&runtime.ci_jobs);
                Ok(runtime.ci.template_catalog())
            })
            .await?
        }
        ["v1", "contributors", "enrollments"] => {
            runtime.contributors.issue(serde_json::from_value(body)?)?
        }
        ["v1", "contributors", node, "renew-enrollment"] => {
            empty(&body)?;
            runtime.contributors.renew_enrollment(node)?
        }
        ["v1", "contributors", node, "revoke"] => {
            empty(&body)?;
            runtime.contributors.revoke(node)?
        }
        ["v1", "contributors", node, "reconcile"] => {
            keys(&body, &["stopped"], &["stopped"])?;
            ensure!(body["stopped"] == true, "Confirm the owned worker stopped");
            runtime.contributors.reconcile(node)?
        }
        ["v1", "contributors", node, "runs"] => {
            code = StatusCode::ACCEPTED;
            json!({"job":runtime.contributors.submit(&runtime.ci_jobs,node,serde_json::from_value(body)?)?})
        }
        ["v1", "ci", "plan"] => runtime.ci.plan(&serde_json::from_value(body)?)?,
        ["v1", "ci", "runs"] => {
            code = StatusCode::ACCEPTED;
            json!({"job":runtime.ci.submit(&runtime.ci_jobs, serde_json::from_value(body)?)?})
        }
        ["v1", "ci", "github", owner, repo, "runs", run, "cancel"] => {
            empty(&body)?;
            let repository = format!("{owner}/{repo}");
            let run = run.parse()?;
            blocking(move || enginebox_ci::github::Github::from_env()?.cancel(&repository, run))
                .await?
        }
        ["v1", "storage", "hosts", host, operation] => {
            let plan = runtime.storage.plan(host, operation, &body)?;
            let operation = plan.operation.clone();
            let name = plan.name.clone();
            code = StatusCode::ACCEPTED;
            json!({"job":runtime.jobs.submit(host,&operation,&name,move||plan.execute())?})
        }
        ["v1", "agents", provider, "login"] => {
            empty(&body)?;
            let provider = provider.to_string();
            code = StatusCode::ACCEPTED;
            json!({"session":blocking(move||runtime.agents.start(&provider)).await?})
        }
        ["v1", "agents", provider, "sessions", session, action] => {
            let provider = provider.to_string();
            let session = session.to_string();
            let result = match *action {
                "input" => {
                    keys(&body, &["text"], &["text"])?;
                    let text = string(&body, "text")?.to_owned();
                    blocking(move || runtime.agents.input(&provider, &session, &text)).await?
                }
                "cancel" => {
                    empty(&body)?;
                    blocking(move || runtime.agents.cancel(&provider, &session)).await?
                }
                _ => anyhow::bail!("Unknown session operation"),
            };
            json!({"session":result})
        }
        [
            "v1",
            "hosts",
            host,
            "instances",
            name,
            "agent-runs",
            id,
            "cancel",
        ] => {
            empty(&body)?;
            let (host, name, id) = (host.to_string(), name.to_string(), id.to_string());
            code = StatusCode::ACCEPTED;
            json!({"run":blocking(move||runtime.agents.cancel_run(&host,&name,&id)).await?})
        }
        ["v1", "hosts", host, "instances", name, "agent-runs"] => {
            identifier(host, false)?;
            identifier(name, true)?;
            let target = RunTarget::Workspace(host.to_string(), name.to_string());
            return start_run(&runtime, target, body, &principal).await;
        }
        ["v1", "pools", pool, "agent-runs"] => {
            let target = RunTarget::Pool(pool.to_string());
            return start_run(&runtime, target, body, &principal).await;
        }
        ["v1", "pools", pool, "agent-runs", id, "cancel"] => {
            empty(&body)?;
            let (pool, id) = (pool.to_string(), id.to_string());
            code = StatusCode::ACCEPTED;
            json!({"run":blocking(move||runtime.agents.cancel_pool_run(&pool,&id)).await?})
        }
        ["v1", "pools"] => {
            let fleet = c.fleet.clone();
            if let Some(profile) = body.get("profile").and_then(Value::as_str) {
                ensure!(runtime.profiles.exists(profile), "Unknown profile");
            }
            runtime
                .agents
                .pools()
                .put(body, &|h| fleet.hosts().iter().any(|x| x.id == h))?
        }
        ["v1", "pools", pool, "delete"] => {
            empty(&body)?;
            runtime.agents.delete_pool(pool)?
        }
        ["v1", "projects", project, "repositories"] => {
            let repositories = runtime.repositories.clone();
            let project = project.to_string();
            let outcome = blocking(move || repositories.create(&project, &body)).await;
            return Ok(match outcome {
                Ok((true, v)) => (StatusCode::CREATED, v),
                Ok((false, v)) => (StatusCode::OK, v),
                Err(e) => repository_reply(Err(e), StatusCode::OK),
            });
        }
        ["v1", "projects", project, "repositories", name, "tokens"] => {
            let (access, ttl) = match repositories::Repositories::token_request(&body) {
                Ok(request) => request,
                Err(e) => return Ok(repository_reply(Err(e), StatusCode::OK)),
            };
            if let Principal::Service(token) = &principal
                && access == repositories::Access::Write
                && token.repository_access(project) != Some(repositories::Access::Write)
            {
                return Ok((
                    StatusCode::FORBIDDEN,
                    json!({"error":"This service token may mint only read tokens for this project's repositories."}),
                ));
            }
            let repositories = runtime.repositories.clone();
            let (project, name) = (project.to_string(), name.to_string());
            return Ok(repository_reply(
                blocking(move || repositories.mint(&project, &name, access, ttl)).await,
                StatusCode::CREATED,
            ));
        }
        ["v1", "profile-files"] => {
            code = StatusCode::CREATED;
            runtime.profiles.upload(&body)?
        }
        ["v1", "profiles"] => runtime.profiles.put(body)?,
        ["v1", "profiles", name, "delete"] => {
            empty(&body)?;
            runtime.profiles.delete(name)?
        }
        ["v1", "service-tokens"] => {
            let request: service_tokens::MintRequest = serde_json::from_value(body)?;
            let fleet = c.fleet.clone();
            code = StatusCode::CREATED;
            let profiles = runtime.profiles.clone();
            let agents = runtime.agents.clone();
            runtime.service_tokens.mint(
                request,
                |host| fleet.hosts().iter().any(|h| h.id == host),
                |profile| profiles.exists(profile),
                |pool| agents.pools().project_of(pool),
            )?
        }
        ["v1", "service-tokens", id, "revoke"] => {
            empty(&body)?;
            runtime.service_tokens.revoke(id)?
        }
        ["v1", "templates"] => {
            let fleet = c.fleet.clone();
            let record = if body.as_object().unwrap().len() == 1 && body["manifest"].is_object() {
                blocking(move || fleet.register_template(body["manifest"].clone())).await?
            } else {
                keys(&body, &["url"], &["url"])?;
                let url = string(&body, "url")?.to_owned();
                blocking(move || fleet.import_template(&url)).await?
            };
            json!({"template":record})
        }
        ["v1", "templates", template, "sync"] => {
            empty(&body)?;
            identifier(template, false)?;
            let mut jobs = vec![];
            for host in c.fleet.hosts() {
                jobs.push(submit(
                    &c,
                    &runtime.jobs,
                    &host.id,
                    "install_template",
                    json!({"template":template}),
                )?);
            }
            code = StatusCode::ACCEPTED;
            json!({"template":template,"jobs":jobs})
        }
        [
            "v1",
            "hosts",
            host,
            operation @ ("instances" | "allocations"),
        ] => {
            keys(
                &body,
                &["name", "template", "size", "resources"],
                &["name", "template"],
            )?;
            let op = if *operation == "instances" {
                "create"
            } else {
                "allocate"
            };
            code = StatusCode::ACCEPTED;
            json!({"job":submit(&c,&runtime.jobs,host,op,body)?})
        }
        ["v1", "hosts", host, "templates", template, "install"] => {
            empty(&body)?;
            code = StatusCode::ACCEPTED;
            json!({"job":submit(&c,&runtime.jobs,host,"install_template",json!({"template":template}))?})
        }
        ["v1", "hosts", host, "instances", name, "actions"] => {
            keys(&body, &["action"], &["action"])?;
            let action = string(&body, "action")?;
            ensure!(
                ["pause", "resume", "hibernate", "stop"].contains(&action),
                "Unsupported action"
            );
            code = StatusCode::ACCEPTED;
            json!({"job":submit(&c,&runtime.jobs,host,action,json!({"name":name}))?})
        }
        ["v1", "hosts", _, "instances", _, "accounts", _, "login"] => {
            return Ok((
                StatusCode::CONFLICT,
                json!({"error":"Connect agents on the controller through /v1/agents; workspaces receive access automatically."}),
            ));
        }
        _ => return Ok((StatusCode::NOT_FOUND, json!({"error":"Unknown endpoint."}))),
    };
    Ok((code, v))
}
/// A repository operation's reply: its refusal status and message, or `ok`.
fn repository_reply(result: Result<Value>, ok: StatusCode) -> (StatusCode, Value) {
    match result {
        Ok(v) => (ok, v),
        Err(e) => {
            let (status, message) = repositories::status_of(&e);
            (
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),
                json!({"error":message}),
            )
        }
    }
}
enum RunTarget {
    Workspace(String, String),
    Pool(String),
}

/// Accept a native run into the durable queue: HTTP 202 (`queued`), 200 for
/// a replayed idempotency key, 403/404 for profile or pool refusals, and 429
/// with `Retry-After` only when a hard queue bound is reached.
async fn start_run(
    runtime: &Runtime,
    target: RunTarget,
    mut body: Value,
    principal: &Principal,
) -> Result<(StatusCode, Value)> {
    let object = body.as_object_mut().context("Expected object")?;
    // v2 fields are controller-side; the native request stays as before.
    let mut profile_name = match object.remove("profile") {
        None | Some(Value::Null) => None,
        Some(Value::String(p)) => Some(p),
        Some(_) => anyhow::bail!("profile must be a string"),
    };
    let idempotency_key = match object.remove("idempotency_key") {
        None | Some(Value::Null) => None,
        Some(Value::String(k)) => Some(k),
        Some(_) => anyhow::bail!("idempotency_key must be a string"),
    };
    let repository = match object.remove("repository") {
        None | Some(Value::Null) => None,
        Some(Value::String(r)) => Some(r),
        Some(_) => anyhow::bail!("repository must be a string"),
    };
    let callback_url = match object.remove("callback") {
        None | Some(Value::Null) => None,
        Some(callback) => {
            keys(&callback, &["url"], &["url"])?;
            Some(string(&callback, "url")?.to_owned())
        }
    };
    // A pool's profile applies to every run in it; granting the pool grants
    // its profile.
    let mut pool_profile = false;
    if let RunTarget::Pool(pool) = &target {
        let Some(definition) = runtime.agents.pools().get(pool) else {
            return Ok((StatusCode::NOT_FOUND, json!({"error":"Unknown pool."})));
        };
        if let Some(fixed) = definition.profile {
            if profile_name.as_ref().is_some_and(|p| *p != fixed) {
                anyhow::bail!("Runs in this pool use the pool's profile");
            }
            profile_name = Some(fixed);
            pool_profile = true;
        }
    }
    if let Some(profile) = &profile_name {
        if let Principal::Service(token) = principal
            && !pool_profile
            && !token.allows_profile(profile)
        {
            return Ok((
                StatusCode::FORBIDDEN,
                json!({"error":"This service token is not granted that profile."}),
            ));
        }
        if !runtime.profiles.exists(profile) {
            return Ok((StatusCode::NOT_FOUND, json!({"error":"Unknown profile."})));
        }
    }
    if let Some(reference) = &repository {
        let Ok((project, name)) = enginebox_agents::repository::parse_ref(reference) else {
            return Ok((
                StatusCode::BAD_REQUEST,
                json!({"error":"repository must be <project>/<name>."}),
            ));
        };
        if let Principal::Service(token) = principal
            && token.repository_access(&project) != Some(repositories::Access::Write)
        {
            return Ok((
                StatusCode::FORBIDDEN,
                json!({"error":"This service token is not granted write access to that project's repositories."}),
            ));
        }
        if let RunTarget::Pool(pool) = &target
            && runtime.agents.pools().project_of(pool).as_deref() != Some(project.as_str())
        {
            return Ok((
                StatusCode::FORBIDDEN,
                json!({"error":"A pool run may use only a repository in the pool's project."}),
            ));
        }
        if !runtime.repositories.exists(&project, &name) {
            return Ok((
                StatusCode::NOT_FOUND,
                json!({"error":"Unknown repository."}),
            ));
        }
        if runtime.repositories.backend().is_err() {
            return Ok((
                StatusCode::CONFLICT,
                json!({"error":"No repository backend is configured."}),
            ));
        }
    }
    let resolved = match &profile_name {
        Some(profile) => Some(
            runtime
                .profiles
                .resolve(profile, string(&body, "provider")?)?,
        ),
        None => None,
    };
    let callback = match callback_url {
        Some(url) => {
            callbacks::validate_url(&url)?;
            // A service token signs with its own key; the owner signs with
            // the profile's. Keys are never taken from the request.
            let secret = match principal {
                Principal::Service(token) => token.callback_secret().map(str::to_owned),
                Principal::Owner => profile_name
                    .as_deref()
                    .and_then(|p| runtime.profiles.callback_secret(p)),
            }
            .context("No callback key is configured for this caller")?;
            Some(enginebox_agents::service::Callback { url, secret })
        }
        None => None,
    };
    let options = enginebox_agents::service::LaunchOptions {
        profile: resolved,
        idempotency_key,
        callback,
        repository,
    };
    let started_by = principal.started_by();
    let agents = runtime.agents.clone();
    let outcome = blocking(move || {
        Ok(match target {
            RunTarget::Workspace(host, name) => {
                agents.launch_with(&host, &name, body, &started_by, options)
            }
            RunTarget::Pool(pool) => agents.launch_pool(&pool, body, &started_by, options),
        })
    })
    .await?;
    match outcome {
        Ok((run, true)) => Ok((StatusCode::OK, json!({"run":run,"replayed":true}))),
        Ok((run, false)) => Ok((StatusCode::ACCEPTED, json!({"run":run}))),
        Err(error) => match error.downcast_ref::<enginebox_agents::queue::QueueFull>() {
            Some(full) => Ok((
                StatusCode::TOO_MANY_REQUESTS,
                json!({"error":full.to_string(),"retry_after_seconds":full.retry_after_seconds}),
            )),
            None => Err(error),
        },
    }
}

fn submit(
    c: &Controller,
    jobs: &jobs::Jobs,
    host: &str,
    operation: &str,
    params: Value,
) -> Result<Value> {
    c.fleet.validate_operation(host, operation, &params)?;
    let name = params[if operation == "install_template" {
        "template"
    } else {
        "name"
    }]
    .as_str()
    .context("Required name missing")?
    .to_owned();
    let fleet = c.fleet.clone();
    let host_id = host.to_owned();
    let op = operation.to_owned();
    jobs.submit(host, operation, &name, move || {
        fleet.operate(&host_id, &op, &params)
    })
}

async fn github_webhook(c: Arc<Controller>, request: Request<Body>) -> Response {
    let kind = request
        .uri()
        .path()
        .trim_start_matches("/v1/github/webhook/")
        .to_owned();
    if !["repos", "runners"].contains(&kind.as_str()) || request.uri().query().is_some() {
        return error(StatusCode::NOT_FOUND, "Unknown webhook.");
    }
    let event = request
        .headers()
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let signature = request
        .headers()
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let body = match to_bytes(request.into_body(), 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "Webhook too large."),
    };
    let admission = match c.admit() {
        Ok(a) => a,
        Err(_) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Controller is draining; redeliver this event.",
            );
        }
    };
    let result = admitted(admission, async move {
        blocking(move || {
            c.runtime()?
                .github
                .webhook(&kind, &event, &signature, &body)
        })
        .await
    })
    .await;
    match result {
        Ok(value) => reply(StatusCode::ACCEPTED, value),
        Err(_) => error(
            StatusCode::BAD_REQUEST,
            "Webhook rejected; check signature, installation, payload and inbox capacity.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;
    fn setup() -> (tempfile::TempDir, Arc<Controller>) {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("fleet.toml");
        std::fs::write(
            &config,
            "schema_version=1\n[[hosts]]\nid='worker'\ntarget='unused-fixture-host'\n",
        )
        .unwrap();
        let c = Controller::new(&config, &temp.path().join("state"), "x".repeat(32)).unwrap();
        (temp, c)
    }
    fn request(method: Method, path: &str, body: &str, authorized: bool) -> Request<Body> {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json");
        if authorized {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {}", "x".repeat(32)));
        }
        builder.body(Body::from(body.to_owned())).unwrap()
    }
    #[tokio::test]
    async fn auth_and_origin_are_checked_before_body_or_state() {
        let (_temp, c) = setup();
        let app = router(c);
        assert_eq!(
            app.clone()
                .oneshot(request(Method::GET, "/healthz", "", false))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            app.clone()
                .oneshot(request(Method::GET, "/v1/agents", "", false))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let mut browser = request(Method::POST, "/internal/activate", "{}", true);
        browser
            .headers_mut()
            .insert(header::ORIGIN, "https://example.com".parse().unwrap());
        assert_eq!(
            app.oneshot(browser).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }
    #[tokio::test]
    async fn github_is_optional_and_webhook_exception_cannot_bypass_owner_auth() {
        let (_temp, c) = setup();
        let app = router(c.clone());
        assert_eq!(
            app.clone()
                .oneshot(request(Method::GET, "/v1/github", "", false))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            app.clone()
                .oneshot(request(
                    Method::POST,
                    "/v1/github/repos/connect",
                    "{}",
                    true
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        c.activate().unwrap();
        let response = app
            .clone()
            .oneshot(request(Method::GET, "/v1/github", "", true))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(body["configured"], false);
        assert!(!c.root.join("github").exists());
        assert_eq!(
            app.clone()
                .oneshot(request(
                    Method::POST,
                    "/v1/github/webhook/runners",
                    "{}",
                    false
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            app.clone()
                .oneshot(request(Method::POST, "/v1/github/select", "{}", false))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        c.drain();
        assert_eq!(
            app.oneshot(request(
                Method::POST,
                "/v1/github/webhook/runners",
                "{}",
                false
            ))
            .await
            .unwrap()
            .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
    #[tokio::test]
    async fn ci_routes_require_auth_and_obey_controller_activation_and_drain() {
        let (_temp, c) = setup();
        let app = router(c.clone());
        let node = json!({"schema_version":1,"id":"mac-builder","target":"builder@dedicated-ci.example","os":"macos","architecture":"arm64","image":"xcode-v1","capabilities":["xcode"]}).to_string();
        let trigger = json!({"schema_version":1,"request_id":"merge-ios","event":"merge","sha":"a".repeat(40),"job":{"runner":"automation","runs-on":["macos","xcode"],"shell":"bash","run":"true","timeout-minutes":1}}).to_string();
        for path in [
            "/v1/ci/nodes",
            "/v1/ci/plan",
            "/v1/ci/runs",
            "/v1/ci/templates",
            "/v1/ci/templates/example/enable",
            "/v1/ci/templates/example/disable",
            "/v1/ci/templates/example/retry",
        ] {
            assert_eq!(
                app.clone()
                    .oneshot(request(Method::POST, path, &trigger, false))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::UNAUTHORIZED
            );
        }
        for path in [
            "/v1/ci/nodes",
            "/v1/ci/templates",
            "/v1/ci/templates/example/enable",
        ] {
            assert_eq!(
                app.clone()
                    .oneshot(request(Method::POST, path, &node, true))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::SERVICE_UNAVAILABLE
            );
        }
        c.activate().unwrap();
        assert_eq!(
            app.clone()
                .oneshot(request(Method::POST, "/v1/ci/nodes", &node, true))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let response = app
            .clone()
            .oneshot(request(Method::POST, "/v1/ci/plan", &trigger, true))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let value: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 16384).await.unwrap()).unwrap();
        assert_eq!(value["node_id"], "mac-builder");
        c.drain();
        for path in [
            "/v1/ci/templates",
            "/v1/ci/templates/example/enable",
            "/v1/ci/templates/example/retry",
        ] {
            assert_eq!(
                app.clone()
                    .oneshot(request(Method::POST, path, "{}", true))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::SERVICE_UNAVAILABLE
            );
        }
        assert_eq!(
            app.oneshot(request(Method::POST, "/v1/ci/runs", &trigger, true))
                .await
                .unwrap()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
    #[tokio::test]
    async fn agent_run_listing_and_cancellation_require_operator_auth() {
        let (_temp, c) = setup();
        let app = router(c.clone());
        let cancel = "/v1/hosts/worker/instances/ebox-private/agent-runs/11111111111111111111111111111111/cancel";
        for (method, path, body) in [
            (Method::GET, "/v1/agent-runs", ""),
            (Method::POST, cancel, "{}"),
        ] {
            assert_eq!(
                app.clone()
                    .oneshot(request(method, path, body, false))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::UNAUTHORIZED
            );
        }
        c.activate().unwrap();
        let response = app
            .clone()
            .oneshot(request(Method::GET, "/v1/agent-runs", "", true))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let payload: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(payload, json!({"runs":[],"total":0,"truncated":false}));
        // Unknown runs are rejected before attempting to contact a registered host.
        assert_eq!(
            app.oneshot(request(Method::POST, cancel, "{}", true))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }

    fn bearer(method: Method, path: &str, body: &str, token: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(body.to_owned()))
            .unwrap()
    }
    async fn call(app: &Router, request: Request<Body>) -> (StatusCode, Value) {
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
    async fn mint(app: &Router, body: Value) -> (String, String) {
        let (status, minted) = call(
            app,
            request(Method::POST, "/v1/service-tokens", &body.to_string(), true),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{minted}");
        assert!(minted["token"].get("secret_hash").is_none());
        (
            minted["token"]["id"].as_str().unwrap().to_owned(),
            minted["secret"].as_str().unwrap().to_owned(),
        )
    }
    const RUNS_A: &str = "/v1/hosts/worker/instances/ebox-a/agent-runs";
    const RUNS_B: &str = "/v1/hosts/worker/instances/ebox-b/agent-runs";

    #[tokio::test]
    async fn service_tokens_reach_only_granted_agent_run_operations_on_their_workspaces() {
        let (_temp, c) = setup();
        let app = router(c.clone());
        let all = json!({"label":"tenant-a","scopes":[{"host_id":"worker","name":"ebox-a"}],
            "operations":["agent_runs.start","agent_runs.get","agent_runs.list","agent_runs.cancel"]});
        // Minting needs the active controller; a standby refuses service tokens.
        assert_eq!(
            call(
                &app,
                request(Method::POST, "/v1/service-tokens", &all.to_string(), true)
            )
            .await
            .0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        c.activate().unwrap();
        for bad in [
            json!({"label":"x","scopes":[{"host_id":"unknown","name":"ebox-a"}],"operations":["agent_runs.get"]}),
            json!({"label":"x","scopes":[{"host_id":"worker","name":"ebox-a"}],"operations":["hosts.admin"]}),
            json!({"label":"x","scopes":[{"host_id":"worker","name":"ebox-a"}],"operations":[]}),
            json!({"label":"x","scopes":[{"host_id":"worker","name":"ebox-a"}],"operations":["agent_runs.get"],"expires_at":"2000-01-01T00:00:00Z"}),
            json!({"label":"x","scopes":[{"host_id":"worker","name":"ebox-a"}],"operations":["agent_runs.get"],"admin":true}),
        ] {
            assert_eq!(
                call(
                    &app,
                    request(Method::POST, "/v1/service-tokens", &bad.to_string(), true)
                )
                .await
                .0,
                StatusCode::BAD_REQUEST,
                "{bad}"
            );
        }
        let (id, secret) = mint(&app, all.clone()).await;
        let (_, list) = call(&app, request(Method::GET, "/v1/service-tokens", "", true)).await;
        assert_eq!(list["tokens"][0]["id"], id);
        assert!(!list.to_string().contains(secret.split_once('.').unwrap().1));
        // A scoped token cannot mint, list or revoke tokens, nor reach admin,
        // login, lifecycle, storage, CI or controller-internal routes.
        for (method, path, body) in [
            (Method::POST, "/v1/service-tokens", all.to_string()),
            (Method::GET, "/v1/service-tokens", String::new()),
            (
                Method::POST,
                &format!("/v1/service-tokens/{id}/revoke"),
                "{}".into(),
            ),
            (Method::GET, "/v1/fleet", String::new()),
            (Method::GET, "/v1/agents", String::new()),
            (Method::POST, "/v1/agents/codex/login", "{}".into()),
            (
                Method::POST,
                "/v1/hosts/worker/instances",
                r#"{"name":"ebox-c","template":"t"}"#.into(),
            ),
            (
                Method::POST,
                "/v1/hosts/worker/instances/ebox-a/actions",
                r#"{"action":"stop"}"#.into(),
            ),
            (
                Method::GET,
                "/v1/hosts/worker/instances/ebox-a/accounts",
                String::new(),
            ),
            (Method::POST, "/v1/ci/runs", "{}".into()),
            (Method::GET, "/internal/status", String::new()),
            (Method::POST, "/internal/drain", "{}".into()),
            // Wrong workspace for every agent-run operation.
            (
                Method::POST,
                RUNS_B,
                r#"{"provider":"codex","prompt":"x"}"#.into(),
            ),
            (
                Method::GET,
                &format!("{RUNS_B}/{}", "1".repeat(32)),
                String::new(),
            ),
            (
                Method::POST,
                &format!("{RUNS_B}/{}/cancel", "1".repeat(32)),
                "{}".into(),
            ),
            (
                Method::POST,
                "/v1/hosts/other/instances/ebox-a/agent-runs",
                r#"{"provider":"codex","prompt":"x"}"#.into(),
            ),
        ] {
            assert_eq!(
                call(&app, bearer(method.clone(), path, &body, &secret))
                    .await
                    .0,
                StatusCode::FORBIDDEN,
                "{method} {path}"
            );
        }
        // The granted workspace passes authorization and reaches validation:
        // missing fields and denylisted or malformed env are rejected (400).
        for body in [
            json!({"provider":"codex"}),
            json!({"provider":"codex","prompt":"x","env":{"CLAUDE_CODE_OAUTH_TOKEN":"steal"}}),
            json!({"provider":"codex","prompt":"x","env":{"PATH":"/tmp"}}),
            json!({"provider":"codex","prompt":"x","env":{"LD_PRELOAD":"/tmp/x.so"}}),
            json!({"provider":"codex","prompt":"x","env":{"lower":"v"}}),
            json!({"provider":"codex","prompt":"x","env":{"BIG":"v".repeat(9000)}}),
        ] {
            let (status, value) = call(
                &app,
                bearer(Method::POST, RUNS_A, &body.to_string(), &secret),
            )
            .await;
            assert!(
                status == StatusCode::BAD_REQUEST || status == StatusCode::PAYLOAD_TOO_LARGE,
                "{body}: {status}"
            );
            assert!(!value.to_string().contains("steal"));
        }
        let (status, runs) = call(&app, bearer(Method::GET, "/v1/agent-runs", "", &secret)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(runs["total"], 0);
        // Operations are enforced separately from workspaces.
        let (_, read_only) = mint(
            &app,
            json!({"label":"reader","scopes":[{"host_id":"worker","name":"ebox-a"}],"operations":["agent_runs.get"]}),
        )
        .await;
        for (method, path, body) in [
            (
                Method::POST,
                RUNS_A.to_string(),
                r#"{"provider":"codex","prompt":"x"}"#,
            ),
            (Method::GET, "/v1/agent-runs".to_string(), ""),
            (
                Method::POST,
                format!("{RUNS_A}/{}/cancel", "1".repeat(32)),
                "{}",
            ),
        ] {
            assert_eq!(
                call(&app, bearer(method, &path, body, &read_only)).await.0,
                StatusCode::FORBIDDEN
            );
        }
        // Unknown, tampered, revoked and expired tokens are unauthenticated.
        let tampered = format!("{}0", &secret[..secret.len() - 1]);
        for token in [tampered.as_str(), "ebst_nope", "not-a-token"] {
            assert_eq!(
                call(&app, bearer(Method::GET, "/v1/agent-runs", "", token))
                    .await
                    .0,
                StatusCode::UNAUTHORIZED
            );
        }
        let (status, revoked) = call(
            &app,
            request(
                Method::POST,
                &format!("/v1/service-tokens/{id}/revoke"),
                "{}",
                true,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(revoked["token"]["active"], false);
        assert_eq!(
            call(&app, bearer(Method::GET, "/v1/agent-runs", "", &secret))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        let soon = (chrono::Utc::now() + chrono::Duration::milliseconds(1500)).to_rfc3339();
        let (_, expiring) = mint(
            &app,
            json!({"label":"short","scopes":[{"host_id":"worker","name":"ebox-a"}],"operations":["agent_runs.list"],"expires_at":soon}),
        )
        .await;
        assert_eq!(
            call(&app, bearer(Method::GET, "/v1/agent-runs", "", &expiring))
                .await
                .0,
            StatusCode::OK
        );
        tokio::time::sleep(Duration::from_millis(1600)).await;
        assert_eq!(
            call(&app, bearer(Method::GET, "/v1/agent-runs", "", &expiring))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        // The owner token is unchanged.
        assert_eq!(
            call(&app, request(Method::GET, "/v1/agent-runs", "", true))
                .await
                .0,
            StatusCode::OK
        );
        // Revocation survives a controller handoff.
        assert_eq!(c.drain()["drained"], true);
        c.activate().unwrap();
        assert_eq!(
            call(&app, bearer(Method::GET, "/v1/agent-runs", "", &secret))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn profiles_are_owner_managed_validated_and_granted_per_token() {
        use base64::Engine;
        let (_temp, c) = setup();
        let app = router(c.clone());
        c.activate().unwrap();
        let skill = b"---\nname: review\ndescription: Review a diff.\n---\nRead the diff.\n";
        let encoded = base64::engine::general_purpose::STANDARD.encode(skill);
        let (status, uploaded) = call(
            &app,
            request(
                Method::POST,
                "/v1/profile-files",
                &json!({"content_base64":encoded}).to_string(),
                true,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{uploaded}");
        let sha = uploaded["sha256"].as_str().unwrap().to_owned();
        assert_eq!(sha, enginebox_agents::profile::sha256_hex(skill));
        let key = "c".repeat(40);
        let verifier = json!({"name":"verifier","description":"Black-box checks","providers":{
            "claude":{"max_turns":200,"permission_profile":"read_only",
                "skills":[{"name":"review","files":[{"path":"SKILL.md","sha256":sha}]}],
                "env":{"CHECK_MODE":"strict-secret-value"}},
            "codex":{"permission_profile":"read_only"}},
            "callback_secret":key});
        let (status, saved) = call(
            &app,
            request(Method::POST, "/v1/profiles", &verifier.to_string(), true),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{saved}");
        assert_eq!(saved["profile"]["revision"], 1);
        assert_eq!(saved["profile"]["owner"], "owner");
        assert_eq!(saved["profile"]["has_callback_secret"], true);
        let (_, listed) = call(&app, request(Method::GET, "/v1/profiles", "", true)).await;
        let text = listed.to_string();
        assert!(!text.contains(&key) && !text.contains("strict-secret-value"));
        assert_eq!(
            listed["profiles"][0]["providers"]["claude"]["env_keys"],
            json!(["CHECK_MODE"])
        );
        let (_, again) = call(
            &app,
            request(Method::POST, "/v1/profiles", &verifier.to_string(), true),
        )
        .await;
        assert_eq!(again["profile"]["revision"], 2);
        let coder = json!({"name":"coder","providers":{"claude":{"max_turns":500}}});
        assert_eq!(
            call(
                &app,
                request(Method::POST, "/v1/profiles", &coder.to_string(), true)
            )
            .await
            .0,
            StatusCode::OK
        );
        for bad in [
            json!({"name":"x","providers":{"claude":{"max_turns":501}}}),
            json!({"name":"x","providers":{"claude":{"max_turns":0}}}),
            json!({"name":"x","providers":{"claude":{"permission_profile":"bypassPermissions"}}}),
            json!({"name":"x","providers":{"claude":{"allowed_tools":["--dangerously-skip-permissions"]}}}),
            json!({"name":"x","providers":{"claude":{"flags":["--yolo"]}}}),
            json!({"name":"x","providers":{"claude":{"skills":[{"name":"s","files":[{"path":"SKILL.md","sha256":"0".repeat(64)}]}]}}}),
            json!({"name":"x","providers":{"claude":{"skills":[{"name":"s","files":[{"path":"SKILL.md","sha256":"XYZ"}]}]}}}),
            json!({"name":"x","providers":{"claude":{}},"owner":"someone-else"}),
            json!({"name":"x","providers":{"claude":{}},"revision":9}),
            json!({"name":"x","providers":{"claude":{"env":{"LD_PRELOAD":"/tmp/x.so"}}}}),
            json!({"name":"Bad Name","providers":{"claude":{}}}),
        ] {
            let (status, body) = call(
                &app,
                request(Method::POST, "/v1/profiles", &bad.to_string(), true),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad} -> {body}");
        }
        // Grants must name existing profiles.
        let token = |profiles: Value| {
            json!({"label":"runner","scopes":[{"host_id":"worker","name":"ebox-a"}],
            "operations":["agent_runs.start","agent_runs.get"],"profiles":profiles,"callback_secret":"t".repeat(40)})
        };
        assert_eq!(
            call(
                &app,
                request(
                    Method::POST,
                    "/v1/service-tokens",
                    &token(json!(["missing"])).to_string(),
                    true
                )
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        let (_, secret) = mint(&app, token(json!(["verifier"]))).await;
        // Profiles and profile files are owner-only.
        for (method, path, body) in [
            (Method::GET, "/v1/profiles".to_string(), String::new()),
            (
                Method::GET,
                "/v1/profiles/verifier".to_string(),
                String::new(),
            ),
            (Method::POST, "/v1/profiles".to_string(), coder.to_string()),
            (
                Method::POST,
                "/v1/profiles/verifier/delete".to_string(),
                "{}".into(),
            ),
            (
                Method::POST,
                "/v1/profile-files".to_string(),
                json!({"content_base64":"eA=="}).to_string(),
            ),
        ] {
            assert_eq!(
                call(&app, bearer(method, &path, &body, &secret)).await.0,
                StatusCode::FORBIDDEN,
                "{path}"
            );
        }
        let launch = |profile: &str| {
            json!({"provider":"claude","prompt":"check","profile":profile}).to_string()
        };
        let (status, body) = call(
            &app,
            bearer(Method::POST, RUNS_A, &launch("coder"), &secret),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(
            body["error"],
            "This service token is not granted that profile."
        );
        // A granted profile passes the grant check; the offline fixture host
        // then refuses the launch itself.
        let (status, _) = call(
            &app,
            bearer(Method::POST, RUNS_A, &launch("verifier"), &secret),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = call(&app, request(Method::POST, RUNS_A, &launch("nope"), true)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // Callbacks: https only, no private targets, no key in the request.
        for callback in [
            json!({"url":"http://hooks.example.com/x"}),
            json!({"url":"https://169.254.169.254/latest"}),
            json!({"url":"https://hooks.example.com/x","secret":"mine-mine-mine-mine-mine-mine-mine"}),
        ] {
            let body = json!({"provider":"claude","prompt":"check","profile":"verifier","callback":callback});
            let (status, _) = call(
                &app,
                bearer(Method::POST, RUNS_A, &body.to_string(), &secret),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{callback}");
        }
        assert_eq!(
            call(
                &app,
                request(Method::POST, "/v1/profiles/coder/delete", "{}", true)
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            call(&app, request(Method::GET, "/v1/profiles/coder", "", true))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        // Profiles survive a controller reload.
        drop(app);
        let reopened = profiles::Profiles::open(&c.root).unwrap();
        assert!(reopened.exists("verifier") && !reopened.exists("coder"));
        let resolved = reopened.resolve("verifier", "claude").unwrap();
        assert_eq!(resolved.revision, 2);
        assert_eq!(resolved.files.len(), 1);
        assert!(reopened.resolve("verifier", "cursor").is_err());
        // A stored file that no longer matches its digest is refused.
        let path = c.root.join("profile-files").join(&sha);
        enginebox_agents::security::write_bytes(
            &c.root.join("profile-files"),
            &sha,
            b"tampered",
            1024,
        )
        .unwrap();
        assert!(path.exists());
        assert!(reopened.resolve("verifier", "claude").is_err());
    }
    #[tokio::test]
    async fn service_tokens_see_and_cancel_only_runs_on_their_workspaces() {
        let (_temp, c) = setup();
        let app = router(c.clone());
        c.activate().unwrap();
        let (token_id, secret) = mint(
            &app,
            json!({"label":"tenant-a","scopes":[{"host_id":"worker","name":"ebox-a"}],
                "operations":["agent_runs.get","agent_runs.list","agent_runs.cancel"]}),
        )
        .await;
        assert_eq!(c.drain()["drained"], true);
        // Durable runs as the agent service writes them; reloaded on activation.
        let principal = format!("service-token:{token_id}");
        let (run_a, run_b) = ("a".repeat(32), "b".repeat(32));
        for (id, name, started_by, keys) in [
            (
                &run_a,
                "ebox-a",
                principal.as_str(),
                json!(["API_BASE_URL"]),
            ),
            (&run_b, "ebox-b", "owner", json!([])),
        ] {
            enginebox_core::state::write_json(
                &c.root.join("agent-runs").join(format!("{id}.json")),
                &json!({"id":id,"host_id":"worker","name":name,"provider":"codex","scope":"controller-owner","state_scope":"private-workspace","status":"succeeded","workdir":".","resume_id":null,"created_at":chrono::Utc::now().to_rfc3339(),"finished_at":chrono::Utc::now().to_rfc3339(),"result":{"exit_code":0,"output":"","timed_out":false,"output_truncated":false}}),
            )
            .unwrap();
            enginebox_core::state::write_json(
                &c.root.join("agent-run-details").join(format!("{id}.json")),
                &json!({"schema":1,"task":"fixture","started_at":null,"cancel_requested":false,"cancelled":false,"started_by":started_by,"env_keys":keys}),
            )
            .unwrap();
        }
        c.activate().unwrap();
        let (status, runs) = call(&app, bearer(Method::GET, "/v1/agent-runs", "", &secret)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(runs["total"], 1);
        assert_eq!(runs["runs"][0]["id"], run_a.as_str());
        assert_eq!(runs["runs"][0]["started_by"], principal.as_str());
        assert_eq!(runs["runs"][0]["env_keys"], json!(["API_BASE_URL"]));
        let (_, owner_runs) = call(&app, request(Method::GET, "/v1/agent-runs", "", true)).await;
        assert_eq!(owner_runs["total"], 2);
        let (status, run) = call(
            &app,
            bearer(Method::GET, &format!("{RUNS_A}/{run_a}"), "", &secret),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(run["run"]["started_by"], principal.as_str());
        // Another workspace's run is invisible: refused by scope, or unknown
        // when addressed through the token's own workspace.
        assert_eq!(
            call(
                &app,
                bearer(Method::GET, &format!("{RUNS_B}/{run_b}"), "", &secret)
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(
                &app,
                bearer(Method::GET, &format!("{RUNS_A}/{run_b}"), "", &secret)
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(
                &app,
                bearer(
                    Method::POST,
                    &format!("{RUNS_B}/{run_b}/cancel"),
                    "{}",
                    &secret
                )
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(
                &app,
                bearer(
                    Method::POST,
                    &format!("{RUNS_A}/{run_b}/cancel"),
                    "{}",
                    &secret
                )
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        // A finished run's cancel returns its state without contacting the host.
        let (status, cancelled) = call(
            &app,
            bearer(
                Method::POST,
                &format!("{RUNS_A}/{run_a}/cancel"),
                "{}",
                &secret,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(cancelled["run"]["id"], run_a.as_str());
    }

    #[tokio::test]
    async fn storage_routes_use_registered_targets_and_durable_admitted_jobs() {
        let (_temp, c) = setup();
        enginebox_core::state::write_json(&c.root.join("storage/config.json"), &json!({
            "schema_version":1,"nodes":[{"host_id":"worker","executable":"/usr/local/bin/ebox","config":"/etc/enginebox/layerfs.json","projects":[{"id":"game","repository":"/srv/source/game"}]}]
        })).unwrap();
        let app = router(c.clone());
        assert_eq!(
            app.clone()
                .oneshot(request(Method::GET, "/v1/storage", "", false))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            app.clone()
                .oneshot(request(
                    Method::POST,
                    "/v1/storage/hosts/worker/fork",
                    r#"{"project":"game","name":"agent-a"}"#,
                    true
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        c.activate().unwrap();
        let (started_send, started_receive) = std::sync::mpsc::channel();
        let (finish_send, finish_receive) = std::sync::mpsc::channel();
        let finish_receive = Arc::new(Mutex::new(finish_receive));
        {
            let mut gate = c.gate.lock().unwrap();
            let runtime = gate.runtime.as_mut().unwrap();
            runtime.storage =
                runtime
                    .storage
                    .clone()
                    .with_runner(Arc::new(move |host, argv, _, _| {
                        assert_eq!(host.id, "worker");
                        assert!(argv.windows(3).any(|v| v == ["fork", "game", "agent-a"]));
                        started_send.send(()).unwrap();
                        finish_receive
                            .lock()
                            .unwrap()
                            .recv_timeout(Duration::from_secs(5))
                            .unwrap();
                        Ok(json!({"ok":true,"workspace":"agent-a","path":"/project"}))
                    }));
        }
        for (path, body) in [
            (
                "/v1/storage/hosts/worker/exec",
                r#"{"name":"agent-a","argv":["id"]}"#,
            ),
            (
                "/v1/storage/hosts/worker/fork",
                r#"{"project":"game","name":"agent-a","config":"/tmp/foreign"}"#,
            ),
            ("/v1/storage/hosts/foreign/list", "{}"),
        ] {
            assert_eq!(
                app.clone()
                    .oneshot(request(Method::POST, path, body, true))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::BAD_REQUEST
            );
        }
        let response = app
            .clone()
            .oneshot(request(
                Method::POST,
                "/v1/storage/hosts/worker/fork",
                r#"{"project":"game","name":"agent-a"}"#,
                true,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let value: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 16384).await.unwrap()).unwrap();
        tokio::task::spawn_blocking(move || {
            started_receive
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
        })
        .await
        .unwrap();
        assert_eq!(c.drain()["drained"], false);
        finish_send.send(()).unwrap();
        while c.runtime().unwrap().jobs.pending() > 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let job = c
            .runtime()
            .unwrap()
            .jobs
            .get(value["job"]["id"].as_str().unwrap())
            .unwrap();
        assert_eq!(job["status"], "succeeded");
        assert_eq!(job["result"]["result"]["workspace"], "agent-a");
        assert_eq!(c.drain()["drained"], true);
    }
    #[tokio::test]
    async fn standby_never_admits_writes_and_payloads_are_bounded() {
        let (_temp, c) = setup();
        let app = router(c);
        assert_eq!(
            app.clone()
                .oneshot(request(
                    Method::POST,
                    "/v1/hosts/worker/instances",
                    "{}",
                    true
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            app.clone()
                .oneshot(request(
                    Method::POST,
                    "/v1/hosts/worker/instances",
                    &"a".repeat(20000),
                    true
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            app.clone()
                .oneshot(request(Method::POST, "/v1/templates", "[]", true))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            app.oneshot(request(
                Method::POST,
                "/v1/templates",
                r#"{"manifest":{"revision":1,"revision":2}}"#,
                true
            ))
            .await
            .unwrap()
            .status(),
            StatusCode::BAD_REQUEST
        );
    }
    #[tokio::test]
    async fn handoff_observations_cannot_present_cached_allowance_as_current() {
        let (_temp, c) = setup();
        c.activate().unwrap();
        c.observations.lock().unwrap().insert(
            "/v1/agents/codex/usage".into(),
            json!({"usage":{"provider":"codex","freshness":"current","observed_at":"2026-10-19T00:00:00Z","allowance_windows":[{"used_percentage":25,"remaining_percentage":75,"freshness":"current","resets_at":"2000-01-01T00:00:00Z"}]}}),
        );
        assert_eq!(c.drain()["drained"], true);
        let response = router(c.clone())
            .oneshot(request(Method::GET, "/v1/agents/codex/usage", "", true))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 16384).await.unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["usage"]["freshness"], "stale");
        assert!(value["usage"]["allowance_windows"][0]["remaining_percentage"].is_null());
        assert_eq!(c.status()["mode"], "drained");
    }
    #[tokio::test]
    async fn status_reports_background_reconcile_failure_until_next_success() {
        let (_temp, c) = setup();
        assert!(c.status()["background_reconcile_error"].is_null());
        c.record_reconcile(vec!["fleet: boom".into(), "agents: bang".into()]);
        let s = c.status();
        assert_eq!(
            s["background_reconcile_error"]["message"],
            "fleet: boom; agents: bang"
        );
        assert!(s["background_reconcile_error"]["at"].as_str().is_some());
        c.record_reconcile(Vec::new());
        assert!(c.status()["background_reconcile_error"].is_null());
    }
    #[tokio::test]
    async fn only_one_controller_can_activate_and_rollback_reloads_state() {
        let (temp, old) = setup();
        old.activate().unwrap();
        let next = Controller::new(
            &temp.path().join("fleet.toml"),
            &temp.path().join("state"),
            "x".repeat(32),
        )
        .unwrap();
        assert!(next.activate().is_err());
        assert_eq!(old.drain()["drained"], true);
        next.activate().unwrap();
        assert!(old.activate().is_err());
        let job = next
            .runtime()
            .unwrap()
            .jobs
            .submit("worker", "test", "ebox-test", || Ok(json!({"done":true})))
            .unwrap();
        while next.runtime().unwrap().jobs.pending() > 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(next.drain()["drained"], true);
        old.activate().unwrap();
        assert_eq!(
            old.runtime()
                .unwrap()
                .jobs
                .get(job["id"].as_str().unwrap())
                .unwrap()["status"],
            "succeeded"
        );
    }
    #[tokio::test]
    async fn drain_waits_for_admitted_jobs_and_timeout_can_resume_old_owner() {
        for ci in [false, true] {
            let (_temp, c) = setup();
            c.activate().unwrap();
            let (send, receive) = std::sync::mpsc::channel();
            let runtime = c.runtime().unwrap();
            let jobs = if ci { &runtime.ci_jobs } else { &runtime.jobs };
            jobs.submit(
                "worker",
                if ci { "ci_automation" } else { "test" },
                "ebox-test",
                move || {
                    receive.recv().unwrap();
                    Ok(json!({"done":true}))
                },
            )
            .unwrap();
            assert_eq!(c.drain()["drained"], false);
            assert!(c.admit().is_err());
            c.activate().unwrap();
            assert_eq!(c.status()["accepting_mutations"], true);
            send.send(()).unwrap();
            while c.runtime().unwrap().pending_jobs() > 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let request = c.admit().unwrap();
            assert_eq!(c.drain()["drained"], false);
            drop(request);
            assert_eq!(c.drain()["drained"], true);
        }
    }

    #[tokio::test]
    async fn cancelled_http_waiter_does_not_release_an_unfinished_operation() {
        let (_temp, c) = setup();
        c.activate().unwrap();
        let admission = c.admit().unwrap();
        let (started, begun) = tokio::sync::oneshot::channel();
        let (release, wait) = tokio::sync::oneshot::channel();
        let waiter = tokio::spawn(admitted(admission, async move {
            started.send(()).unwrap();
            wait.await.unwrap();
            Ok(())
        }));
        begun.await.unwrap();
        waiter.abort();
        let _ = waiter.await;
        assert_eq!(c.drain()["drained"], false);
        assert_eq!(c.status()["active_requests"], 1);
        release.send(()).unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while c.drain()["drained"] != true {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::task::yield_now().await;
        }
    }

    /// An active controller with a connected agent and no measurable host,
    /// so accepted runs stay queued.
    fn queue_fixture(c: &Arc<Controller>) {
        c.activate().unwrap();
        let home = c.root.join("agents/homes/claude");
        enginebox_agents::security::private_dir(&home).unwrap();
        enginebox_agents::security::write_json(
            &home,
            ".config/enginebox/controller-token.json",
            &json!({"kind":"setup_token","value":"controller-token-fixture","expires_at":null}),
        )
        .unwrap();
        c.runtime()
            .unwrap()
            .agents
            .set_measure_hook(Arc::new(|_| anyhow::bail!("fixture host is offline")));
    }

    #[tokio::test]
    async fn pools_belong_to_projects_and_tokens_can_be_restricted_to_one_project() {
        let (_temp, c) = setup();
        let app = router(c.clone());
        queue_fixture(&c);
        for pool in [
            json!({"name":"alpha-w","project":"alpha","workspaces":[{"host_id":"worker","name":"ebox-p1"}]}),
            json!({"name":"beta-w","project":"beta","workspaces":[{"host_id":"worker","name":"ebox-p2"}]}),
            json!({"name":"legacy","workspaces":[{"host_id":"worker","name":"ebox-p3"}]}),
        ] {
            let (status, body) = call(
                &app,
                request(Method::POST, "/v1/pools", &pool.to_string(), true),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        let (_, all) = call(&app, request(Method::GET, "/v1/pools", "", true)).await;
        assert_eq!(
            all["projects"],
            json!([{"project":"alpha","pools":["alpha-w"]},{"project":"beta","pools":["beta-w"]},
                {"project":"default","pools":["legacy"]}])
        );
        let (status, alpha) = call(
            &app,
            request(Method::GET, "/v1/projects/alpha/pools", "", true),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(alpha["pools"].as_array().unwrap().len(), 1);
        assert_eq!(alpha["pools"][0]["name"], "alpha-w");
        let ops = json!(["agent_runs.start", "agent_runs.get", "agent_runs.list"]);
        // A project restriction refuses pools of other projects at mint.
        let (status, _) = call(
            &app,
            request(
                Method::POST,
                "/v1/service-tokens",
                &json!({"label":"x","scopes":[],"pools":["alpha-w","beta-w"],"project":"alpha","operations":ops})
                    .to_string(),
                true,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (_, secret) = mint(
            &app,
            json!({"label":"alpha-only","scopes":[],"pools":["alpha-w"],"project":"alpha","operations":ops}),
        )
        .await;
        let (_, list) = call(&app, request(Method::GET, "/v1/service-tokens", "", true)).await;
        let token = &list["tokens"][0];
        assert_eq!(token["project"], "alpha");
        assert_eq!(
            token["pool_grants"],
            json!([{"project":"alpha","pools":["alpha-w"]}])
        );
        let start = r#"{"provider":"claude","prompt":"work"}"#;
        let (status, _) = call(
            &app,
            bearer(Method::POST, "/v1/pools/alpha-w/agent-runs", start, &secret),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        // The owner moves the pool to another project: the restricted token
        // loses it on the next request.
        let (status, _) = call(
            &app,
            request(
                Method::POST,
                "/v1/pools",
                &json!({"name":"alpha-w","project":"beta","workspaces":[{"host_id":"worker","name":"ebox-p1"}]})
                    .to_string(),
                true,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = call(
            &app,
            bearer(Method::POST, "/v1/pools/alpha-w/agent-runs", start, &secret),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (_, runs) = call(&app, bearer(Method::GET, "/v1/agent-runs", "", &secret)).await;
        assert_eq!(
            runs["total"], 0,
            "its runs in the moved pool are hidden too"
        );
        // Launch events and project listings are owner-only.
        for path in [
            "/v1/launch-events",
            "/v1/pools/alpha-w/launch-events",
            "/v1/projects/alpha/launch-events",
            "/v1/projects/alpha/pools",
        ] {
            let (status, _) = call(&app, bearer(Method::GET, path, "", &secret)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
            let (status, body) = call(&app, request(Method::GET, path, "", true)).await;
            assert_eq!(status, StatusCode::OK, "{path}");
            if path.ends_with("launch-events") {
                assert_eq!(body["max_retained"], 500);
                assert!(body["events"].is_array());
            }
        }
    }

    #[tokio::test]
    async fn pools_are_owner_managed_and_pool_runs_need_a_pool_grant() {
        let (_temp, c) = setup();
        let app = router(c.clone());
        queue_fixture(&c);
        let workers =
            json!({"name":"workers","workspaces":[{"host_id":"worker","name":"ebox-p1"}]});
        for bad in [
            json!({"name":"workers","workspaces":[{"host_id":"unknown","name":"ebox-p1"}]}),
            json!({"name":"Bad","workspaces":[{"host_id":"worker","name":"ebox-p1"}]}),
            json!({"name":"workers"}),
            json!({"name":"workers","autoscale":{"hosts":["worker"],"max_workspaces":2}}),
            json!({"name":"workers","profile":"missing","workspaces":[{"host_id":"worker","name":"ebox-p1"}]}),
            json!({"name":"workers","workspaces":[{"host_id":"worker","name":"ebox-p1"}],"created":[]}),
        ] {
            let (status, _) = call(
                &app,
                request(Method::POST, "/v1/pools", &bad.to_string(), true),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        }
        let (status, created) = call(
            &app,
            request(Method::POST, "/v1/pools", &workers.to_string(), true),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{created}");
        assert_eq!(created["pool"]["revision"], 1);
        // Grants must name existing pools; a pool-only token needs no workspace scope.
        let grant = |pools: Value| {
            json!({"label":"pool-client","scopes":[],"pools":pools,
                "operations":["agent_runs.start","agent_runs.get","agent_runs.list","agent_runs.cancel"]})
        };
        let (status, _) = call(
            &app,
            request(
                Method::POST,
                "/v1/service-tokens",
                &grant(json!(["missing"])).to_string(),
                true,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (_, secret) = mint(&app, grant(json!(["workers"]))).await;
        let (_, list) = call(&app, request(Method::GET, "/v1/service-tokens", "", true)).await;
        assert_eq!(list["tokens"][0]["pools"], json!(["workers"]));
        let (_, other_secret) = mint(
            &app,
            json!({"label":"workspace-client","scopes":[{"host_id":"worker","name":"ebox-a"}],
                "operations":["agent_runs.list"]}),
        )
        .await;
        // Pool management is owner-only; ungranted pools and workspaces are 403.
        let start = r#"{"provider":"claude","prompt":"work","idempotency_key":"job-1"}"#;
        for (method, path, body) in [
            (Method::POST, "/v1/pools".to_string(), workers.to_string()),
            (Method::GET, "/v1/pools".to_string(), String::new()),
            (Method::GET, "/v1/pools/workers".to_string(), String::new()),
            (
                Method::POST,
                "/v1/pools/workers/delete".to_string(),
                "{}".into(),
            ),
            (
                Method::POST,
                "/v1/pools/other/agent-runs".to_string(),
                start.into(),
            ),
            (
                Method::GET,
                format!("/v1/pools/other/agent-runs/{}", "1".repeat(32)),
                String::new(),
            ),
            (Method::POST, RUNS_A.to_string(), start.into()),
        ] {
            let (status, _) = call(&app, bearer(method, &path, &body, &secret)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
        }
        let (status, _) = call(
            &app,
            bearer(
                Method::POST,
                "/v1/pools/workers/agent-runs",
                start,
                &other_secret,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        // A pool start is queued with its landing workspace still unknown.
        let (status, started) = call(
            &app,
            bearer(Method::POST, "/v1/pools/workers/agent-runs", start, &secret),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{started}");
        let run = &started["run"];
        assert_eq!(run["status"], "queued");
        assert!(run["host_id"].is_null() && run["name"].is_null());
        assert_eq!(run["pool"], "workers");
        let id = run["id"].as_str().unwrap().to_owned();
        let (status, replayed) = call(
            &app,
            bearer(Method::POST, "/v1/pools/workers/agent-runs", start, &secret),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(replayed["replayed"], true);
        assert_eq!(replayed["run"]["id"], id.as_str());
        let path = format!("/v1/pools/workers/agent-runs/{id}");
        let mut polled = Value::Null;
        for _ in 0..200 {
            polled = call(&app, bearer(Method::GET, &path, "", &secret)).await.1;
            if polled["run"]["waiting_reason"]
                .as_str()
                .is_some_and(|r| r.contains("host worker"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            polled["run"]["waiting_reason"]
                .as_str()
                .unwrap()
                .contains("waiting for capacity on host worker"),
            "{polled}"
        );
        assert_eq!(polled["run"]["queue_position"], 1);
        let (_, runs) = call(&app, bearer(Method::GET, "/v1/agent-runs", "", &secret)).await;
        assert_eq!(runs["total"], 1);
        let (_, runs) = call(
            &app,
            bearer(Method::GET, "/v1/agent-runs", "", &other_secret),
        )
        .await;
        assert_eq!(runs["total"], 0);
        let (_, pool) = call(&app, request(Method::GET, "/v1/pools/workers", "", true)).await;
        assert_eq!(pool["pool"]["queued_runs"], 1);
        // The pool cannot be deleted while it has queued runs.
        let (status, _) = call(
            &app,
            request(Method::POST, "/v1/pools/workers/delete", "{}", true),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, cancelled) = call(
            &app,
            bearer(Method::POST, &format!("{path}/cancel"), "{}", &secret),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(cancelled["run"]["status"], "cancelled");
        let (status, _) = call(
            &app,
            request(Method::POST, "/v1/pools/workers/delete", "{}", true),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_full_queue_answers_429_with_retry_after() {
        let (_temp, c) = setup();
        let app = router(c.clone());
        queue_fixture(&c);
        c.runtime()
            .unwrap()
            .agents
            .set_limits(enginebox_agents::queue::QueueLimits {
                max_queued_per_principal: 1,
                retry_after_seconds: 9,
                ..Default::default()
            });
        let start = r#"{"provider":"claude","prompt":"work"}"#;
        let (status, first) = call(&app, request(Method::POST, RUNS_A, start, true)).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(first["run"]["status"], "queued");
        // A busy workspace queues rather than refusing.
        let response = app
            .clone()
            .oneshot(request(Method::POST, RUNS_A, start, true))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[header::RETRY_AFTER], "9");
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(body["retry_after_seconds"], 9);
        c.runtime()
            .unwrap()
            .agents
            .set_limits(enginebox_agents::queue::QueueLimits::default());
        let (status, second) = call(&app, request(Method::POST, RUNS_A, start, true)).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(second["run"]["status"], "queued");
        // Draining pauses admission without dropping the queue.
        c.drain();
        let (_, runs) = call(&app, request(Method::GET, "/v1/agent-runs", "", true)).await;
        assert_eq!(runs["total"], 2);
    }

    fn files_contain(root: &Path, needle: &str) -> Vec<PathBuf> {
        let mut found = vec![];
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if std::fs::read(&path)
                    .map(|b| String::from_utf8_lossy(&b).contains(needle))
                    .unwrap_or(false)
                {
                    found.push(path);
                }
            }
        }
        found
    }

    #[tokio::test]
    async fn repositories_are_owner_configured_project_scoped_and_runs_get_tokens_at_admission() {
        const ACCOUNT: &str = "0123456789abcdef0123456789abcdef";
        const API_TOKEN: &str = "cf-test-api-token-0123456789abcdef";
        let (temp, c) = setup();
        let api = repositories::mock::MockArtifacts::new(ACCOUNT);
        c.set_repository_transport("https://api.example.test/client/v4", api.clone());
        let app = router(c.clone());
        queue_fixture(&c);
        let backend = "/v1/repository-backends/artifacts";
        let config =
            json!({"account_id":ACCOUNT,"namespace_prefix":"eb","api_token":API_TOKEN}).to_string();
        // Owner-only, write-only backend configuration.
        let (status, put) = call(&app, request(Method::PUT, backend, &config, true)).await;
        assert_eq!(status, StatusCode::OK, "{put}");
        assert_eq!(put["backend"]["api_token"], "[redacted]");
        let (status, got) = call(&app, request(Method::GET, backend, "", true)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(got["backend"]["account_id"], ACCOUNT);
        assert!(!put.to_string().contains(API_TOKEN) && !got.to_string().contains(API_TOKEN));
        let (status, _) = call(&app, request(Method::PATCH, backend, &config, true)).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        for pool in [
            json!({"name":"alpha-w","project":"alpha","workspaces":[{"host_id":"worker","name":"ebox-p1"}]}),
            json!({"name":"beta-w","project":"beta","workspaces":[{"host_id":"worker","name":"ebox-p2"}]}),
        ] {
            let (status, body) = call(
                &app,
                request(Method::POST, "/v1/pools", &pool.to_string(), true),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        let repos = "/v1/projects/alpha/repositories";
        let (status, created) = call(
            &app,
            request(Method::POST, repos, r#"{"name":"game"}"#, true),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        let remote = format!("https://{ACCOUNT}.artifacts.cloudflare.net/git/eb-alpha/game.git");
        assert_eq!(created["repository"]["remote"], remote);
        assert!(created["repository"].get("token").is_none());
        let (status, _) = call(
            &app,
            request(Method::POST, repos, r#"{"name":"game"}"#, true),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "create is idempotent on name");
        let (_, all) = call(&app, request(Method::GET, "/v1/repositories", "", true)).await;
        assert_eq!(all["repositories"][0]["project"], "alpha");

        let ops = json!(["agent_runs.start", "agent_runs.get"]);
        let (_, none) = mint(&app, json!({"label":"none","scopes":[{"host_id":"worker","name":"ebox-a"}],"operations":ops})).await;
        let (_, read) = mint(&app, json!({"label":"read","scopes":[],"operations":[],"repositories":[{"project":"alpha","access":"read"}]})).await;
        let (_, write) = mint(&app, json!({"label":"write","scopes":[],"pools":["alpha-w","beta-w"],"operations":ops,
            "repositories":[{"project":"alpha","access":"write"}],"callback_secret":"k".repeat(40)})).await;
        let (_, pool_read) = mint(
            &app,
            json!({"label":"pool-read","scopes":[],"pools":["alpha-w"],"operations":ops,
            "repositories":[{"project":"alpha","access":"read"}]}),
        )
        .await;
        // No grant: 403 on every repository route and on the backend.
        for (method, path, body) in [
            (Method::GET, repos, ""),
            (Method::POST, repos, r#"{"name":"x"}"#),
            (Method::GET, "/v1/projects/alpha/repositories/game", ""),
            (
                Method::POST,
                "/v1/projects/alpha/repositories/game/tokens",
                r#"{"access":"read"}"#,
            ),
            (Method::GET, backend, ""),
            (Method::PUT, backend, config.as_str()),
            (Method::DELETE, backend, ""),
            (Method::GET, "/v1/repositories", ""),
        ] {
            let (status, _) = call(&app, bearer(method.clone(), path, body, &none)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
            let (status, _) = call(&app, bearer(method.clone(), path, body, &write)).await;
            if path.starts_with("/v1/projects/") {
                assert_ne!(status, StatusCode::FORBIDDEN, "{method} {path}");
            } else {
                assert_eq!(status, StatusCode::FORBIDDEN, "owner-only {method} {path}");
            }
        }
        // Read grant: list, read, read tokens; never create or write tokens.
        let (status, listed) = call(&app, bearer(Method::GET, repos, "", &read)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            listed["repositories"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["name"] == "game")
        );
        let (status, _) = call(
            &app,
            bearer(
                Method::GET,
                "/v1/projects/alpha/repositories/game",
                "",
                &read,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = call(
            &app,
            bearer(Method::POST, repos, r#"{"name":"mine"}"#, &read),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let tokens = "/v1/projects/alpha/repositories/game/tokens";
        let (status, minted) = call(
            &app,
            bearer(
                Method::POST,
                tokens,
                r#"{"access":"read","ttl_seconds":300}"#,
                &read,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{minted}");
        assert_eq!(minted["remote"], remote);
        assert_eq!(minted["access"], "read");
        let (status, _) = call(
            &app,
            bearer(Method::POST, tokens, r#"{"access":"write"}"#, &read),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        // Wrong project.
        for (method, path) in [
            (Method::GET, "/v1/projects/beta/repositories"),
            (Method::POST, "/v1/projects/beta/repositories"),
        ] {
            let (status, _) = call(&app, bearer(method, path, r#"{"name":"x"}"#, &write)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
        }
        // Write grant: create, write tokens; TTLs are bounded.
        let (status, _) = call(
            &app,
            bearer(Method::POST, repos, r#"{"name":"tools"}"#, &write),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, _) = call(
            &app,
            bearer(
                Method::POST,
                tokens,
                r#"{"access":"write","ttl_seconds":3600}"#,
                &write,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, _) = call(
            &app,
            bearer(
                Method::POST,
                tokens,
                r#"{"access":"write","ttl_seconds":86401}"#,
                &write,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = call(
            &app,
            bearer(
                Method::GET,
                "/v1/projects/alpha/repositories/missing",
                "",
                &write,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // Runs: a write grant on the repository's project, and for a pool
        // run the pool's own project.
        let run = |repository: &str, prompt: &str| {
            json!({"provider":"claude","prompt":prompt,"repository":repository}).to_string()
        };
        let alpha_runs = "/v1/pools/alpha-w/agent-runs";
        let (status, _) = call(
            &app,
            bearer(
                Method::POST,
                alpha_runs,
                &run("alpha/game", "x"),
                &pool_read,
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "a read grant cannot name a repository on a run"
        );
        let (status, body) = call(
            &app,
            bearer(
                Method::POST,
                "/v1/pools/beta-w/agent-runs",
                &run("alpha/game", "x"),
                &write,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        let (status, _) = call(
            &app,
            bearer(Method::POST, alpha_runs, &run("alpha/missing", "x"), &write),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = call(
            &app,
            bearer(Method::POST, alpha_runs, &run("game", "x"), &write),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = call(
            &app,
            request(
                Method::POST,
                "/v1/pools/beta-w/agent-runs",
                &run("alpha/game", "x"),
                true,
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "the owner too: pool and repository projects must match"
        );

        // Admission: the executor sees the minted access; nothing else does.
        let agents = c.runtime().unwrap().agents;
        let seen: Arc<Mutex<Vec<enginebox_agents::repository::Access>>> = Arc::default();
        let finished: Arc<Mutex<Vec<Value>>> = Arc::default();
        {
            let seen = seen.clone();
            agents.set_run_executor(Arc::new(move |run| {
                let access = run.repository_access.clone().expect("minted at admission");
                seen.lock().unwrap().push(access.clone());
                if run.request.prompt == "fail" {
                    return Err(enginebox_agents::launch_events::LaunchFailure::new(
                        enginebox_agents::launch_events::Stage::Start,
                        "guest_launch_failed",
                        format!("git push failed for Bearer {}", access.token),
                    )
                    .into());
                }
                Ok(json!({"exit_code":0,"output":"{\"type\":\"result\"}","timed_out":false,"output_truncated":false}))
            }));
            let finished = finished.clone();
            agents.set_finish_hook(Arc::new(move |f| finished.lock().unwrap().push(f.payload)));
        }
        agents.set_measure_hook(Arc::new(|_| {
            Ok(enginebox_agents::queue::HostLoad {
                cpu_total: 64,
                reserve_cpu: 2,
                memory_available_bytes: 1 << 40,
                reserve_memory_bytes: 1 << 30,
            })
        }));
        let mut ids = vec![];
        for prompt in ["work", "fail"] {
            let body = json!({"provider":"claude","prompt":prompt,"repository":"alpha/game",
                "callback":{"url":"https://hooks.example.com/runs"}})
            .to_string();
            let (status, accepted) =
                call(&app, bearer(Method::POST, alpha_runs, &body, &write)).await;
            assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
            assert_eq!(accepted["run"]["repository"], "alpha/game");
            let id = accepted["run"]["id"].as_str().unwrap().to_owned();
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            loop {
                let (_, current) = call(
                    &app,
                    bearer(Method::GET, &format!("{alpha_runs}/{id}"), "", &write),
                )
                .await;
                if !["queued", "running"].contains(&current["run"]["status"].as_str().unwrap_or(""))
                {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "run did not finish: {current}"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            ids.push(id);
        }
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2);
        {
            let state = api.state.lock().unwrap();
            for access in &seen {
                assert_eq!(access.remote, remote);
                assert_eq!(access.default_branch, "main");
                let (id, token) = state
                    .tokens
                    .iter()
                    .find(|(_, t)| t.3 == access.token)
                    .unwrap();
                assert_eq!(token.2, "write", "runs get write tokens");
                assert!(token.4, "revoked at run end: {id}");
                let (_, _, _, body) = state
                    .calls
                    .iter()
                    .find(|(m, u, _, b)| m == "POST" && u.ends_with("/tokens") && b.is_some())
                    .unwrap();
                assert_eq!(body.as_ref().unwrap()["repo"], "game");
            }
            // The run TTL is the run timeout (default 120 s) plus the margin.
            assert!(state.calls.iter().any(|(_, _, _, b)| {
                b.as_ref()
                    .is_some_and(|b| b["ttl"] == 720 && b["scope"] == "write")
            }));
        }
        let (_, run) = call(
            &app,
            bearer(Method::GET, &format!("{alpha_runs}/{}", ids[0]), "", &write),
        )
        .await;
        assert_eq!(run["run"]["repository"], "alpha/game");
        assert_eq!(run["run"]["status"], "succeeded");
        let (_, failed) = call(
            &app,
            bearer(Method::GET, &format!("{alpha_runs}/{}", ids[1]), "", &write),
        )
        .await;
        assert_eq!(failed["run"]["status"], "failed");
        let (_, events) = call(&app, request(Method::GET, "/v1/launch-events", "", true)).await;
        let (_, listing) = call(&app, request(Method::GET, "/v1/agent-runs", "", true)).await;
        let payloads = finished.lock().unwrap().clone();
        assert_eq!(payloads.len(), 2);
        assert_eq!(payloads[0]["repository"], "alpha/game");
        for access in &seen {
            let secret = access.token.split_once("?expires=").unwrap().0;
            for (what, value) in [
                ("run", &run),
                ("failed run", &failed),
                ("launch events", &events),
                ("listing", &listing),
            ] {
                assert!(
                    !value.to_string().contains(secret),
                    "token leaked into {what}: {value}"
                );
            }
            assert!(
                !json!(payloads).to_string().contains(secret),
                "token leaked into a callback"
            );
            let leaks = files_contain(temp.path(), secret);
            assert!(leaks.is_empty(), "token persisted in {leaks:?}");
        }
        assert!(events.to_string().contains("guest_launch_failed"));
    }
}
