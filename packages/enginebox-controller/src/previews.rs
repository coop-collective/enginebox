//! Branch previews (docs/ENVIRONMENT-SERVICES.md, "Repository branch
//! previews"): a leased development environment of one branch of a hosted or
//! linked repository, run by the environment service. The controller is the
//! authenticated front door: it checks the caller's repository grant and org,
//! then forwards to the environment service's private API with the operator's
//! token. The request never selects a host, command, image or path; the
//! branch's own `.enginebox/config.json` `dev` section selects the template,
//! and the host's operator-owned exporter fetches the source with a read token
//! that never reaches the container.

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

/// The longest lease one request may ask for; Ideator renews while it needs one.
pub const MAX_TTL_SECONDS: u64 = 3600;
pub const MIN_TTL_SECONDS: u64 = 30;

/// The environment service's private API: (status, JSON body).
pub trait PreviewApi: Send + Sync {
    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Value)>;
}

/// Loopback HTTP (or HTTPS) to the environment service; no redirects.
pub struct HttpPreviewApi {
    url: String,
    token: String,
}
impl PreviewApi for HttpPreviewApi {
    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Value)> {
        let client = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        let url = format!("{}{path}", self.url);
        let mut request = match method {
            "GET" => client.get(url),
            _ => client.post(url).header("content-type", "application/json"),
        }
        .bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.body(body.to_string());
        }
        let response = request
            .send()
            .map_err(|_| anyhow::anyhow!("The environment service is unreachable."))?;
        let status = response.status().as_u16();
        let value = response.json::<Value>().unwrap_or(Value::Null);
        Ok((status, value))
    }
}

#[derive(Clone, Default)]
pub struct Previews {
    api: Option<Arc<dyn PreviewApi>>,
    host: String,
}

/// `ENGINEBOX_ENVIRONMENTS_URL` (loopback http or https, origin only),
/// `ENGINEBOX_ENVIRONMENTS_TOKEN_FILE` and `ENGINEBOX_PREVIEW_HOST` (the
/// environment service's bootstrap host whose `repositoryCommand` exports
/// branches). Unset: previews are not configured.
fn http_api_from_env() -> Result<Option<(HttpPreviewApi, String)>> {
    let (Ok(url), Ok(file), Ok(host)) = (
        std::env::var("ENGINEBOX_ENVIRONMENTS_URL"),
        std::env::var("ENGINEBOX_ENVIRONMENTS_TOKEN_FILE"),
        std::env::var("ENGINEBOX_PREVIEW_HOST"),
    ) else {
        return Ok(None);
    };
    let parsed = reqwest::Url::parse(&url).context("ENGINEBOX_ENVIRONMENTS_URL is not a URL")?;
    ensure!(
        parsed.path() == "/"
            && parsed.query().is_none()
            && parsed.username().is_empty()
            && (parsed.scheme() == "https"
                || (parsed.scheme() == "http"
                    && matches!(parsed.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")))),
        "ENGINEBOX_ENVIRONMENTS_URL must be https, or http to this machine"
    );
    let token = std::fs::read_to_string(&file)
        .context("ENGINEBOX_ENVIRONMENTS_TOKEN_FILE is unreadable")?
        .trim()
        .to_owned();
    ensure!(
        token.len() >= 32,
        "The environment service token is too short"
    );
    ensure!(valid_task(&host), "ENGINEBOX_PREVIEW_HOST is not a host id");
    Ok(Some((
        HttpPreviewApi {
            url: url.trim_end_matches('/').to_owned(),
            token,
        },
        host,
    )))
}

/// The environment service's ids for hosts and tasks.
fn valid_task(value: &str) -> bool {
    let b = value.as_bytes();
    !b.is_empty()
        && b.len() <= 48
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && b[0] != b'-'
        && b[b.len() - 1] != b'-'
}
/// A branch name git and the environment service both accept.
pub fn valid_branch(value: &str) -> bool {
    let b = value.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || b"_./-".contains(c))
        && !value.contains("..")
        && !value.contains("//")
        && !value.ends_with('/')
        && !value.ends_with(".lock")
}
fn valid_id(value: &str) -> bool {
    let b = value.as_bytes();
    (3..=40).contains(&b.len())
        && b.starts_with(b"r-")
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// What callers see: snake_case, without the service's internal fields.
fn public(v: &Value) -> Value {
    json!({
        "id": v["id"], "repository": v["repository"], "branch": v["branch"], "commit": v["commit"],
        "status": v["status"], "healthy": v["healthy"], "urls": v["urls"], "error": v["error"],
        "warnings": v["warnings"].as_array().cloned().unwrap_or_default(),
        "lease": v["lease"], "revision": v["revision"], "expires_at": v["expiresAt"],
        "source_revision": v["sourceRevision"],
    })
}
/// The service's error code as a caller-facing reply.
fn refused(status: u16, v: &Value) -> (u16, Value) {
    let code = v["error"].as_str().unwrap_or("environment-service-error");
    let status = match status {
        404 => 404,
        401 | 403 => 502,
        _ => 409,
    };
    (status, json!({"error": code}))
}

impl Previews {
    pub fn from_env() -> Result<Self> {
        Ok(match http_api_from_env()? {
            Some((api, host)) => Self {
                api: Some(Arc::new(api)),
                host,
            },
            None => Self::default(),
        })
    }
    pub fn with_api(api: Arc<dyn PreviewApi>, host: &str) -> Self {
        Self {
            api: Some(api),
            host: host.to_owned(),
        }
    }
    fn api(&self) -> Result<&Arc<dyn PreviewApi>> {
        self.api
            .as_ref()
            .context("Branch previews are not configured on this controller.")
    }
    pub fn configured(&self) -> bool {
        self.api.is_some()
    }
    /// `{branch, ttl_seconds?, task?}`: start (or return the live) preview of
    /// `repository`'s branch.
    pub fn create(&self, repository: &str, body: &Value) -> Result<(u16, Value)> {
        crate::keys(body, &["branch", "ttl_seconds", "task"], &["branch"])?;
        let branch = body["branch"].as_str().unwrap_or("");
        ensure!(valid_branch(branch), "branch is not a valid branch name.");
        let ttl = ttl(body)?.unwrap_or(MAX_TTL_SECONDS);
        let task = body["task"].as_str().unwrap_or("preview");
        ensure!(
            valid_task(task),
            "task must match ^[a-z0-9][a-z0-9-]{{0,46}}[a-z0-9]$."
        );
        let (status, value) = self.api()?.call(
            "POST",
            "/v1/environments/previews",
            Some(&json!({"host": self.host, "repository": repository, "branch": branch, "task": task, "ttlSeconds": ttl})),
        )?;
        Ok(if (200..300).contains(&status) {
            (202, public(&value))
        } else {
            refused(status, &value)
        })
    }
    /// The preview, if it is `repository`'s.
    pub fn get(&self, repository: &str, id: &str) -> Result<Value> {
        ensure!(valid_id(id), "Unknown preview");
        let (status, value) =
            self.api()?
                .call("GET", &format!("/v1/environments/previews/{id}"), None)?;
        ensure!(
            status == 200 && value["repository"] == repository,
            "Unknown preview"
        );
        Ok(public(&value))
    }
    /// `{lease, expected_revision, ttl_seconds? | revoke? | retry?}`: renew,
    /// retry or revoke `repository`'s preview.
    pub fn change(&self, repository: &str, id: &str, body: &Value) -> Result<(u16, Value)> {
        crate::keys(
            body,
            &[
                "lease",
                "expected_revision",
                "ttl_seconds",
                "revoke",
                "retry",
            ],
            &["lease", "expected_revision"],
        )?;
        if self.get(repository, id).is_err() {
            return Ok((404, json!({"error":"Unknown preview."})));
        }
        let mut forwarded = json!({
            "id": id,
            "lease": body["lease"].as_str().context("lease must be a string.")?,
            "expectedRevision": body["expected_revision"].as_u64().context("expected_revision must be an integer.")?,
        });
        match (body.get("revoke"), ttl(body)?) {
            (Some(Value::Bool(true)), None) => forwarded["revoke"] = json!(true),
            (None, Some(ttl)) => forwarded["ttlSeconds"] = json!(ttl),
            _ => bail!("Send ttl_seconds to renew, or revoke: true."),
        }
        if let Some(retry) = body.get("retry") {
            forwarded["retry"] = json!(retry.as_bool().context("retry must be a boolean.")?);
        }
        let (status, value) =
            self.api()?
                .call("POST", "/v1/environments/previews", Some(&forwarded))?;
        Ok(if (200..300).contains(&status) {
            (200, public(&value))
        } else {
            refused(status, &value)
        })
    }
}

fn ttl(body: &Value) -> Result<Option<u64>> {
    match body.get("ttl_seconds") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => Ok(Some(
            v.as_u64()
                .filter(|t| (MIN_TTL_SECONDS..=MAX_TTL_SECONDS).contains(t))
                .with_context(|| {
                    format!("ttl_seconds must be {MIN_TTL_SECONDS}-{MAX_TTL_SECONDS}.")
                })?,
        )),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct FakeService {
        pub calls: Mutex<Vec<(String, String, Option<Value>)>>,
    }
    impl PreviewApi for FakeService {
        fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Value)> {
            self.calls
                .lock()
                .unwrap()
                .push((method.into(), path.into(), body.cloned()));
            let record = json!({"id":"r-0123456789abcdef0123","repository":"alpha/site","branch":"autopilot/feat-v2","status":"queued","healthy":false,"urls":[],"lease":"l".repeat(64),"revision":1,"expiresAt":"2026-01-01T00:00:00Z","host":"node","task":"feat"});
            Ok(match (method, body) {
                ("POST", Some(b)) if b["branch"] == "bad-template" => {
                    (400, json!({"error":"dev-template-required"}))
                }
                ("POST", _) => (202, record),
                ("GET", _) if path.ends_with("r-0123456789abcdef0123") => (200, record),
                _ => (404, json!({"error":"unknown-preview"})),
            })
        }
    }

    #[test]
    fn create_get_and_change_forward_only_validated_fields() {
        let service = Arc::new(FakeService::default());
        let previews = Previews::with_api(service.clone(), "node");
        let (status, created) = previews
            .create(
                "alpha/site",
                &json!({"branch":"autopilot/feat-v2","task":"feat","ttl_seconds":600}),
            )
            .unwrap();
        assert_eq!(status, 202);
        assert_eq!(created["expires_at"], "2026-01-01T00:00:00Z");
        assert!(created.get("host").is_none());
        let sent = service.calls.lock().unwrap()[0].2.clone().unwrap();
        assert_eq!(
            sent,
            json!({"host":"node","repository":"alpha/site","branch":"autopilot/feat-v2","task":"feat","ttlSeconds":600})
        );
        for bad in [
            json!({"branch":"../x"}),
            json!({"branch":"main","ttl_seconds":5}),
            json!({"branch":"main","task":"Feat_1"}),
            json!({"branch":"main","host":"other"}),
        ] {
            assert!(previews.create("alpha/site", &bad).is_err(), "{bad}");
        }
        assert_eq!(
            previews
                .create("alpha/site", &json!({"branch":"bad-template"}))
                .unwrap(),
            (409, json!({"error":"dev-template-required"}))
        );
        // Another repository's preview is unknown, for reads and changes.
        assert!(previews.get("alpha/site", "r-0123456789abcdef0123").is_ok());
        assert!(
            previews
                .get("alpha/other", "r-0123456789abcdef0123")
                .is_err()
        );
        let change = json!({"lease":"l".repeat(64),"expected_revision":1,"revoke":true});
        assert_eq!(
            previews
                .change("alpha/other", "r-0123456789abcdef0123", &change)
                .unwrap()
                .0,
            404
        );
        assert_eq!(
            previews
                .change("alpha/site", "r-0123456789abcdef0123", &change)
                .unwrap()
                .0,
            200
        );
        let last = service
            .calls
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .2
            .clone()
            .unwrap();
        assert_eq!(last["revoke"], true);
        assert_eq!(last["expectedRevision"], 1);
        assert!(
            previews
                .change(
                    "alpha/site",
                    "r-0123456789abcdef0123",
                    &json!({"lease":"x","expected_revision":1,"revoke":true,"ttl_seconds":60})
                )
                .is_err()
        );
        assert!(!Previews::default().configured());
        assert!(
            Previews::default()
                .create("alpha/site", &json!({"branch":"main"}))
                .is_err()
        );
    }
}
