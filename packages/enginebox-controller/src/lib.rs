//! Authenticated fleet API, durable jobs and exclusive controller ownership.
pub mod ci;
mod ci_templates;
pub mod contributors;
mod environment;
pub mod jobs;
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
        }))
    }
    pub fn activate(self: &Arc<Self>) -> Result<Value> {
        let mut gate = self.gate.lock().unwrap();
        if gate.mode == Mode::Active {
            return Ok(json!({"active":true,"state_schema":STATE_SCHEMA}));
        }
        if gate.mode == Mode::Draining && gate.owner.is_some() {
            gate.mode = Mode::Active;
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
        gate.runtime = Some(runtime);
        gate.owner = Some(owner);
        gate.mode = Mode::Active;
        Ok(json!({"active":true,"state_schema":STATE_SCHEMA}))
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
    if !controller.authorized(&request) {
        return error(StatusCode::UNAUTHORIZED, "Authentication required.");
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
        let result = blocking(move || {
            let _admission = admission;
            read(&operation, &parts)
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
    if method != Method::POST {
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
    } else if path == "/v1/ci/templates" {
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
    let result = admitted(admission, write(controller, parts, body)).await;
    match result {
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

fn read(c: &Controller, p: &[String]) -> Result<Value> {
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
        ["v1", "agent-runs"] => Ok(c.runtime()?.agents.list_runs()),
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

async fn write(c: Arc<Controller>, parts: Vec<String>, body: Value) -> Result<(StatusCode, Value)> {
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
            let host = host.to_string();
            let name = name.to_string();
            identifier(&host, false)?;
            identifier(&name, true)?;
            code = StatusCode::ACCEPTED;
            json!({"run":blocking(move||runtime.agents.launch(&host,&name,body)).await?})
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
}
