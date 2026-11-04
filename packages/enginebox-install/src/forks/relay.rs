//! The node's loopback gateway relay to Xava Inference (contract §4.1).
//!
//! An agent run never sees the member's xinf access token. Its harness is
//! configured with a per-run bearer `ebr_<64 hex>` and a base URL of
//! `http://127.0.0.1:<port>`; the relay swaps that bearer for the run's xinf
//! token and forwards a fixed set of model routes to xinf, streaming the
//! response back unchanged. Per run it enforces the run deadline, the xinf
//! token's expiry, a USD budget (the sum of xinf's `x-xm-cost-usd` response
//! header), at most `max_concurrent` requests in flight and `rpm` requests a
//! minute.
//!
//! Only SHA-256 digests of run bearers are kept. Nothing here logs a token or
//! a body, and no error returned to a client carries an upstream token.
use anyhow::{Context, Result, bail, ensure};
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};
use subtle::ConstantTimeEq;

pub const DEFAULT_UPSTREAM: &str = "https://xavainf.com";
/// Largest request body forwarded.
pub const MAX_BODY: usize = 32 << 20;
const ROUTES: &[(&str, &str)] = &[
    ("POST", "/v1/messages"),
    ("POST", "/v1/messages/count_tokens"),
    ("GET", "/v1/models"),
    ("POST", "/v1/responses"),
    ("POST", "/v1/chat/completions"),
];
const FORWARD_REQUEST: &[&str] = &[
    "content-type",
    "accept",
    "anthropic-version",
    "anthropic-beta",
    "openai-beta",
    "user-agent",
];
const FORWARD_RESPONSE: &[&str] = &[
    "content-type",
    "cache-control",
    "request-id",
    "x-request-id",
    "x-xm-cost-usd",
    "retry-after",
];

#[derive(Clone, Debug)]
pub struct RelayConfig {
    /// Upstream origins a grant may name (exact origin, no path).
    pub allowed_upstreams: Vec<String>,
    /// Requests in flight per run.
    pub max_concurrent: usize,
    /// Requests per rolling minute per run.
    pub rpm: u32,
}
impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            allowed_upstreams: vec![DEFAULT_UPSTREAM.into()],
            max_concurrent: 4,
            rpm: 600,
        }
    }
}

/// What the controller delivers for one run.
pub struct RunGrant {
    pub run_id: String,
    /// The member's xinf access token (`xat_…`) or key.
    pub access_token: String,
    /// The upstream origin, e.g. `https://xavainf.com`.
    pub base_url: String,
    pub budget_usd: f64,
    /// The run's end: the bearer stops working then.
    pub expires_at: SystemTime,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct RunUsage {
    pub spent_usd: f64,
    pub requests: u64,
    pub refused: u64,
}

struct Run {
    run_id: String,
    access_token: String,
    base_url: String,
    budget_usd: f64,
    deadline: SystemTime,
    token_expires_at: SystemTime,
    usage: RunUsage,
    in_flight: usize,
    recent: VecDeque<Instant>,
}

struct Inner {
    config: RelayConfig,
    port: u16,
    /// Keyed by SHA-256 of the run bearer.
    runs: Mutex<HashMap<[u8; 32], Run>>,
    client: reqwest::Client,
    loopback_client: reqwest::Client,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[derive(Clone)]
pub struct Relay {
    inner: Arc<Inner>,
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}
fn random_hex(bytes: usize) -> String {
    let mut out = String::new();
    while out.len() < bytes * 2 {
        out.push_str(&uuid::Uuid::new_v4().simple().to_string());
    }
    out.truncate(bytes * 2);
    out
}
/// The canonical origin of `url`, refusing paths, queries and credentials.
fn origin(url: &str) -> Result<String> {
    let parsed = reqwest::Url::parse(url).context("upstream is not a URL")?;
    ensure!(
        matches!(parsed.scheme(), "https" | "http")
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.path() == "/"
            && parsed.query().is_none()
            && parsed.fragment().is_none()
            && parsed.host().is_some(),
        "upstream must be a bare origin"
    );
    Ok(parsed.origin().ascii_serialization())
}
fn loopback(origin: &str) -> bool {
    reqwest::Url::parse(origin).is_ok_and(|u| {
        matches!(
            u.host_str(),
            Some("127.0.0.1") | Some("localhost") | Some("[::1]")
        )
    })
}

impl Relay {
    pub async fn start(config: RelayConfig) -> Result<Relay> {
        ensure!(config.max_concurrent > 0 && config.rpm > 0, "relay limits");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let build = |no_proxy: bool| {
            let mut b = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(15));
            if no_proxy {
                b = b.no_proxy();
            }
            b.build()
        };
        let inner = Arc::new(Inner {
            config,
            port,
            runs: Mutex::new(HashMap::new()),
            client: build(false)?,
            loopback_client: build(true)?,
            task: Mutex::new(None),
        });
        let app = Router::new().fallback(handle).with_state(inner.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        *inner.task.lock().unwrap() = Some(task);
        Ok(Relay { inner })
    }
    pub fn port(&self) -> u16 {
        self.inner.port
    }
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.inner.port)
    }
    /// Admit a run; returns its bearer (`ebr_` + 64 hex), which the relay
    /// itself does not keep.
    pub fn register(&self, grant: RunGrant) -> Result<String> {
        let upstream = origin(&grant.base_url)?;
        let allowed = self
            .inner
            .config
            .allowed_upstreams
            .iter()
            .filter_map(|a| origin(a).ok())
            .any(|a| a == upstream);
        ensure!(allowed, "upstream is not allowed for agent runs");
        ensure!(
            grant.budget_usd.is_finite() && grant.budget_usd > 0.0 && grant.budget_usd <= 100.0,
            "run budget must be above 0 and at most 100 USD"
        );
        ensure!(
            !grant.access_token.trim().is_empty()
                && grant.access_token.len() <= 8192
                && HeaderValue::from_str(&format!("Bearer {}", grant.access_token)).is_ok(),
            "the run has no usable gateway token"
        );
        ensure!(
            !grant.run_id.is_empty() && grant.run_id.len() <= 128,
            "invalid run id"
        );
        ensure!(
            grant.expires_at > SystemTime::now(),
            "the run already ended"
        );
        let token = format!("ebr_{}", random_hex(32));
        let mut runs = self.inner.runs.lock().unwrap();
        if runs.values().any(|r| r.run_id == grant.run_id) {
            bail!("the run is already registered");
        }
        runs.insert(
            digest(&token),
            Run {
                run_id: grant.run_id,
                access_token: grant.access_token,
                base_url: upstream,
                budget_usd: grant.budget_usd,
                deadline: grant.expires_at,
                token_expires_at: grant.expires_at,
                usage: RunUsage::default(),
                in_flight: 0,
                recent: VecDeque::new(),
            },
        );
        Ok(token)
    }
    /// A fresh xinf token for a running run. False when the run is unknown.
    pub fn refresh(&self, run_id: &str, access_token: String, expires_at: SystemTime) -> bool {
        if access_token.trim().is_empty()
            || HeaderValue::from_str(&format!("Bearer {access_token}")).is_err()
        {
            return false;
        }
        let mut runs = self.inner.runs.lock().unwrap();
        match runs.values_mut().find(|r| r.run_id == run_id) {
            Some(run) => {
                run.access_token = access_token;
                run.token_expires_at = expires_at;
                true
            }
            None => false,
        }
    }
    pub fn usage(&self, run_id: &str) -> Option<RunUsage> {
        let runs = self.inner.runs.lock().unwrap();
        runs.values()
            .find(|r| r.run_id == run_id)
            .map(|r| r.usage.clone())
    }
    /// End a run: its bearer stops working. Returns its final usage.
    pub fn revoke(&self, run_id: &str) -> Option<RunUsage> {
        let mut runs = self.inner.runs.lock().unwrap();
        let key = runs
            .iter()
            .find(|(_, r)| r.run_id == run_id)
            .map(|(k, _)| *k)?;
        runs.remove(&key).map(|r| r.usage)
    }
    /// The live xinf tokens, for redaction (run bearers are not kept).
    pub fn secrets(&self) -> Vec<String> {
        let runs = self.inner.runs.lock().unwrap();
        runs.values().map(|r| r.access_token.clone()).collect()
    }
    pub async fn shutdown(self) {
        let task = self.inner.task.lock().unwrap().take();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
        self.inner.runs.lock().unwrap().clear();
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Shape {
    Anthropic,
    OpenAi,
}
fn error(shape: Shape, status: StatusCode, kind: &str, code: &str, message: &str) -> Response {
    let body = match shape {
        Shape::Anthropic => json!({"type": "error", "error": {"type": kind, "message": message}}),
        Shape::OpenAi => json!({"error": {"message": message, "type": kind, "code": code}}),
    };
    (status, axum::Json(body)).into_response()
}
fn unauthorized(shape: Shape) -> Response {
    let kind = if shape == Shape::Anthropic {
        "authentication_error"
    } else {
        "invalid_request_error"
    };
    error(
        shape,
        StatusCode::UNAUTHORIZED,
        kind,
        "invalid_api_key",
        "This agent run's gateway token is not valid (the run ended or was cancelled).",
    )
}
fn too_many(shape: Shape, message: &str) -> Response {
    let kind = if shape == Shape::Anthropic {
        "rate_limit_error"
    } else {
        "rate_limit_exceeded"
    };
    error(
        shape,
        StatusCode::TOO_MANY_REQUESTS,
        kind,
        "rate_limit_exceeded",
        message,
    )
}
fn bearer(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        let v = v.trim();
        if let Some(t) = v
            .strip_prefix("Bearer ")
            .or_else(|| v.strip_prefix("bearer "))
        {
            return Some(t.trim().to_owned());
        }
    }
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_owned())
}

/// Decrements the run's in-flight count when the response (body) is done.
struct Slot {
    inner: Arc<Inner>,
    key: [u8; 32],
}
impl Drop for Slot {
    fn drop(&mut self) {
        if let Ok(mut runs) = self.inner.runs.lock()
            && let Some(run) = runs.get_mut(&self.key)
        {
            run.in_flight = run.in_flight.saturating_sub(1);
        }
    }
}

async fn handle(State(inner): State<Arc<Inner>>, request: Request) -> Response {
    let path = request.uri().path().to_owned();
    let shape = if path.starts_with("/v1/messages")
        || request.headers().contains_key("anthropic-version")
    {
        Shape::Anthropic
    } else {
        Shape::OpenAi
    };
    // Browsers and DNS rebinding: only a loopback Host and no Origin.
    let host_ok = request
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|h| {
            h == format!("127.0.0.1:{}", inner.port) || h == format!("localhost:{}", inner.port)
        });
    if !host_ok || request.headers().contains_key(header::ORIGIN) {
        return error(
            shape,
            StatusCode::FORBIDDEN,
            "permission_error",
            "forbidden",
            "Forbidden.",
        );
    }
    let method = request.method().clone();
    if !ROUTES
        .iter()
        .any(|(m, p)| *p == path && method.as_str() == *m)
    {
        return error(
            shape,
            StatusCode::NOT_FOUND,
            "not_found_error",
            "not_found",
            "Not found.",
        );
    }
    let Some(token) = bearer(request.headers()) else {
        return unauthorized(shape);
    };
    let key = digest(&token);
    drop(token);
    // Admission, under the lock: identity, expiry, budget, limits.
    let (access_token, upstream) = {
        let mut runs = inner.runs.lock().unwrap();
        let Some((_, run)) = runs.iter_mut().find(|(k, _)| bool::from(k.ct_eq(&key))) else {
            return unauthorized(shape);
        };
        let now = SystemTime::now();
        if now >= run.deadline || now >= run.token_expires_at {
            return unauthorized(shape);
        }
        if run.usage.spent_usd >= run.budget_usd {
            run.usage.refused += 1;
            return match shape {
                Shape::Anthropic => error(
                    shape,
                    StatusCode::PAYMENT_REQUIRED,
                    "billing_error",
                    "run_budget_exhausted",
                    "This agent run reached its budget",
                ),
                Shape::OpenAi => error(
                    shape,
                    StatusCode::PAYMENT_REQUIRED,
                    "insufficient_quota",
                    "run_budget_exhausted",
                    "This agent run reached its budget",
                ),
            };
        }
        let instant = Instant::now();
        while run
            .recent
            .front()
            .is_some_and(|t| instant.duration_since(*t) >= Duration::from_secs(60))
        {
            run.recent.pop_front();
        }
        if run.recent.len() >= inner.config.rpm as usize {
            run.usage.refused += 1;
            return too_many(shape, "This agent run sent too many requests this minute.");
        }
        if run.in_flight >= inner.config.max_concurrent {
            run.usage.refused += 1;
            return too_many(shape, "This agent run has too many requests in flight.");
        }
        run.in_flight += 1;
        run.recent.push_back(instant);
        run.usage.requests += 1;
        (run.access_token.clone(), run.base_url.clone())
    };
    let slot = Slot {
        inner: inner.clone(),
        key,
    };
    let query = request
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let url = format!("{upstream}{path}{query}");
    let mut forward = reqwest::header::HeaderMap::new();
    for (name, value) in request.headers() {
        let n = name.as_str();
        if FORWARD_REQUEST.contains(&n) || n.starts_with("x-stainless-") {
            forward.insert(name.clone(), value.clone());
        }
    }
    let Ok(auth) = HeaderValue::from_str(&format!("Bearer {access_token}")) else {
        return unauthorized(shape);
    };
    forward.insert(header::AUTHORIZATION, auth);
    let body = match axum::body::to_bytes(request.into_body(), MAX_BODY).await {
        Ok(b) => b,
        Err(_) => {
            return error(
                shape,
                StatusCode::PAYLOAD_TOO_LARGE,
                "invalid_request_error",
                "request_too_large",
                "The request body is too large.",
            );
        }
    };
    let client = if loopback(&upstream) {
        &inner.loopback_client
    } else {
        &inner.client
    };
    let mut builder = client.request(method.clone(), &url).headers(forward);
    if method != Method::GET {
        builder = builder.body(body);
    }
    let upstream_response = match builder.send().await {
        Ok(r) => r,
        Err(_) => {
            return error(
                shape,
                StatusCode::BAD_GATEWAY,
                "api_error",
                "upstream_unavailable",
                "Xava Inference could not be reached.",
            );
        }
    };
    let cost = upstream_response
        .headers()
        .get("x-xm-cost-usd")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|c| c.is_finite() && *c >= 0.0);
    if let Some(cost) = cost
        && let Some(run) = inner.runs.lock().unwrap().get_mut(&key)
    {
        run.usage.spent_usd += cost;
    }
    let status = upstream_response.status();
    let mut headers = HeaderMap::new();
    for (name, value) in upstream_response.headers() {
        let n = name.as_str();
        if (FORWARD_RESPONSE.contains(&n)
            || n.starts_with("anthropic-")
            || n.starts_with("openai-"))
            && let Ok(name) = HeaderName::from_bytes(name.as_str().as_bytes())
        {
            headers.append(name, value.clone());
        }
    }
    let stream = upstream_response.bytes_stream().map(move |chunk| {
        let _held = &slot;
        chunk
    });
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::{get, post};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    #[derive(Default)]
    struct Upstream {
        seen: Mutex<Vec<HeaderMap>>,
        release: Notify,
        waiting: AtomicUsize,
    }

    async fn record(State(u): State<Arc<Upstream>>, headers: HeaderMap) -> Arc<Upstream> {
        u.seen.lock().unwrap().push(headers);
        u
    }

    async fn fake_upstream() -> (String, Arc<Upstream>) {
        let state = Arc::new(Upstream::default());
        let app = Router::new()
            .route(
                "/v1/messages",
                post(|s: State<Arc<Upstream>>, h: HeaderMap| async move {
                    record(s, h).await;
                    let chunks = [
                        "event: a\ndata: {\"x\":1}\n\n",
                        "event: b\ndata: {\"x\":2}\n\n",
                        "data: [DONE]\n\n",
                    ];
                    let stream = futures_util::stream::iter(chunks).then(|c| async move {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        Ok::<_, std::io::Error>(c)
                    });
                    let mut r = Response::new(Body::from_stream(stream));
                    r.headers_mut().insert(
                        "content-type",
                        HeaderValue::from_static("text/event-stream"),
                    );
                    r.headers_mut()
                        .insert("x-xm-cost-usd", HeaderValue::from_static("0.6"));
                    r.headers_mut()
                        .insert("set-cookie", HeaderValue::from_static("a=b"));
                    r
                }),
            )
            .route(
                "/v1/responses",
                post(|s: State<Arc<Upstream>>, h: HeaderMap| async move {
                    record(s, h).await;
                    ([("x-xm-cost-usd", "1.5")], "{\"ok\":true}")
                }),
            )
            .route(
                "/v1/models",
                get(
                    |s: State<Arc<Upstream>>, h: HeaderMap, uri: axum::http::Uri| async move {
                        record(s, h).await;
                        uri.query().unwrap_or("").to_owned()
                    },
                ),
            )
            .route(
                "/v1/chat/completions",
                post(|s: State<Arc<Upstream>>, h: HeaderMap| async move {
                    let u = record(s, h).await;
                    let wait = u.release.notified();
                    u.waiting.fetch_add(1, Ordering::SeqCst);
                    wait.await;
                    "{}"
                }),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://127.0.0.1:{port}"), state)
    }

    async fn relay_for(upstream: &str) -> Relay {
        Relay::start(RelayConfig {
            allowed_upstreams: vec![upstream.to_owned()],
            ..RelayConfig::default()
        })
        .await
        .unwrap()
    }
    fn grant(run: &str, token: &str, upstream: &str, budget: f64) -> RunGrant {
        RunGrant {
            run_id: run.into(),
            access_token: token.into(),
            base_url: upstream.into(),
            budget_usd: budget,
            expires_at: SystemTime::now() + Duration::from_secs(600),
        }
    }
    fn client() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    #[tokio::test]
    async fn forwards_with_the_run_token_and_strips_client_credentials() {
        let (up, state) = fake_upstream().await;
        let relay = relay_for(&up).await;
        let token = relay
            .register(grant("run-a", "xat_member_a_secret", &up, 5.0))
            .unwrap();
        assert!(token.starts_with("ebr_") && token.len() == 68);
        let r = client()
            .post(format!("{}/v1/messages", relay.base_url()))
            .header("x-api-key", &token)
            .header("cookie", "session=1")
            .header("x-forwarded-for", "1.2.3.4")
            .header("anthropic-version", "2023-06-01")
            .header("x-stainless-lang", "js")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert!(r.headers().get("set-cookie").is_none());
        assert_eq!(r.headers()["x-xm-cost-usd"], "0.6");
        let body = r.text().await.unwrap();
        assert_eq!(
            body,
            "event: a\ndata: {\"x\":1}\n\nevent: b\ndata: {\"x\":2}\n\ndata: [DONE]\n\n"
        );
        let h = state.seen.lock().unwrap()[0].clone();
        assert_eq!(h["authorization"], "Bearer xat_member_a_secret");
        assert!(h.get("x-api-key").is_none());
        assert!(h.get("cookie").is_none());
        assert!(h.get("x-forwarded-for").is_none());
        assert_eq!(h["anthropic-version"], "2023-06-01");
        assert_eq!(h["x-stainless-lang"], "js");
        assert!(!format!("{h:?}").contains(&token));
        // GET /v1/models keeps its query.
        let r = client()
            .get(format!("{}/v1/models?limit=5", relay.base_url()))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(r.text().await.unwrap(), "limit=5");
        let usage = relay.usage("run-a").unwrap();
        assert_eq!(usage.requests, 2);
        assert!((usage.spent_usd - 0.6).abs() < 1e-9);
        assert_eq!(relay.secrets(), vec!["xat_member_a_secret".to_owned()]);
    }

    #[tokio::test]
    async fn budget_cuts_off_the_next_request_in_each_shape() {
        let (up, _) = fake_upstream().await;
        let relay = relay_for(&up).await;
        let token = relay.register(grant("run", "xat_t", &up, 1.0)).unwrap();
        let c = client();
        let send = |path: &'static str| {
            c.post(format!("{}{path}", relay.base_url()))
                .bearer_auth(&token)
                .body("{}")
                .send()
        };
        // 0.6 then 0.6: the second is allowed (spent 0.6 < 1.0), the third refused.
        assert_eq!(send("/v1/messages").await.unwrap().status(), 200);
        let r = send("/v1/messages").await.unwrap();
        assert_eq!(r.status(), 200);
        r.text().await.unwrap();
        let r = send("/v1/messages").await.unwrap();
        assert_eq!(r.status(), 402);
        let v: serde_json::Value = r.json().await.unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["error"]["type"], "billing_error");
        let r = send("/v1/responses").await.unwrap();
        assert_eq!(r.status(), 402);
        let v: serde_json::Value = r.json().await.unwrap();
        assert_eq!(v["error"]["type"], "insufficient_quota");
        assert_eq!(v["error"]["code"], "run_budget_exhausted");
        let usage = relay.usage("run").unwrap();
        assert_eq!((usage.requests, usage.refused), (2, 2));
    }

    #[tokio::test]
    async fn runs_are_isolated_and_revocation_ends_a_token() {
        let (up, state) = fake_upstream().await;
        let relay = relay_for(&up).await;
        let a = relay.register(grant("a", "xat_aaa", &up, 5.0)).unwrap();
        let b = relay.register(grant("b", "xat_bbb", &up, 5.0)).unwrap();
        assert_ne!(a, b);
        let c = client();
        let r = c
            .post(format!("{}/v1/responses", relay.base_url()))
            .bearer_auth(&b)
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(
            state.seen.lock().unwrap().last().unwrap()["authorization"],
            "Bearer xat_bbb"
        );
        let usage = relay.revoke("a").unwrap();
        assert_eq!(usage.requests, 0);
        let r = c
            .post(format!("{}/v1/messages", relay.base_url()))
            .bearer_auth(&a)
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401);
        let v: serde_json::Value = r.json().await.unwrap();
        assert_eq!(v["error"]["type"], "authentication_error");
        let r = c
            .post(format!("{}/v1/responses", relay.base_url()))
            .bearer_auth(&a)
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401);
        let v: serde_json::Value = r.json().await.unwrap();
        assert_eq!(v["error"]["type"], "invalid_request_error");
        assert!(relay.usage("a").is_none());
        let b_usage = relay.usage("b").unwrap();
        assert_eq!(b_usage.requests, 1);
        assert!((b_usage.spent_usd - 1.5).abs() < 1e-9);
        assert_eq!(relay.secrets(), vec!["xat_bbb".to_owned()]);
        // A refresh swaps the upstream token.
        assert!(relay.refresh(
            "b",
            "xat_bbb2".into(),
            SystemTime::now() + Duration::from_secs(60)
        ));
        assert!(!relay.refresh("a", "xat_x".into(), SystemTime::now()));
        c.post(format!("{}/v1/responses", relay.base_url()))
            .bearer_auth(&b)
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(
            state.seen.lock().unwrap().last().unwrap()["authorization"],
            "Bearer xat_bbb2"
        );
    }

    #[tokio::test]
    async fn expired_runs_unknown_paths_and_bad_upstreams_are_refused() {
        let (up, _) = fake_upstream().await;
        let relay = relay_for(&up).await;
        let mut g = grant("short", "xat_s", &up, 1.0);
        g.expires_at = SystemTime::now() + Duration::from_millis(200);
        let token = relay.register(g).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let c = client();
        let r = c
            .post(format!("{}/v1/messages", relay.base_url()))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401);
        // An expired xinf token also stops the run.
        let token = relay.register(grant("tok", "xat_t", &up, 1.0)).unwrap();
        assert!(relay.refresh(
            "tok",
            "xat_t2".into(),
            SystemTime::now() - Duration::from_secs(1)
        ));
        let r = c
            .post(format!("{}/v1/messages", relay.base_url()))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401);
        for (method, path) in [
            ("POST", "/v1/embeddings"),
            ("GET", "/v1/messages"),
            ("POST", "/"),
        ] {
            let r = c
                .request(
                    method.parse().unwrap(),
                    format!("{}{path}", relay.base_url()),
                )
                .bearer_auth(&token)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 404, "{method} {path}");
        }
        assert!(
            relay
                .register(grant("x", "xat", "https://evil.example", 1.0))
                .is_err()
        );
        assert!(
            relay
                .register(grant("x", "xat", &format!("{up}/v1"), 1.0))
                .is_err()
        );
        assert!(relay.register(grant("x", "xat", &up, 0.0)).is_err());
        assert!(relay.register(grant("x", "xat", &up, 101.0)).is_err());
        assert!(relay.register(grant("x", " ", &up, 1.0)).is_err());
        assert!(relay.register(grant("tok", "xat", &up, 1.0)).is_err());
        let default = Relay::start(RelayConfig::default()).await.unwrap();
        assert!(
            default
                .register(grant("x", "xat", DEFAULT_UPSTREAM, 1.0))
                .is_ok()
        );
        assert!(default.register(grant("y", "xat", &up, 1.0)).is_err());
        default.shutdown().await;
    }

    #[tokio::test]
    async fn host_and_origin_guards() {
        let (up, state) = fake_upstream().await;
        let relay = relay_for(&up).await;
        let token = relay.register(grant("run", "xat_t", &up, 1.0)).unwrap();
        let c = client();
        let r = c
            .post(format!("{}/v1/responses", relay.base_url()))
            .header("host", "attacker.example")
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
        let r = c
            .post(format!("{}/v1/responses", relay.base_url()))
            .header("origin", "https://attacker.example")
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
        let r = c
            .post(format!("http://localhost:{}/v1/responses", relay.port()))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(state.seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn concurrency_and_rate_limits() {
        let (up, state) = fake_upstream().await;
        let relay = relay_for(&up).await;
        let token = relay.register(grant("run", "xat_t", &up, 5.0)).unwrap();
        let mut held = Vec::new();
        for _ in 0..4 {
            let url = format!("{}/v1/chat/completions", relay.base_url());
            let token = token.clone();
            held.push(tokio::spawn(async move {
                client()
                    .post(url)
                    .bearer_auth(token)
                    .body("{}")
                    .send()
                    .await
                    .unwrap()
                    .status()
            }));
        }
        while state.waiting.load(Ordering::SeqCst) < 4 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let r = client()
            .post(format!("{}/v1/chat/completions", relay.base_url()))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 429);
        state.release.notify_waiters();
        for h in held {
            assert_eq!(h.await.unwrap(), 200);
        }
        // Slots come back once the bodies are done.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let r = client()
            .post(format!("{}/v1/responses", relay.base_url()))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);

        let limited = Relay::start(RelayConfig {
            allowed_upstreams: vec![up.clone()],
            rpm: 2,
            ..RelayConfig::default()
        })
        .await
        .unwrap();
        let t = limited.register(grant("r", "xat_t", &up, 5.0)).unwrap();
        for expected in [200, 200, 429] {
            let r = client()
                .post(format!("{}/v1/responses", limited.base_url()))
                .bearer_auth(&t)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), expected);
        }
        assert_eq!(limited.usage("r").unwrap().refused, 1);
    }
}
