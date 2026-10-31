//! Hosted Git repositories per project. Each repository lives in a backend;
//! today that is Cloudflare Artifacts reached through the Engine Box
//! Artifacts gateway Worker (`packages/artifacts-gateway`), which holds the
//! Artifacts bindings: one Artifacts namespace per Engine Box project,
//! `<namespace_prefix>-<project>`. The [`Backend`] trait is the seam for
//! further backends (a GitHub App next).
//!
//! The gateway's shared signing secret is owner-only private state: written
//! once through `PUT /v1/repository-backends/artifacts-worker`, never returned
//! or logged. No Cloudflare API token is used. Clients
//! receive short-lived repository tokens only when they ask for one (`POST
//! .../tokens`) or, for a run that names a repository, inside that run's
//! private native home (minted at admission, revoked when the run ends).
//! Repository records hold names, remotes and branches, never a token.
use anyhow::{Context, Result};
use enginebox_agents::{pools, repository};
use enginebox_core::state;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

pub const DEFAULT_NAMESPACE_PREFIX: &str = "enginebox";
const MAX_PER_PROJECT: usize = 1000;
const MAX_TOTAL: usize = 10_000;
const MAX_DESCRIPTION: usize = 512;
const WORKER_FILE: &str = "artifacts-worker.json";
/// Default lifetime of a client-requested repository token.
pub const DEFAULT_TOKEN_TTL_SECONDS: u64 = 3600;

/// A refusal with the HTTP status and message the API returns. Messages never
/// carry a token or a backend response body.
#[derive(Debug)]
pub struct Refusal {
    pub status: u16,
    pub message: String,
}
impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Refusal {}
fn refuse(status: u16, message: impl Into<String>) -> anyhow::Error {
    Refusal {
        status,
        message: message.into(),
    }
    .into()
}
fn invalid(message: impl Into<String>) -> anyhow::Error {
    refuse(400, message)
}
fn not_configured() -> anyhow::Error {
    refuse(409, "No repository backend is configured.")
}
fn unknown_repository() -> anyhow::Error {
    refuse(404, "Unknown repository.")
}
fn upstream() -> anyhow::Error {
    refuse(502, "The repository backend did not accept the request.")
}

/// Read or write access to a project's repositories.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    Read,
    Write,
}
impl Access {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            _ => None,
        }
    }
}

/// A repository as a backend reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct BackendRepository {
    pub id: Option<String>,
    pub name: String,
    pub remote: String,
    pub default_branch: String,
    pub last_push_at: Option<String>,
}
/// A repository token. `Debug` never prints it.
#[derive(Clone)]
pub struct MintedToken {
    pub id: Option<String>,
    pub token: String,
    pub expires_at: String,
}
impl std::fmt::Debug for MintedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MintedToken")
            .field("id", &self.id)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// A repository host. Implementations map an Engine Box project and
/// repository name onto their own namespaces; every method is blocking.
pub trait Backend: Send + Sync {
    /// The `backend` value on repository records.
    fn kind(&self) -> &'static str;
    /// Create the repository, or return the existing one of that name
    /// (`false`: it already existed).
    fn create(
        &self,
        project: &str,
        name: &str,
        description: Option<&str>,
    ) -> Result<(BackendRepository, bool)>;
    fn get(&self, project: &str, name: &str) -> Result<Option<BackendRepository>>;
    fn list(&self, project: &str) -> Result<Vec<BackendRepository>>;
    fn mint(&self, project: &str, name: &str, access: Access, ttl: u64) -> Result<MintedToken>;
    /// Revoke a token by ID. Backends without revocation return Ok.
    fn revoke(&self, project: &str, name: &str, token_id: &str) -> Result<()>;
}

/// GitHub repositories linked to a project: the selected bindings of the
/// controller's GitHub repository App. Tests stand in for GitHub.
pub trait GithubLink: Send + Sync {
    /// The selected binding's default branch, or None when it is not selected.
    fn selected_branch(&self, full_name: &str) -> Option<String>;
    /// A single-repository installation token (about an hour).
    fn token(&self, full_name: &str, write: bool) -> Result<String>;
    /// Open (or find the open) pull request from `head`: `{number, url, reused}`.
    fn open_pull_request(
        &self,
        full_name: &str,
        head: &str,
        title: &str,
        body: &str,
    ) -> Result<Value>;
}
impl GithubLink for enginebox_github::GitHub {
    fn selected_branch(&self, full_name: &str) -> Option<String> {
        self.selected_repository(full_name).map(|r| r.branch)
    }
    fn token(&self, full_name: &str, write: bool) -> Result<String> {
        let minted = self.transfer_credential(full_name, None, write)?;
        Ok(minted["token"].as_str().unwrap_or("").to_owned())
    }
    fn open_pull_request(
        &self,
        full_name: &str,
        head: &str,
        title: &str,
        body: &str,
    ) -> Result<Value> {
        enginebox_github::GitHub::open_pull_request(self, full_name, head, title, body)
    }
}
/// GitHub installation tokens last an hour.
const GITHUB_TOKEN_SECONDS: i64 = 3600;
pub const GITHUB_KIND: &str = "github";

/// One signed POST to a backend service: (status, JSON body). Separate from
/// the backend so tests can stand in for the remote service.
pub trait Transport: Send + Sync {
    fn post(&self, url: &str, headers: &[(&str, String)], body: Vec<u8>) -> Result<(u16, Value)>;
}

/// HTTPS with rustls; no redirects, bounded time. A client is built per
/// request on the calling (blocking) thread, never inside the async runtime.
pub struct HttpTransport;
impl HttpTransport {
    fn client() -> Result<reqwest::blocking::Client> {
        Ok(reqwest::blocking::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(5))
            .build()?)
    }
}
impl Transport for HttpTransport {
    fn post(&self, url: &str, headers: &[(&str, String)], body: Vec<u8>) -> Result<(u16, Value)> {
        let mut request = Self::client()?
            .post(url)
            .header("content-type", "application/json")
            .body(body);
        for (name, value) in headers {
            request = request.header(*name, value);
        }
        let response = request
            .send()
            .map_err(|_| anyhow::anyhow!("Repository backend unreachable"))?;
        let status = response.status().as_u16();
        let bytes = response
            .bytes()
            .map_err(|_| anyhow::anyhow!("Repository backend response unreadable"))?;
        anyhow::ensure!(bytes.len() <= 4 * 1024 * 1024, "Backend response too large");
        Ok((
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        ))
    }
}

/// Owner-supplied configuration of the Artifacts gateway Worker
/// (`packages/artifacts-gateway`). The shared secret signs every request; it
/// is write-only.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerConfig {
    pub url: String,
    secret: String,
    pub namespace_prefix: String,
    pub updated_at: String,
}
impl std::fmt::Debug for WorkerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerConfig")
            .field("url", &self.url)
            .field("namespace_prefix", &self.namespace_prefix)
            .finish()
    }
}
impl WorkerConfig {
    pub fn new(url: &str, secret: &str, namespace_prefix: &str) -> Result<Self> {
        let config = Self {
            url: url.trim_end_matches('/').into(),
            secret: secret.into(),
            namespace_prefix: namespace_prefix.into(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        };
        config.validate()?;
        Ok(config)
    }
    fn validate(&self) -> Result<()> {
        if !valid_gateway_url(&self.url) {
            return Err(invalid(
                "url must be an https URL such as https://<site>/_artifacts (no credentials, query, fragment or workers.dev).",
            ));
        }
        if !valid_secret(&self.secret) {
            return Err(invalid(
                "secret must be 32-256 printable characters without spaces.",
            ));
        }
        if !valid_namespace_prefix(&self.namespace_prefix) {
            return Err(invalid(
                "namespace_prefix must be 1-14 lowercase letters, digits or dashes, starting with a letter or digit.",
            ));
        }
        Ok(())
    }
    /// Redacted view: never the secret.
    pub fn public(&self) -> Value {
        json!({
            "kind":WORKER_KIND,
            "configured":true,
            "url":self.url,
            "namespace_prefix":self.namespace_prefix,
            "secret":"[redacted]",
            "updated_at":self.updated_at,
        })
    }
}
/// The backend kind name in API views.
pub const WORKER_KIND: &str = "artifacts_worker";
fn valid_gateway_url(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    let host = url.host_str().unwrap_or("");
    url.scheme() == "https"
        && !host.is_empty()
        && !host.ends_with(".workers.dev")
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && (url.path() == "/"
            || url
                .path()
                .trim_end_matches('/')
                .split('/')
                .skip(1)
                .all(|segment| {
                    !segment.is_empty()
                        && segment != "."
                        && segment != ".."
                        && segment
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                }))
        && value.len() <= 256
}
fn valid_secret(secret: &str) -> bool {
    (32..=256).contains(&secret.len()) && secret.bytes().all(|b| (33..=126).contains(&b))
}
/// `<prefix>-<project>` must fit the Artifacts namespace limit (63) for the
/// longest project ID (48), so prefixes are at most 14 characters.
pub fn valid_namespace_prefix(prefix: &str) -> bool {
    let b = prefix.as_bytes();
    (1..=14).contains(&b.len())
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}
/// The Artifacts namespace of one Engine Box project.
pub fn namespace(prefix: &str, project: &str) -> Result<String> {
    if !valid_namespace_prefix(prefix) || !pools::valid_project(project) {
        return Err(invalid("Invalid namespace."));
    }
    let ns = format!("{prefix}-{project}");
    anyhow::ensure!((2..=63).contains(&ns.len()), "Invalid namespace.");
    Ok(ns)
}

/// The canonical string the gateway verifies (see `packages/artifacts-gateway`).
pub fn canonical(path: &str, timestamp: u64, nonce: &str, body: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!(
        "v1\nPOST\n{path}\n{timestamp}\n{nonce}\n{:x}",
        Sha256::digest(body)
    )
}

/// Cloudflare Artifacts through the gateway Worker's bindings. No Cloudflare
/// API token is involved; requests are HMAC-signed with the shared secret.
pub struct ArtifactsWorkerBackend {
    config: WorkerConfig,
    transport: Arc<dyn Transport>,
}
impl ArtifactsWorkerBackend {
    pub fn new(config: WorkerConfig, transport: Arc<dyn Transport>) -> Self {
        Self { config, transport }
    }
    fn namespace(&self, project: &str) -> Result<String> {
        namespace(&self.config.namespace_prefix, project)
    }
    /// One signed call: `operation` is `repos.create`, `repos.get`,
    /// `repos.list`, `tokens.create` or `tokens.revoke`.
    fn call(&self, operation: &str, body: &Value) -> Result<(u16, Value)> {
        let bytes = serde_json::to_vec(body)?;
        let path = format!("/{operation}");
        let timestamp = chrono::Utc::now().timestamp().max(0) as u64;
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let signature = crate::callbacks::hmac_sha256_hex(
            self.config.secret.as_bytes(),
            canonical(&path, timestamp, &nonce, &bytes).as_bytes(),
        );
        let (status, response) = self
            .transport
            .post(
                &format!("{}{path}", self.config.url),
                &[
                    ("x-enginebox-timestamp", timestamp.to_string()),
                    ("x-enginebox-nonce", nonce),
                    ("x-enginebox-signature", format!("v1={signature}")),
                ],
                bytes,
            )
            .map_err(|_| upstream())?;
        let code = response["error"].as_str().unwrap_or("");
        match (status, code) {
            (404, "namespace_not_provisioned") => Err(refuse(
                409,
                "The repository gateway does not serve this project's namespace; add the project to the gateway's configuration and redeploy it.",
            )),
            (403, "namespace_not_allowed") => Err(refuse(
                409,
                "The repository gateway refuses this namespace; its namespace prefix differs from the configured one.",
            )),
            (401, _) => {
                Self::log_refusal(operation, status, code);
                Err(refuse(
                    502,
                    "The repository gateway refused the controller's signature; check the shared secret.",
                ))
            }
            _ => Ok((status, response)),
        }
    }
    /// Log a refusal: operation, status and error code only.
    fn log_refusal(operation: &str, status: u16, code: &str) {
        eprintln!(
            "{}",
            json!({"event":"enginebox.repositories.backend_refused","backend":WORKER_KIND,"operation":operation,"status":status,"code":code})
        );
    }
    fn failed(operation: &str, status: u16, response: &Value) -> anyhow::Error {
        Self::log_refusal(operation, status, response["error"].as_str().unwrap_or(""));
        upstream()
    }
    fn repository(value: &Value, need_remote: bool) -> Result<BackendRepository> {
        let text = |key: &str| value[key].as_str().map(str::to_owned);
        let repository = BackendRepository {
            id: text("id"),
            name: text("name").context("Backend repository without a name")?,
            remote: text("remote").unwrap_or_default(),
            default_branch: text("default_branch").unwrap_or_else(|| "main".into()),
            last_push_at: text("last_push_at"),
        };
        if (need_remote && !repository::valid_remote(&repository.remote))
            || !repository::valid_branch(&repository.default_branch)
        {
            return Err(upstream());
        }
        Ok(repository)
    }
}
impl Backend for ArtifactsWorkerBackend {
    fn kind(&self) -> &'static str {
        WORKER_KIND
    }
    fn create(
        &self,
        project: &str,
        name: &str,
        description: Option<&str>,
    ) -> Result<(BackendRepository, bool)> {
        let mut body = json!({"namespace":self.namespace(project)?,"name":name});
        if let Some(d) = description.filter(|d| !d.is_empty()) {
            body["description"] = json!(d);
        }
        // The gateway revokes the creation token itself and never returns it.
        let (status, response) = self.call("repos.create", &body)?;
        match status {
            200 => Ok((Self::repository(&response["repository"], true)?, true)),
            409 if response["error"] == "already_exists" => {
                let existing = self.get(project, name)?.ok_or_else(upstream)?;
                Ok((existing, false))
            }
            _ => Err(Self::failed("repos.create", status, &response)),
        }
    }
    fn get(&self, project: &str, name: &str) -> Result<Option<BackendRepository>> {
        let body = json!({"namespace":self.namespace(project)?,"name":name});
        let (status, response) = self.call("repos.get", &body)?;
        match status {
            200 => Ok(Some(Self::repository(&response["repository"], true)?)),
            404 => Ok(None),
            _ => Err(Self::failed("repos.get", status, &response)),
        }
    }
    fn list(&self, project: &str) -> Result<Vec<BackendRepository>> {
        let namespace = self.namespace(project)?;
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        // At most 10 pages of 200 (MAX_PER_PROJECT fits).
        for _ in 0..10 {
            let mut body = json!({"namespace":namespace});
            if let Some(c) = &cursor {
                body["cursor"] = json!(c);
            }
            let (status, response) = self.call("repos.list", &body)?;
            if status != 200 {
                return Err(Self::failed("repos.list", status, &response));
            }
            let page = response["repositories"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            out.extend(page.iter().filter_map(|r| Self::repository(r, false).ok()));
            cursor = response["cursor"]
                .as_str()
                .filter(|c| !c.is_empty())
                .map(str::to_owned);
            if page.is_empty() || cursor.is_none() {
                break;
            }
        }
        Ok(out)
    }
    fn mint(&self, project: &str, name: &str, access: Access, ttl: u64) -> Result<MintedToken> {
        let body = json!({"namespace":self.namespace(project)?,"repo":name,"scope":access.as_str(),"ttl":ttl});
        let (status, response) = self.call("tokens.create", &body)?;
        if status == 404 {
            return Err(unknown_repository());
        }
        if status != 200 {
            return Err(Self::failed("tokens.create", status, &response));
        }
        let result = &response["token"];
        let token = result["plaintext"].as_str().unwrap_or("").to_owned();
        if !repository::valid_token(&token) || result["scope"] != access.as_str() {
            return Err(upstream());
        }
        let expires_at = result["expires_at"]
            .as_str()
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
            .map(|at| at.with_timezone(&chrono::Utc).to_rfc3339())
            .ok_or_else(upstream)?;
        Ok(MintedToken {
            id: result["id"].as_str().map(str::to_owned),
            token,
            expires_at,
        })
    }
    fn revoke(&self, project: &str, name: &str, token_id: &str) -> Result<()> {
        if token_id.is_empty()
            || token_id.len() > 128
            || !token_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(invalid("Invalid token ID."));
        }
        let body = json!({"namespace":self.namespace(project)?,"repo":name,"id":token_id});
        let (status, response) = self.call("tokens.revoke", &body)?;
        if status == 200 || status == 404 {
            return Ok(());
        }
        Err(Self::failed("tokens.revoke", status, &response))
    }
}

/// One repository record (`repositories/records/<project>/<name>.json`).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub project: String,
    pub name: String,
    pub backend: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub remote: String,
    pub default_branch: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_id: Option<String>,
    /// The org that owns the repository. Omitted for the platform org so
    /// older releases still read the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_id: Option<String>,
}
impl Record {
    /// The owning org (the platform org when unset).
    pub fn org(&self) -> &str {
        self.org_id.as_deref().unwrap_or(pools::DEFAULT_ORG)
    }
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            pools::valid_project(&self.project)
                && repository::valid_name(&self.name)
                && repository::valid_remote(&self.remote)
                && repository::valid_branch(&self.default_branch)
                && (self.backend == "artifacts"
                    || (self.backend == GITHUB_KIND
                        && self.backend_id.as_deref().is_some_and(|f| {
                            self.remote == format!("https://github.com/{f}.git")
                        })))
                && self.description.len() <= MAX_DESCRIPTION
                && self
                    .org_id
                    .as_deref()
                    .is_none_or(|o| pools::valid_org(o) && o != pools::DEFAULT_ORG),
            "Invalid repository record"
        );
        Ok(())
    }
    pub fn public(&self, last_push_at: Option<&str>) -> Value {
        json!({
            "project":self.project,
            "name":self.name,
            "org_id":self.org(),
            "backend":self.backend,
            "description":self.description,
            "remote":self.remote,
            "default_branch":self.default_branch,
            "created_at":self.created_at,
            "last_push_at":last_push_at,
        })
    }
}

#[derive(Clone)]
pub struct Repositories {
    inner: Arc<Inner>,
}
struct Inner {
    records_root: PathBuf,
    backends_root: PathBuf,
    records: Mutex<BTreeMap<(String, String), Record>>,
    worker: Mutex<Option<WorkerConfig>>,
    /// Serializes creation so one name creates one repository.
    creating: Mutex<()>,
    transport: Arc<dyn Transport>,
    github: Mutex<Option<Arc<dyn GithubLink>>>,
}
impl Repositories {
    pub fn open(root: &Path) -> Result<Self> {
        Self::with_transport(root, Arc::new(HttpTransport))
    }
    /// Open with another transport (tests stand in for the gateway).
    pub fn with_transport(root: &Path, transport: Arc<dyn Transport>) -> Result<Self> {
        let base = root.join("repositories");
        let records_root = base.join("records");
        let backends_root = base.join("backends");
        state::private_dir(&records_root)?;
        state::private_dir(&backends_root)?;
        let mut records = BTreeMap::new();
        for entry in std::fs::read_dir(&records_root)? {
            let dir = entry?.path();
            let project = dir
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_owned();
            if project.starts_with('.') {
                continue;
            }
            anyhow::ensure!(
                pools::valid_project(&project) && dir.is_dir(),
                "Invalid repository records directory"
            );
            for path in state::json_files(&dir)? {
                let record: Record = serde_json::from_value(state::read_json(&path)?)?;
                record.validate()?;
                anyhow::ensure!(
                    record.project == project
                        && path.file_stem().and_then(|s| s.to_str()) == Some(record.name.as_str()),
                    "Invalid repository record"
                );
                records.insert((record.project.clone(), record.name.clone()), record);
            }
        }
        let worker_path = backends_root.join(WORKER_FILE);
        let worker = if worker_path.exists() {
            let config: WorkerConfig = serde_json::from_value(state::read_json(&worker_path)?)?;
            config.validate()?;
            Some(config)
        } else {
            None
        };
        Ok(Self {
            inner: Arc::new(Inner {
                records_root,
                backends_root,
                records: Mutex::new(records),
                worker: Mutex::new(worker),
                creating: Mutex::new(()),
                transport,
                github: Mutex::new(None),
            }),
        })
    }

    /// Link GitHub: the controller's GitHub repository App (or a test stand-in).
    pub fn with_github(self, github: Arc<dyn GithubLink>) -> Self {
        *self.inner.github.lock().unwrap() = Some(github);
        self
    }
    /// `with_github` when GitHub is configured; unchanged otherwise.
    pub fn with_github_if(self, github: Option<Arc<dyn GithubLink>>) -> Self {
        match github {
            Some(g) => self.with_github(g),
            None => self,
        }
    }
    fn github(&self) -> Result<Arc<dyn GithubLink>> {
        self.inner
            .github
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| refuse(409, "GitHub is not configured on this controller."))
    }

    // ------------------------------------------------------------ backend config

    /// `PUT /v1/repository-backends/artifacts-worker`.
    pub fn put_worker(&self, body: &Value) -> Result<Value> {
        let object = body
            .as_object()
            .ok_or_else(|| invalid("Expected an object."))?;
        if !object
            .keys()
            .all(|k| ["url", "secret", "namespace_prefix"].contains(&k.as_str()))
        {
            return Err(invalid("Accepted fields: url, secret, namespace_prefix."));
        }
        let url = body["url"]
            .as_str()
            .ok_or_else(|| invalid("url is required."))?;
        let secret = body["secret"]
            .as_str()
            .ok_or_else(|| invalid("secret is required."))?;
        let prefix = match body.get("namespace_prefix") {
            None | Some(Value::Null) => DEFAULT_NAMESPACE_PREFIX,
            Some(Value::String(p)) => p.as_str(),
            Some(_) => return Err(invalid("namespace_prefix must be a string.")),
        };
        let config = WorkerConfig::new(url, secret, prefix)?;
        let mut current = self.inner.worker.lock().unwrap();
        let has_records = !self.inner.records.lock().unwrap().is_empty();
        if has_records
            && current
                .as_ref()
                .is_some_and(|c| c.namespace_prefix != config.namespace_prefix)
        {
            return Err(refuse(
                409,
                "Repositories exist under the current namespace prefix; only the URL and secret can be replaced.",
            ));
        }
        state::write_json(
            &self.inner.backends_root.join(WORKER_FILE),
            &serde_json::to_value(&config)?,
        )?;
        let public = config.public();
        *current = Some(config);
        Ok(json!({"backend":public}))
    }
    /// `GET /v1/repository-backends/artifacts-worker` (redacted).
    pub fn get_worker(&self) -> Value {
        match &*self.inner.worker.lock().unwrap() {
            Some(config) => json!({"backend":config.public()}),
            None => json!({"backend":{"kind":WORKER_KIND,"configured":false}}),
        }
    }
    /// `GET /v1/repository-backends`: the selected backend (exactly one may
    /// be configured), or `configured: false`.
    pub fn backend_view(&self) -> Value {
        match &*self.inner.worker.lock().unwrap() {
            Some(config) => json!({"backend":config.public()}),
            None => json!({"backend":{"kind":null,"configured":false}}),
        }
    }
    /// `DELETE /v1/repository-backends/artifacts-worker`. Repository records
    /// stay; operations that need the backend answer 409 until it is
    /// configured again.
    pub fn delete_worker(&self) -> Result<Value> {
        let mut current = self.inner.worker.lock().unwrap();
        match std::fs::remove_file(self.inner.backends_root.join(WORKER_FILE)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let deleted = current.take().is_some();
        Ok(json!({"deleted":deleted,"backend":{"kind":WORKER_KIND,"configured":false}}))
    }
    /// The one configured backend; 409 when none is.
    pub fn backend(&self) -> Result<Box<dyn Backend>> {
        let config = self
            .inner
            .worker
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(not_configured)?;
        Ok(Box::new(ArtifactsWorkerBackend::new(
            config,
            self.inner.transport.clone(),
        )))
    }

    // ------------------------------------------------------------ repositories

    fn check_project(project: &str) -> Result<()> {
        if pools::valid_project(project) {
            Ok(())
        } else {
            Err(invalid("project must be a project ID."))
        }
    }
    fn record(&self, project: &str, name: &str) -> Result<Record> {
        Self::check_project(project)?;
        if !repository::valid_name(name) {
            return Err(unknown_repository());
        }
        self.inner
            .records
            .lock()
            .unwrap()
            .get(&(project.to_owned(), name.to_owned()))
            .cloned()
            .ok_or_else(unknown_repository)
    }
    pub fn exists(&self, project: &str, name: &str) -> bool {
        self.record(project, name).is_ok()
    }
    /// The org that owns a repository, if it exists.
    pub fn org_of(&self, project: &str, name: &str) -> Option<String> {
        self.record(project, name).ok().map(|r| r.org().to_owned())
    }
    /// `POST /v1/projects/:project/repositories`: create, or return the
    /// existing repository of that name (`false`).
    pub fn create(&self, project: &str, body: &Value) -> Result<(bool, Value)> {
        Self::check_project(project)?;
        let object = body
            .as_object()
            .ok_or_else(|| invalid("Expected an object."))?;
        if !object
            .keys()
            .all(|k| ["name", "description", "org_id"].contains(&k.as_str()))
        {
            return Err(invalid("Accepted fields: name, description, org_id."));
        }
        // The creating org (the dashboard names it for orgs other than the platform org).
        let org_id = match body.get("org_id") {
            None | Some(Value::Null) => None,
            Some(Value::String(o)) if pools::valid_org(o) => {
                (o != pools::DEFAULT_ORG).then(|| o.clone())
            }
            Some(_) => return Err(invalid("org_id must be an org ID.")),
        };
        let name = body["name"].as_str().unwrap_or("");
        if !repository::valid_name(name) {
            return Err(invalid(
                "name must be 1-63 lowercase letters, digits, '.', '_' or '-', starting with a letter or digit and not ending in .git.",
            ));
        }
        let description = match body.get("description") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(d))
                if d.len() <= MAX_DESCRIPTION && !d.chars().any(char::is_control) =>
            {
                d.clone()
            }
            Some(_) => {
                return Err(invalid("description must be at most 512 printable bytes."));
            }
        };
        let _creating = self.inner.creating.lock().unwrap();
        if let Ok(existing) = self.record(project, name) {
            // Idempotent for its own org only; never reveal or return another org's repository.
            if existing.org_id != org_id {
                return Err(refuse(
                    409,
                    "A repository of that name already exists in this project.",
                ));
            }
            return Ok((false, json!({"repository":existing.public(None)})));
        }
        {
            let records = self.inner.records.lock().unwrap();
            if records.len() >= MAX_TOTAL
                || records.keys().filter(|(p, _)| p == project).count() >= MAX_PER_PROJECT
            {
                return Err(refuse(409, "Repository limit reached."));
            }
        }
        let backend = self.backend()?;
        let (created, fresh) = backend.create(
            project,
            name,
            (!description.is_empty()).then_some(description.as_str()),
        )?;
        if created.name != name {
            return Err(upstream());
        }
        let record = Record {
            project: project.to_owned(),
            name: name.to_owned(),
            // Where the repository lives; the access path is the configured backend.
            backend: "artifacts".to_owned(),
            description,
            remote: created.remote,
            default_branch: created.default_branch,
            created_at: chrono::Utc::now().to_rfc3339(),
            backend_id: created.id,
            org_id,
        };
        record.validate()?;
        state::write_json(
            &self
                .inner
                .records_root
                .join(project)
                .join(format!("{name}.json")),
            &serde_json::to_value(&record)?,
        )?;
        let public = record.public(created.last_push_at.as_deref());
        self.inner
            .records
            .lock()
            .unwrap()
            .insert((project.to_owned(), name.to_owned()), record);
        Ok((fresh, json!({"repository":public})))
    }
    /// `POST /v1/projects/:project/repositories/github` (owner): link a GitHub
    /// repository the owner selected in the GitHub panel as `name` in the
    /// project. Runs on it get a single-repository installation token.
    pub fn link_github(&self, project: &str, body: &Value) -> Result<(bool, Value)> {
        Self::check_project(project)?;
        let object = body
            .as_object()
            .ok_or_else(|| invalid("Expected an object."))?;
        if !object
            .keys()
            .all(|k| ["name", "full_name", "description"].contains(&k.as_str()))
        {
            return Err(invalid("Accepted fields: name, full_name, description."));
        }
        let full_name = body["full_name"].as_str().unwrap_or("");
        let valid_part = |p: &str| {
            !p.is_empty()
                && p.len() <= 100
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                && !p.starts_with('.')
        };
        if !full_name
            .split_once('/')
            .is_some_and(|(o, r)| valid_part(o) && valid_part(r) && !r.ends_with(".git"))
        {
            return Err(invalid("full_name must be owner/repository."));
        }
        let name = match body.get("name") {
            None | Some(Value::Null) => full_name.split_once('/').unwrap().1.to_ascii_lowercase(),
            Some(Value::String(n)) => n.clone(),
            Some(_) => return Err(invalid("name must be a string.")),
        };
        if !repository::valid_name(&name) {
            return Err(invalid(
                "name must be 1-63 lowercase letters, digits, '.', '_' or '-', starting with a letter or digit and not ending in .git.",
            ));
        }
        let description = match body.get("description") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(d))
                if d.len() <= MAX_DESCRIPTION && !d.chars().any(char::is_control) =>
            {
                d.clone()
            }
            Some(_) => return Err(invalid("description must be at most 512 printable bytes.")),
        };
        let github = self.github()?;
        let branch = github.selected_branch(full_name).ok_or_else(|| {
            refuse(
                409,
                "Select this repository in the dashboard's GitHub panel first.",
            )
        })?;
        let _creating = self.inner.creating.lock().unwrap();
        if let Ok(existing) = self.record(project, &name) {
            if existing.backend == GITHUB_KIND && existing.backend_id.as_deref() == Some(full_name)
            {
                return Ok((false, json!({"repository":existing.public(None)})));
            }
            return Err(refuse(
                409,
                "A repository of that name already exists in this project.",
            ));
        }
        let record = Record {
            project: project.to_owned(),
            name: name.clone(),
            backend: GITHUB_KIND.to_owned(),
            description,
            remote: format!("https://github.com/{full_name}.git"),
            default_branch: branch,
            created_at: chrono::Utc::now().to_rfc3339(),
            backend_id: Some(full_name.to_owned()),
            org_id: None,
        };
        record
            .validate()
            .map_err(|_| invalid("The GitHub repository cannot be linked under that name."))?;
        state::write_json(
            &self
                .inner
                .records_root
                .join(project)
                .join(format!("{name}.json")),
            &serde_json::to_value(&record)?,
        )?;
        let public = record.public(None);
        self.inner
            .records
            .lock()
            .unwrap()
            .insert((project.to_owned(), name), record);
        Ok((true, json!({"repository":public})))
    }
    /// A token for a record, from its own backend.
    fn mint_for(
        &self,
        record: &Record,
        access: Access,
        ttl: u64,
    ) -> Result<(MintedToken, repository::Auth)> {
        if record.backend == GITHUB_KIND {
            let full_name = record.backend_id.as_deref().unwrap_or("");
            let token = self
                .github()?
                .token(full_name, access == Access::Write)
                .map_err(|_| refuse(502, "GitHub did not issue a token for this repository; check the App installation and the panel selection."))?;
            if !repository::valid_token(&token) {
                return Err(upstream());
            }
            let expires = chrono::Utc::now()
                + chrono::Duration::seconds(GITHUB_TOKEN_SECONDS.min(ttl as i64));
            return Ok((
                MintedToken {
                    id: None,
                    token,
                    expires_at: expires.to_rfc3339(),
                },
                repository::Auth::GithubToken,
            ));
        }
        Ok((
            self.backend()?
                .mint(&record.project, &record.name, access, ttl)?,
            repository::Auth::Bearer,
        ))
    }
    /// `GET /v1/projects/:project/repositories`. Last push times come from the
    /// backend when it answers; records are listed either way.
    pub fn list(&self, project: &str) -> Result<Value> {
        Self::check_project(project)?;
        let records: Vec<Record> = self
            .inner
            .records
            .lock()
            .unwrap()
            .values()
            .filter(|r| r.project == project)
            .cloned()
            .collect();
        let pushes: BTreeMap<String, Option<String>> = if records.is_empty() {
            BTreeMap::new()
        } else {
            self.backend()
                .and_then(|b| b.list(project))
                .map(|listed| {
                    listed
                        .into_iter()
                        .map(|r| (r.name, r.last_push_at))
                        .collect()
                })
                .unwrap_or_default()
        };
        Ok(json!({
            "project":project,
            "repositories":records
                .iter()
                .map(|r| r.public(pushes.get(&r.name).and_then(|p| p.as_deref())))
                .collect::<Vec<_>>(),
        }))
    }
    /// Owner view of every repository (no backend calls).
    pub fn list_all(&self) -> Value {
        json!({"repositories":self
            .inner
            .records
            .lock()
            .unwrap()
            .values()
            .map(|r| r.public(None))
            .collect::<Vec<_>>()})
    }
    /// `GET /v1/projects/:project/repositories/:name`.
    pub fn get(&self, project: &str, name: &str) -> Result<Value> {
        let record = self.record(project, name)?;
        let last_push = if record.backend == GITHUB_KIND {
            None
        } else {
            self.backend()
                .and_then(|b| b.get(project, name))
                .ok()
                .flatten()
                .and_then(|r| r.last_push_at)
        };
        Ok(json!({"repository":record.public(last_push.as_deref())}))
    }
    /// Parse a token request: `{access, ttl_seconds?}`.
    pub fn token_request(body: &Value) -> Result<(Access, u64)> {
        let object = body
            .as_object()
            .ok_or_else(|| invalid("Expected an object."))?;
        if !object
            .keys()
            .all(|k| ["access", "ttl_seconds"].contains(&k.as_str()))
        {
            return Err(invalid("Accepted fields: access, ttl_seconds."));
        }
        let access = body["access"]
            .as_str()
            .and_then(Access::parse)
            .ok_or_else(|| invalid("access must be read or write."))?;
        let ttl = match body.get("ttl_seconds") {
            None | Some(Value::Null) => DEFAULT_TOKEN_TTL_SECONDS,
            Some(v) => v
                .as_u64()
                .filter(|t| {
                    (repository::MIN_TOKEN_TTL_SECONDS..=repository::MAX_TOKEN_TTL_SECONDS)
                        .contains(t)
                })
                .ok_or_else(|| {
                    invalid(format!(
                        "ttl_seconds must be {}-{}.",
                        repository::MIN_TOKEN_TTL_SECONDS,
                        repository::MAX_TOKEN_TTL_SECONDS
                    ))
                })?,
        };
        Ok((access, ttl))
    }
    /// `POST /v1/projects/:project/repositories/:name/tokens`: a token for the
    /// caller's own short use. Nothing about it is stored.
    pub fn mint(&self, project: &str, name: &str, access: Access, ttl: u64) -> Result<Value> {
        let record = self.record(project, name)?;
        let (minted, auth) = self.mint_for(&record, access, ttl)?;
        Ok(json!({
            "token":minted.token,
            "expires_at":minted.expires_at,
            "remote":record.remote,
            "default_branch":record.default_branch,
            "access":access.as_str(),
            // How git presents it: bearer, or github_token (Basic x-access-token).
            "auth":auth,
        }))
    }
    /// A run's write token (`<project>/<name>`), minted at admission.
    pub fn run_credential(&self, reference: &str, ttl: u64) -> Result<repository::Credential> {
        self.credential(reference, Access::Write, ttl)
    }
    /// Open a pull request from `head` on a GitHub-linked repository; None for
    /// repositories on another backend, which have no pull requests.
    pub fn open_pull_request(
        &self,
        project: &str,
        name: &str,
        head: &str,
        title: &str,
        body: &str,
    ) -> Result<Option<Value>> {
        let record = self.record(project, name)?;
        if record.backend != GITHUB_KIND {
            return Ok(None);
        }
        let full_name = record.backend_id.as_deref().unwrap_or("");
        self.github()?
            .open_pull_request(full_name, head, title, body)
            .map(Some)
    }
    /// A read token for the controller's own reads (the repository config).
    pub fn read_credential(&self, reference: &str, ttl: u64) -> Result<repository::Credential> {
        self.credential(reference, Access::Read, ttl)
    }
    fn credential(
        &self,
        reference: &str,
        access: Access,
        ttl: u64,
    ) -> Result<repository::Credential> {
        let (project, name) = repository::parse_ref(reference)?;
        let record = self.record(&project, &name)?;
        let (minted, auth) = self.mint_for(&record, access, ttl)?;
        Ok(repository::Credential {
            remote: record.remote,
            default_branch: record.default_branch,
            token: minted.token,
            token_id: minted.id,
            expires_at: minted.expires_at,
            auth,
        })
    }
    /// Revoke a run's token when the run ends. Failures are logged without
    /// the token; the token still expires on its own.
    pub fn revoke_run_credential(&self, reference: &str, credential: &repository::Credential) {
        let Some(id) = &credential.token_id else {
            return;
        };
        let outcome = repository::parse_ref(reference)
            .and_then(|(project, name)| self.backend()?.revoke(&project, &name, id));
        if outcome.is_err() {
            eprintln!(
                "{}",
                json!({"event":"enginebox.repositories.revoke_failed","repository":reference,"expires_at":credential.expires_at})
            );
        }
    }
}

/// The status and message for a repository error.
pub fn status_of(error: &anyhow::Error) -> (u16, String) {
    match error.downcast_ref::<Refusal>() {
        Some(r) => (r.status, r.message.clone()),
        None => (
            400,
            "Request could not be admitted. Check its fields.".to_owned(),
        ),
    }
}

#[cfg(test)]
pub mod mock {
    //! A stand-in for the Artifacts gateway Worker: it verifies the signature,
    //! timestamp window and nonce exactly as the Worker does, then serves the
    //! five operations from memory.
    use super::*;
    pub const SECRET: &str = "test-only-gateway-secret-0123456789abcdef";
    /// The site Worker's forwarding prefix, as deployed.
    pub const URL: &str = "https://site.example.test/_artifacts";
    #[derive(Default)]
    pub struct State {
        /// (namespace, name) -> (id, default_branch, last_push_at)
        pub repos: BTreeMap<(String, String), (String, String, Option<String>)>,
        /// token id -> (namespace, repo, scope, plaintext, revoked)
        pub tokens: BTreeMap<String, (String, String, String, String, bool)>,
        /// (operation, body) of every accepted call.
        pub calls: Vec<(String, Value)>,
        pub nonces: std::collections::BTreeSet<String>,
        /// Every URL the controller posted to.
        pub urls: Vec<String>,
        pub rejected: usize,
        pub fail: bool,
    }
    pub struct FakeGateway {
        pub account: String,
        pub provisioned: Vec<String>,
        pub state: Mutex<State>,
    }
    impl FakeGateway {
        pub fn new(account: &str, provisioned: &[&str]) -> Arc<Self> {
            Arc::new(Self {
                account: account.into(),
                provisioned: provisioned.iter().map(|s| s.to_string()).collect(),
                state: Mutex::new(State::default()),
            })
        }
        pub fn issued(&self) -> Vec<String> {
            self.state
                .lock()
                .unwrap()
                .tokens
                .values()
                .map(|t| t.3.clone())
                .collect()
        }
        fn remote(&self, ns: &str, name: &str) -> String {
            format!(
                "https://{}.artifacts.cloudflare.net/git/{ns}/{name}.git",
                self.account
            )
        }
    }
    fn reply(status: u16, body: Value) -> (u16, Value) {
        (status, body)
    }
    impl Transport for FakeGateway {
        fn post(
            &self,
            url: &str,
            headers: &[(&str, String)],
            body: Vec<u8>,
        ) -> Result<(u16, Value)> {
            let mut s = self.state.lock().unwrap();
            if s.fail {
                anyhow::bail!("connection refused");
            }
            s.urls.push(url.to_owned());
            // The site Worker forwards `/_artifacts/<op>` as path `/<op>`.
            let path = url.strip_prefix(URL).context("unexpected URL")?;
            let header = |name: &str| {
                headers
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default()
            };
            let timestamp: u64 = header("x-enginebox-timestamp").parse().unwrap_or(0);
            let nonce = header("x-enginebox-nonce");
            let expected = crate::callbacks::hmac_sha256_hex(
                SECRET.as_bytes(),
                canonical(path, timestamp, &nonce, &body).as_bytes(),
            );
            let now = chrono::Utc::now().timestamp() as u64;
            if header("x-enginebox-signature") != format!("v1={expected}")
                || now.abs_diff(timestamp) > 300
                || nonce.len() != 32
                || !s.nonces.insert(nonce)
            {
                s.rejected += 1;
                return Ok(reply(401, json!({"error":"unauthorized"})));
            }
            let input: Value = serde_json::from_slice(&body)?;
            s.calls
                .push((path.trim_start_matches('/').to_owned(), input.clone()));
            let ns = input["namespace"].as_str().unwrap_or("").to_owned();
            if !ns.starts_with("eb-") {
                return Ok(reply(403, json!({"error":"namespace_not_allowed"})));
            }
            if !self.provisioned.contains(&ns) {
                return Ok(reply(404, json!({"error":"namespace_not_provisioned"})));
            }
            let token_id = || uuid::Uuid::new_v4().simple().to_string()[..16].to_owned();
            let repo_view = |id: &str, name: &str, branch: &str, push: &Option<String>| json!({"id":id,"name":name,"description":null,"default_branch":branch,"remote":self.remote(&ns,name),"last_push_at":push});
            Ok(match path {
                "/repos.create" => {
                    let name = input["name"].as_str().unwrap().to_owned();
                    if s.repos.contains_key(&(ns.clone(), name.clone())) {
                        return Ok(reply(409, json!({"error":"already_exists"})));
                    }
                    let id = format!("repo_{}", s.repos.len() + 1);
                    s.repos.insert(
                        (ns.clone(), name.clone()),
                        (id.clone(), "main".into(), None),
                    );
                    reply(
                        200,
                        json!({"created":true,"repository":repo_view(&id,&name,"main",&None)}),
                    )
                }
                "/repos.get" => {
                    let name = input["name"].as_str().unwrap();
                    match s.repos.get(&(ns.clone(), name.to_owned())) {
                        Some((id, branch, push)) => {
                            reply(200, json!({"repository":repo_view(id,name,branch,push)}))
                        }
                        None => reply(404, json!({"error":"not_found"})),
                    }
                }
                "/repos.list" => {
                    let list: Vec<Value> = s
                        .repos
                        .iter()
                        .filter(|((n, _), _)| *n == ns)
                        .map(|((_, name), (id, branch, push))| {
                            let mut v = repo_view(id, name, branch, push);
                            v["remote"] = Value::Null;
                            v
                        })
                        .collect();
                    reply(200, json!({"repositories":list,"cursor":null}))
                }
                "/tokens.create" => {
                    let repo = input["repo"].as_str().unwrap().to_owned();
                    if !s.repos.contains_key(&(ns.clone(), repo.clone())) {
                        return Ok(reply(404, json!({"error":"not_found"})));
                    }
                    let ttl = input["ttl"].as_u64().unwrap();
                    if !(60..=90_000).contains(&ttl) {
                        return Ok(reply(400, json!({"error":"invalid_input"})));
                    }
                    let scope = input["scope"].as_str().unwrap().to_owned();
                    let tid = token_id();
                    let plaintext = format!(
                        "art_v1_{}?expires=1760000000",
                        tid.repeat(3)[..40].to_owned()
                    );
                    s.tokens.insert(
                        tid.clone(),
                        (ns.clone(), repo, scope.clone(), plaintext.clone(), false),
                    );
                    let expires =
                        (chrono::Utc::now() + chrono::Duration::seconds(ttl as i64)).to_rfc3339();
                    reply(
                        200,
                        json!({"token":{"id":tid,"plaintext":plaintext,"scope":scope,"expires_at":expires}}),
                    )
                }
                "/tokens.revoke" => {
                    let id = input["id"].as_str().unwrap();
                    let repo = input["repo"].as_str().unwrap();
                    match s.tokens.get_mut(id) {
                        Some(t) if t.0 == ns && t.1 == repo => {
                            t.4 = true;
                            reply(200, json!({"revoked":true}))
                        }
                        _ => reply(200, json!({"revoked":false})),
                    }
                }
                _ => reply(404, json!({"error":"not_found"})),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const ACCOUNT: &str = "0123456789abcdef0123456789abcdef";
    fn open(temp: &Path) -> (Repositories, Arc<mock::FakeGateway>) {
        let gateway = mock::FakeGateway::new(ACCOUNT, &["eb-alpha"]);
        let repos = Repositories::with_transport(temp, gateway.clone()).unwrap();
        (repos, gateway)
    }
    fn configure(repos: &Repositories) {
        repos
            .put_worker(&json!({"url":mock::URL,"secret":mock::SECRET,"namespace_prefix":"eb"}))
            .unwrap();
    }
    fn files_contain(root: &Path, needle: &str) -> bool {
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
                    return true;
                }
            }
        }
        false
    }

    /// The same vector is asserted by the gateway's tests
    /// (`packages/artifacts-gateway/test/gateway.test.mjs`).
    #[test]
    fn signatures_match_the_gateway_vector() {
        let body = br#"{"namespace":"eb-alpha","name":"game"}"#;
        let canonical = canonical("/repos.get", 1_800_000_000, &"0".repeat(32), body);
        assert_eq!(
            crate::callbacks::hmac_sha256_hex(mock::SECRET.as_bytes(), canonical.as_bytes()),
            "af382d7a1716eee3b28bea32a85f0daa8f803bcec2b0a0643f3c8df94e12dacb"
        );
    }
    #[test]
    fn worker_backend_config_is_validated_redacted_and_owner_private() {
        let temp = tempfile::tempdir().unwrap();
        let (repos, _) = open(temp.path());
        assert_eq!(repos.backend_view()["backend"]["configured"], false);
        assert_eq!(
            status_of(&repos.backend().err().unwrap()).0,
            409,
            "none configured"
        );
        for bad in [
            json!({"url":"http://gw.example.com","secret":mock::SECRET}),
            json!({"url":"https://gw.acct.workers.dev","secret":mock::SECRET}),
            json!({"url":"https://gw.example.com/a%2Fb","secret":mock::SECRET}),
            json!({"url":"https://gw.example.com/_artifacts?x=1","secret":mock::SECRET}),
            json!({"url":"https://user:pw@gw.example.com","secret":mock::SECRET}),
            json!({"url":"https://gw.example.com","secret":"short"}),
            json!({"url":"https://gw.example.com","secret":mock::SECRET,"namespace_prefix":"Bad_Prefix"}),
            json!({"url":"https://gw.example.com","secret":mock::SECRET,"api_token":"x"}),
        ] {
            assert_eq!(
                status_of(&repos.put_worker(&bad).unwrap_err()).0,
                400,
                "{bad}"
            );
        }
        let put = repos
            .put_worker(&json!({"url":"https://gw.example.com/_artifacts/","secret":mock::SECRET}))
            .unwrap();
        assert_eq!(put["backend"]["kind"], "artifacts_worker");
        assert_eq!(put["backend"]["url"], "https://gw.example.com/_artifacts");
        assert_eq!(put["backend"]["namespace_prefix"], "enginebox");
        assert_eq!(put["backend"]["secret"], "[redacted]");
        for view in [put, repos.get_worker(), repos.backend_view()] {
            assert!(!view.to_string().contains(mock::SECRET), "{view}");
        }
        assert!(!format!("{:?}", repos.inner.worker.lock().unwrap()).contains(mock::SECRET));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = temp.path().join("repositories/backends").join(WORKER_FILE);
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "owner-only file");
        }
        let (reopened, _) = open(temp.path());
        assert_eq!(
            reopened.backend_view()["backend"]["url"],
            "https://gw.example.com/_artifacts"
        );
        assert_eq!(reopened.delete_worker().unwrap()["deleted"], true);
        assert_eq!(reopened.get_worker()["backend"]["configured"], false);
        assert!(
            !temp
                .path()
                .join("repositories/backends")
                .join(WORKER_FILE)
                .exists()
        );
    }

    #[test]
    fn requests_are_signed_and_create_is_idempotent_without_returning_a_token() {
        let temp = tempfile::tempdir().unwrap();
        let (repos, gateway) = open(temp.path());
        assert_eq!(
            status_of(&repos.create("alpha", &json!({"name":"game"})).unwrap_err()).0,
            409,
            "no backend configured"
        );
        configure(&repos);
        let (created, first) = repos
            .create("alpha", &json!({"name":"game","description":"The game"}))
            .unwrap();
        assert!(created);
        let repository = &first["repository"];
        assert_eq!(repository["backend"], "artifacts");
        assert_eq!(
            repository["remote"],
            format!("https://{ACCOUNT}.artifacts.cloudflare.net/git/eb-alpha/game.git")
        );
        assert!(first.get("token").is_none() && repository.get("token").is_none());
        {
            let state = gateway.state.lock().unwrap();
            assert_eq!(
                state.rejected, 0,
                "the fake gateway verified every signature"
            );
            assert_eq!(
                state.urls[0], "https://site.example.test/_artifacts/repos.create",
                "the operation joins the base URL's path"
            );
            assert_eq!(
                state.calls[0],
                (
                    "repos.create".to_owned(),
                    json!({"namespace":"eb-alpha","name":"game","description":"The game"})
                )
            );
        }
        let (created, again) = repos.create("alpha", &json!({"name":"game"})).unwrap();
        assert!(!created);
        assert_eq!(again["repository"]["remote"], repository["remote"]);
        // Created outside this controller: adopted through repos.get.
        gateway.state.lock().unwrap().repos.insert(
            ("eb-alpha".into(), "tools".into()),
            (
                "repo_x".into(),
                "trunk".into(),
                Some("2026-10-01T00:00:00Z".into()),
            ),
        );
        let (created, adopted) = repos.create("alpha", &json!({"name":"tools"})).unwrap();
        assert!(!created);
        assert_eq!(adopted["repository"]["default_branch"], "trunk");
        // A project whose namespace the gateway does not serve is a clear 409.
        let (status, message) = status_of(&repos.create("beta", &json!({"name":"x"})).unwrap_err());
        assert_eq!(status, 409);
        assert!(message.contains("redeploy"), "{message}");
        for bad in [
            json!({"name":"Game"}),
            json!({"name":"x.git"}),
            json!({}),
            json!({"name":"ok","token":"x"}),
        ] {
            assert_eq!(
                status_of(&repos.create("alpha", &bad).unwrap_err()).0,
                400,
                "{bad}"
            );
        }
        let listed = repos.list("alpha").unwrap();
        let names: Vec<&str> = listed["repositories"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["game", "tools"]);
        assert_eq!(
            listed["repositories"][1]["last_push_at"],
            "2026-10-01T00:00:00Z"
        );
        assert_eq!(
            repos.get("alpha", "tools").unwrap()["repository"]["last_push_at"],
            "2026-10-01T00:00:00Z"
        );
        assert_eq!(
            status_of(&repos.get("alpha", "missing").unwrap_err()).0,
            404
        );
        let (reopened, _) = open(temp.path());
        assert!(reopened.exists("alpha", "game") && reopened.exists("alpha", "tools"));
        // The prefix cannot change under existing repositories; URL and secret can.
        let moved = repos
            .put_worker(&json!({"url":mock::URL,"secret":mock::SECRET,"namespace_prefix":"other"}));
        assert_eq!(status_of(&moved.unwrap_err()).0, 409);
        // A wrong secret is refused by the gateway: 502 without detail.
        repos
            .put_worker(&json!({"url":mock::URL,"secret":"a-different-secret-0123456789abcdef","namespace_prefix":"eb"}))
            .unwrap();
        let (status, message) =
            status_of(&repos.mint("alpha", "game", Access::Read, 600).unwrap_err());
        assert_eq!(status, 502);
        assert!(
            message.contains("secret") && !message.contains("0123456789abcdef"),
            "{message}"
        );
    }

    struct FakeGithub {
        selected: BTreeMap<String, String>,
        minted: Mutex<Vec<(String, bool)>>,
    }
    impl GithubLink for FakeGithub {
        fn selected_branch(&self, full_name: &str) -> Option<String> {
            self.selected.get(full_name).cloned()
        }
        fn token(&self, full_name: &str, write: bool) -> Result<String> {
            self.minted
                .lock()
                .unwrap()
                .push((full_name.to_owned(), write));
            Ok("ghs_fixture0123456789abcdefghijklmnop".to_owned())
        }
        fn open_pull_request(
            &self,
            full_name: &str,
            head: &str,
            _title: &str,
            _body: &str,
        ) -> Result<Value> {
            Ok(
                json!({"number":1,"url":format!("https://github.com/{full_name}/pull/1"),"head":head,"reused":false}),
            )
        }
    }

    #[test]
    fn linked_github_repositories_mint_single_repository_tokens_as_github_auth() {
        let temp = tempfile::tempdir().unwrap();
        let (repos, _gateway) = open(temp.path());
        // GitHub not configured: linking is refused with 409, nothing is written.
        assert_eq!(
            status_of(
                &repos
                    .link_github("alpha", &json!({"full_name":"null-shot/inference-market"}))
                    .unwrap_err()
            )
            .0,
            409
        );
        let github = Arc::new(FakeGithub {
            selected: BTreeMap::from([(
                "null-shot/inference-market".to_owned(),
                "main".to_owned(),
            )]),
            minted: Mutex::new(vec![]),
        });
        let repos = repos.with_github(github.clone());
        for bad in [
            json!({"full_name":"no-slash"}),
            json!({"full_name":"a/b.git"}),
            json!({"full_name":"../x"}),
            json!({"full_name":"null-shot/inference-market","name":"Bad Name"}),
            json!({"full_name":"null-shot/inference-market","token":"x"}),
        ] {
            assert_eq!(
                status_of(&repos.link_github("alpha", &bad).unwrap_err()).0,
                400,
                "{bad}"
            );
        }
        // Not selected in the GitHub panel: refused.
        assert_eq!(
            status_of(
                &repos
                    .link_github("alpha", &json!({"full_name":"null-shot/other"}))
                    .unwrap_err()
            )
            .0,
            409
        );
        let (created, linked) = repos
            .link_github("alpha", &json!({"full_name":"null-shot/inference-market"}))
            .unwrap();
        assert!(created);
        assert_eq!(linked["repository"]["name"], "inference-market");
        assert_eq!(linked["repository"]["backend"], "github");
        assert_eq!(
            linked["repository"]["remote"],
            "https://github.com/null-shot/inference-market.git"
        );
        assert_eq!(linked["repository"]["default_branch"], "main");
        // Linking again is idempotent; the GitHub record needs no Artifacts backend.
        assert!(
            !repos
                .link_github("alpha", &json!({"full_name":"null-shot/inference-market"}))
                .unwrap()
                .0
        );
        assert_eq!(
            repos.get("alpha", "inference-market").unwrap()["repository"]["backend"],
            "github"
        );
        let credential = repos
            .run_credential("alpha/inference-market", 7200)
            .unwrap();
        assert_eq!(credential.auth, repository::Auth::GithubToken);
        assert_eq!(
            credential.remote,
            "https://github.com/null-shot/inference-market.git"
        );
        assert!(credential.token_id.is_none());
        let expires = chrono::DateTime::parse_from_rfc3339(&credential.expires_at).unwrap();
        assert!(
            expires <= chrono::Utc::now() + chrono::Duration::seconds(GITHUB_TOKEN_SECONDS + 5)
        );
        let read = repos
            .mint("alpha", "inference-market", Access::Read, 600)
            .unwrap();
        assert_eq!(read["access"], "read");
        assert_eq!(
            github.minted.lock().unwrap().clone(),
            [
                ("null-shot/inference-market".to_owned(), true),
                ("null-shot/inference-market".to_owned(), false)
            ]
        );
        // The record survives a reopen (its validation accepts github records).
        let (reopened, _) = open(temp.path());
        assert_eq!(
            reopened.get("alpha", "inference-market").unwrap()["repository"]["backend"],
            "github"
        );
    }

    #[test]
    fn token_minting_is_bounded_and_runs_get_revocable_write_tokens() {
        let temp = tempfile::tempdir().unwrap();
        let (repos, gateway) = open(temp.path());
        configure(&repos);
        repos.create("alpha", &json!({"name":"game"})).unwrap();
        for bad in [
            json!({"access":"admin"}),
            json!({"access":"read","ttl_seconds":59}),
            json!({"access":"read","ttl_seconds":86401}),
            json!({"access":"read","ttl_seconds":-1}),
            json!({"access":"read","repo":"other"}),
        ] {
            assert!(Repositories::token_request(&bad).is_err(), "{bad}");
        }
        assert_eq!(
            Repositories::token_request(&json!({"access":"read"})).unwrap(),
            (Access::Read, DEFAULT_TOKEN_TTL_SECONDS)
        );
        let minted = repos.mint("alpha", "game", Access::Read, 600).unwrap();
        assert!(minted["token"].as_str().unwrap().starts_with("art_v1_"));
        assert_eq!(minted["access"], "read");
        assert!(
            chrono::DateTime::parse_from_rfc3339(minted["expires_at"].as_str().unwrap()).is_ok()
        );
        assert_eq!(
            gateway.state.lock().unwrap().calls.last().unwrap(),
            &(
                "tokens.create".to_owned(),
                json!({"namespace":"eb-alpha","repo":"game","scope":"read","ttl":600})
            )
        );
        assert_eq!(
            status_of(
                &repos
                    .mint("alpha", "missing", Access::Read, 600)
                    .unwrap_err()
            )
            .0,
            404
        );
        let credential = repos.run_credential("alpha/game", 720).unwrap();
        assert!(!format!("{credential:?}").contains(&credential.token));
        let id = credential.token_id.clone().unwrap();
        assert_eq!(gateway.state.lock().unwrap().tokens[&id].2, "write");
        repos.revoke_run_credential("alpha/game", &credential);
        assert!(
            gateway.state.lock().unwrap().tokens[&id].4,
            "revoked at run end"
        );
        assert_eq!(
            gateway.state.lock().unwrap().calls.last().unwrap(),
            &(
                "tokens.revoke".to_owned(),
                json!({"namespace":"eb-alpha","repo":"game","id":id})
            )
        );
        for token in gateway.issued() {
            assert!(!files_contain(temp.path(), &token), "no token persisted");
        }
        gateway.state.lock().unwrap().fail = true;
        assert_eq!(
            status_of(&repos.mint("alpha", "game", Access::Read, 600).unwrap_err()).0,
            502
        );
        assert_eq!(
            repos.list("alpha").unwrap()["repositories"][0]["last_push_at"],
            Value::Null
        );
    }
}
