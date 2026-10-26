//! Hosted Git repositories per project. Each repository lives in a backend;
//! today that is Cloudflare Artifacts (one Artifacts namespace per Engine Box
//! project, `<namespace_prefix>-<project>`). The [`Backend`] trait is the seam
//! for further backends (a GitHub App next).
//!
//! The backend's API token is owner-only private state: written once through
//! `PUT /v1/repository-backends/artifacts`, never returned or logged. Clients
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

/// The Cloudflare v4 API.
pub const CLOUDFLARE_API: &str = "https://api.cloudflare.com/client/v4";
pub const DEFAULT_NAMESPACE_PREFIX: &str = "enginebox";
const MAX_PER_PROJECT: usize = 1000;
const MAX_TOTAL: usize = 10_000;
const MAX_DESCRIPTION: usize = 512;
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
    fn revoke(&self, project: &str, token_id: &str) -> Result<()>;
}

/// One HTTP exchange with a backend API: (status, JSON body). Separate from
/// the backend so tests can stand in for the remote API.
pub trait Transport: Send + Sync {
    fn send(
        &self,
        method: &str,
        url: &str,
        bearer: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Value)>;
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
    fn send(
        &self,
        method: &str,
        url: &str,
        bearer: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Value)> {
        let method = reqwest::Method::from_bytes(method.as_bytes())?;
        let mut request = Self::client()?.request(method, url).bearer_auth(bearer);
        if let Some(body) = body {
            request = request.json(body);
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

/// Owner-supplied Artifacts configuration. The API token needs Artifacts
/// Read and Artifacts Edit; it is write-only.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactsConfig {
    pub account_id: String,
    pub namespace_prefix: String,
    api_token: String,
    pub updated_at: String,
}
impl std::fmt::Debug for ArtifactsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArtifactsConfig")
            .field("account_id", &self.account_id)
            .field("namespace_prefix", &self.namespace_prefix)
            .finish()
    }
}
impl ArtifactsConfig {
    pub fn new(account_id: &str, namespace_prefix: &str, api_token: &str) -> Result<Self> {
        let config = Self {
            account_id: account_id.into(),
            namespace_prefix: namespace_prefix.into(),
            api_token: api_token.into(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        };
        config.validate()?;
        Ok(config)
    }
    fn validate(&self) -> Result<()> {
        if !valid_account_id(&self.account_id) {
            return Err(invalid(
                "account_id must be a 32-character Cloudflare account ID.",
            ));
        }
        if !valid_namespace_prefix(&self.namespace_prefix) {
            return Err(invalid(
                "namespace_prefix must be 1-14 lowercase letters, digits or dashes, starting with a letter or digit.",
            ));
        }
        if !valid_api_token(&self.api_token) {
            return Err(invalid(
                "api_token must be 20-256 printable characters without spaces.",
            ));
        }
        Ok(())
    }
    /// Redacted view: never the API token.
    pub fn public(&self) -> Value {
        json!({
            "kind":"artifacts",
            "configured":true,
            "account_id":self.account_id,
            "namespace_prefix":self.namespace_prefix,
            "api_token":"[redacted]",
            "updated_at":self.updated_at,
        })
    }
}
pub fn valid_account_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
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
fn valid_api_token(token: &str) -> bool {
    (20..=256).contains(&token.len()) && token.bytes().all(|b| (33..=126).contains(&b))
}
/// The Artifacts namespace of one Engine Box project.
pub fn namespace(prefix: &str, project: &str) -> Result<String> {
    if !valid_namespace_prefix(prefix) || !pools::valid_project(project) {
        return Err(invalid("Invalid namespace."));
    }
    let ns = format!("{prefix}-{project}");
    // Artifacts: 2-63 characters, letters, digits, `.`, `_`, `-`.
    anyhow::ensure!((2..=63).contains(&ns.len()), "Invalid namespace.");
    Ok(ns)
}

/// Cloudflare Artifacts over its REST API.
pub struct ArtifactsBackend {
    config: ArtifactsConfig,
    api_base: String,
    transport: Arc<dyn Transport>,
}
impl ArtifactsBackend {
    pub fn new(config: ArtifactsConfig, api_base: &str, transport: Arc<dyn Transport>) -> Self {
        Self {
            config,
            api_base: api_base.trim_end_matches('/').to_owned(),
            transport,
        }
    }
    fn base(&self, project: &str) -> Result<String> {
        Ok(format!(
            "{}/accounts/{}/artifacts/namespaces/{}",
            self.api_base,
            self.config.account_id,
            namespace(&self.config.namespace_prefix, project)?
        ))
    }
    fn call(&self, method: &str, url: &str, body: Option<&Value>) -> Result<(u16, Value)> {
        self.transport
            .send(method, url, &self.config.api_token, body)
            .map_err(|_| upstream())
    }
    /// Log a backend refusal: status and Artifacts error codes only.
    fn log_refusal(operation: &str, status: u16, body: &Value) {
        let codes: Vec<Value> = body["errors"]
            .as_array()
            .map(|e| e.iter().map(|x| x["code"].clone()).collect())
            .unwrap_or_default();
        eprintln!(
            "{}",
            json!({"event":"enginebox.repositories.backend_refused","backend":"artifacts","operation":operation,"status":status,"codes":codes})
        );
    }
    fn has_code(body: &Value, code: u64) -> bool {
        body["errors"]
            .as_array()
            .is_some_and(|e| e.iter().any(|x| x["code"] == code))
    }
    fn repository(value: &Value) -> Result<BackendRepository> {
        let text = |key: &str| value[key].as_str().map(str::to_owned);
        let repository = BackendRepository {
            id: text("id"),
            name: text("name").context("Backend repository without a name")?,
            remote: text("remote").context("Backend repository without a remote")?,
            default_branch: text("default_branch").unwrap_or_else(|| "main".into()),
            last_push_at: text("last_push_at"),
        };
        if !repository::valid_remote(&repository.remote)
            || !repository::valid_branch(&repository.default_branch)
        {
            return Err(upstream());
        }
        Ok(repository)
    }
}
impl Backend for ArtifactsBackend {
    fn kind(&self) -> &'static str {
        "artifacts"
    }
    fn create(
        &self,
        project: &str,
        name: &str,
        description: Option<&str>,
    ) -> Result<(BackendRepository, bool)> {
        let mut body = json!({"name":name});
        if let Some(d) = description.filter(|d| !d.is_empty()) {
            body["description"] = json!(d);
        }
        let base = self.base(project)?;
        let (status, response) = self.call("POST", &format!("{base}/repos"), Some(&body))?;
        if (200..300).contains(&status) && response["success"] == true {
            // The creation response carries a write token. It is never stored
            // or returned; revoke it so it cannot outlive this request.
            let created = Self::repository(&response["result"])?;
            self.revoke_active(project, name);
            return Ok((created, true));
        }
        if status == 409 || Self::has_code(&response, 10201) {
            let existing = self.get(project, name)?.ok_or_else(upstream)?;
            return Ok((existing, false));
        }
        Self::log_refusal("create", status, &response);
        Err(upstream())
    }
    fn get(&self, project: &str, name: &str) -> Result<Option<BackendRepository>> {
        let base = self.base(project)?;
        let (status, response) = self.call("GET", &format!("{base}/repos/{name}"), None)?;
        if status == 404 {
            return Ok(None);
        }
        if !(200..300).contains(&status) || response["success"] != true {
            Self::log_refusal("get", status, &response);
            return Err(upstream());
        }
        Ok(Some(Self::repository(&response["result"])?))
    }
    fn list(&self, project: &str) -> Result<Vec<BackendRepository>> {
        let base = self.base(project)?;
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        // At most 10 pages of 200 (MAX_PER_PROJECT fits).
        for _ in 0..10 {
            let mut url = format!("{base}/repos?limit=200&sort=name&direction=asc");
            if let Some(c) = &cursor {
                url.push_str("&cursor=");
                url.push_str(
                    &url::form_urlencoded::byte_serialize(c.as_bytes()).collect::<String>(),
                );
            }
            let (status, response) = self.call("GET", &url, None)?;
            if status == 404 {
                break;
            }
            if !(200..300).contains(&status) || response["success"] != true {
                Self::log_refusal("list", status, &response);
                return Err(upstream());
            }
            let page = response["result"].as_array().cloned().unwrap_or_default();
            for item in &page {
                if let Ok(r) = Self::repository(item) {
                    out.push(r);
                }
            }
            cursor = response["result_info"]["cursor"]
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
        let base = self.base(project)?;
        let body = json!({"repo":name,"scope":access.as_str(),"ttl":ttl});
        let (status, response) = self.call("POST", &format!("{base}/tokens"), Some(&body))?;
        if !(200..300).contains(&status) || response["success"] != true {
            Self::log_refusal("mint", status, &response);
            return Err(upstream());
        }
        let result = &response["result"];
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
    fn revoke(&self, project: &str, token_id: &str) -> Result<()> {
        if token_id.is_empty()
            || token_id.len() > 128
            || !token_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(invalid("Invalid token ID."));
        }
        let base = self.base(project)?;
        let (status, response) = self.call("DELETE", &format!("{base}/tokens/{token_id}"), None)?;
        if (200..300).contains(&status) || status == 404 {
            return Ok(());
        }
        Self::log_refusal("revoke", status, &response);
        Err(upstream())
    }
}
impl ArtifactsBackend {
    /// Best effort: revoke every active token of a repository just created.
    fn revoke_active(&self, project: &str, name: &str) {
        let Ok(base) = self.base(project) else {
            return;
        };
        let Ok((status, response)) = self.call(
            "GET",
            &format!("{base}/repos/{name}/tokens?state=active&per_page=100"),
            None,
        ) else {
            return;
        };
        if !(200..300).contains(&status) {
            return;
        }
        for token in response["result"].as_array().into_iter().flatten() {
            if let Some(id) = token["id"].as_str() {
                let _ = self.revoke(project, id);
            }
        }
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
}
impl Record {
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            pools::valid_project(&self.project)
                && repository::valid_name(&self.name)
                && repository::valid_remote(&self.remote)
                && repository::valid_branch(&self.default_branch)
                && self.backend == "artifacts"
                && self.description.len() <= MAX_DESCRIPTION,
            "Invalid repository record"
        );
        Ok(())
    }
    pub fn public(&self, last_push_at: Option<&str>) -> Value {
        json!({
            "project":self.project,
            "name":self.name,
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
    artifacts: Mutex<Option<ArtifactsConfig>>,
    /// Serializes creation so one name creates one repository.
    creating: Mutex<()>,
    api_base: String,
    transport: Arc<dyn Transport>,
}
impl Repositories {
    pub fn open(root: &Path) -> Result<Self> {
        Self::with_transport(root, CLOUDFLARE_API, Arc::new(HttpTransport))
    }
    /// Open with another API base and transport (tests stand in for the API).
    pub fn with_transport(
        root: &Path,
        api_base: &str,
        transport: Arc<dyn Transport>,
    ) -> Result<Self> {
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
        let artifacts_path = backends_root.join("artifacts.json");
        let artifacts = if artifacts_path.exists() {
            let config: ArtifactsConfig =
                serde_json::from_value(state::read_json(&artifacts_path)?)?;
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
                artifacts: Mutex::new(artifacts),
                creating: Mutex::new(()),
                api_base: api_base.to_owned(),
                transport,
            }),
        })
    }

    // ------------------------------------------------------------ backend config

    /// `PUT /v1/repository-backends/artifacts`.
    pub fn put_artifacts(&self, body: &Value) -> Result<Value> {
        let object = body
            .as_object()
            .ok_or_else(|| invalid("Expected an object."))?;
        if !object
            .keys()
            .all(|k| ["account_id", "namespace_prefix", "api_token"].contains(&k.as_str()))
        {
            return Err(invalid(
                "Accepted fields: account_id, namespace_prefix, api_token.",
            ));
        }
        let text = |key: &str| body[key].as_str();
        let account_id = text("account_id").ok_or_else(|| invalid("account_id is required."))?;
        let api_token = text("api_token").ok_or_else(|| invalid("api_token is required."))?;
        let prefix = match body.get("namespace_prefix") {
            None | Some(Value::Null) => DEFAULT_NAMESPACE_PREFIX,
            Some(Value::String(p)) => p.as_str(),
            Some(_) => return Err(invalid("namespace_prefix must be a string.")),
        };
        let config = ArtifactsConfig::new(account_id, prefix, api_token)?;
        let mut current = self.inner.artifacts.lock().unwrap();
        let has_records = !self.inner.records.lock().unwrap().is_empty();
        if has_records
            && current.as_ref().is_some_and(|c| {
                c.account_id != config.account_id || c.namespace_prefix != config.namespace_prefix
            })
        {
            return Err(refuse(
                409,
                "Repositories exist under the current account and namespace prefix; only the API token can be replaced.",
            ));
        }
        state::write_json(
            &self.inner.backends_root.join("artifacts.json"),
            &serde_json::to_value(&config)?,
        )?;
        let public = config.public();
        *current = Some(config);
        Ok(json!({"backend":public}))
    }
    /// `GET /v1/repository-backends/artifacts` (redacted).
    pub fn get_artifacts(&self) -> Value {
        match &*self.inner.artifacts.lock().unwrap() {
            Some(config) => json!({"backend":config.public()}),
            None => json!({"backend":{"kind":"artifacts","configured":false}}),
        }
    }
    /// `DELETE /v1/repository-backends/artifacts`. Repository records stay;
    /// operations that need the backend answer 409 until it is configured.
    pub fn delete_artifacts(&self) -> Result<Value> {
        let mut current = self.inner.artifacts.lock().unwrap();
        let path = self.inner.backends_root.join("artifacts.json");
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let deleted = current.take().is_some();
        Ok(json!({"deleted":deleted,"backend":{"kind":"artifacts","configured":false}}))
    }
    pub fn backend(&self) -> Result<Box<dyn Backend>> {
        let config = self
            .inner
            .artifacts
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(not_configured)?;
        Ok(Box::new(ArtifactsBackend::new(
            config,
            &self.inner.api_base,
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
    /// `POST /v1/projects/:project/repositories`: create, or return the
    /// existing repository of that name (`false`).
    pub fn create(&self, project: &str, body: &Value) -> Result<(bool, Value)> {
        Self::check_project(project)?;
        let object = body
            .as_object()
            .ok_or_else(|| invalid("Expected an object."))?;
        if !object
            .keys()
            .all(|k| ["name", "description"].contains(&k.as_str()))
        {
            return Err(invalid("Accepted fields: name, description."));
        }
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
            backend: backend.kind().to_owned(),
            description,
            remote: created.remote,
            default_branch: created.default_branch,
            created_at: chrono::Utc::now().to_rfc3339(),
            backend_id: created.id,
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
        let last_push = self
            .backend()
            .and_then(|b| b.get(project, name))
            .ok()
            .flatten()
            .and_then(|r| r.last_push_at);
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
        let minted = self.backend()?.mint(project, name, access, ttl)?;
        Ok(json!({
            "token":minted.token,
            "expires_at":minted.expires_at,
            "remote":record.remote,
            "default_branch":record.default_branch,
            "access":access.as_str(),
        }))
    }
    /// A run's write token (`<project>/<name>`), minted at admission.
    pub fn run_credential(&self, reference: &str, ttl: u64) -> Result<repository::Credential> {
        let (project, name) = repository::parse_ref(reference)?;
        let record = self.record(&project, &name)?;
        let minted = self.backend()?.mint(&project, &name, Access::Write, ttl)?;
        Ok(repository::Credential {
            remote: record.remote,
            default_branch: record.default_branch,
            token: minted.token,
            token_id: minted.id,
            expires_at: minted.expires_at,
        })
    }
    /// Revoke a run's token when the run ends. Failures are logged without
    /// the token; the token still expires on its own.
    pub fn revoke_run_credential(&self, reference: &str, credential: &repository::Credential) {
        let Some(id) = &credential.token_id else {
            return;
        };
        let outcome = repository::parse_ref(reference)
            .and_then(|(project, _)| self.backend()?.revoke(&project, id));
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
    //! A stand-in for the Artifacts REST API: the documented routes, the v4
    //! envelope and the error codes, held in memory.
    use super::*;
    #[derive(Default)]
    pub struct State {
        /// (namespace, name) -> (id, default_branch, last_push_at)
        pub repos: BTreeMap<(String, String), (String, String, Option<String>)>,
        /// token id -> (namespace, repo, scope, plaintext, revoked)
        pub tokens: BTreeMap<String, (String, String, String, String, bool)>,
        pub calls: Vec<(String, String, String, Option<Value>)>,
        pub fail: bool,
    }
    pub struct MockArtifacts {
        pub account: String,
        pub state: Mutex<State>,
    }
    impl MockArtifacts {
        pub fn new(account: &str) -> Arc<Self> {
            Arc::new(Self {
                account: account.into(),
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
    fn ok(result: Value) -> (u16, Value) {
        (
            200,
            json!({"result":result,"success":true,"errors":[],"messages":[]}),
        )
    }
    fn err(status: u16, code: u64, message: &str) -> (u16, Value) {
        (
            status,
            json!({"result":null,"success":false,"errors":[{"code":code,"message":message}],"messages":[]}),
        )
    }
    impl Transport for MockArtifacts {
        fn send(
            &self,
            method: &str,
            url: &str,
            bearer: &str,
            body: Option<&Value>,
        ) -> Result<(u16, Value)> {
            let mut s = self.state.lock().unwrap();
            s.calls
                .push((method.into(), url.into(), bearer.into(), body.cloned()));
            if s.fail {
                anyhow::bail!("connection refused");
            }
            let prefix = format!(
                "https://api.example.test/client/v4/accounts/{}/artifacts/namespaces/",
                self.account
            );
            let rest = url.strip_prefix(&prefix).context("unexpected URL")?;
            let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
            let parts: Vec<&str> = path.split('/').collect();
            let ns = parts[0].to_owned();
            let token_id = || uuid::Uuid::new_v4().simple().to_string()[..16].to_owned();
            Ok(match (method, &parts[1..]) {
                ("POST", ["repos"]) => {
                    let name = body.unwrap()["name"].as_str().unwrap().to_owned();
                    if s.repos.contains_key(&(ns.clone(), name.clone())) {
                        return Ok(err(409, 10201, "The target repository already exists"));
                    }
                    let id = format!("repo_{}", s.repos.len() + 1);
                    s.repos.insert(
                        (ns.clone(), name.clone()),
                        (id.clone(), "main".into(), None),
                    );
                    let tid = token_id();
                    let plaintext = format!("art_v1_{}?expires=1760000000", "c".repeat(40));
                    s.tokens.insert(
                        tid,
                        (
                            ns.clone(),
                            name.clone(),
                            "write".into(),
                            plaintext.clone(),
                            false,
                        ),
                    );
                    ok(
                        json!({"id":id,"name":name,"description":body.unwrap()["description"],"default_branch":"main","remote":self.remote(&ns,&name),"token":plaintext}),
                    )
                }
                ("GET", ["repos"]) => {
                    assert!(query.contains("limit=200"));
                    let list: Vec<Value> = s
                        .repos
                        .iter()
                        .filter(|((n, _), _)| *n == ns)
                        .map(|((_, name), (id, branch, push))| json!({"id":id,"name":name,"default_branch":branch,"last_push_at":push,"remote":self.remote(&ns,name)}))
                        .collect();
                    let mut response = ok(json!(list));
                    response.1["result_info"] =
                        json!({"cursor":"","per_page":200,"count":list.len()});
                    response
                }
                ("GET", ["repos", name]) => match s.repos.get(&(ns.clone(), name.to_string())) {
                    Some((id, branch, push)) => ok(
                        json!({"id":id,"name":name,"default_branch":branch,"last_push_at":push,"remote":self.remote(&ns,name)}),
                    ),
                    None => err(404, 10200, "Not found"),
                },
                ("GET", ["repos", name, "tokens"]) => {
                    assert!(query.contains("state=active"));
                    let list: Vec<Value> = s
                        .tokens
                        .iter()
                        .filter(|(_, t)| t.0 == ns && t.1 == *name && !t.4)
                        .map(|(id, t)| json!({"id":id,"scope":t.2,"state":"active"}))
                        .collect();
                    ok(json!(list))
                }
                ("POST", ["tokens"]) => {
                    let body = body.unwrap();
                    let repo = body["repo"].as_str().unwrap().to_owned();
                    if !s.repos.contains_key(&(ns.clone(), repo.clone())) {
                        return Ok(err(404, 10200, "Not found"));
                    }
                    let ttl = body["ttl"].as_u64().unwrap();
                    if !(60..=31_536_000).contains(&ttl) {
                        return Ok(err(400, 10103, "Invalid TTL"));
                    }
                    let scope = body["scope"].as_str().unwrap().to_owned();
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
                    ok(json!({"id":tid,"plaintext":plaintext,"scope":scope,"expires_at":expires}))
                }
                ("DELETE", ["tokens", id]) => match s.tokens.get_mut(*id) {
                    Some(t) if t.0 == ns => {
                        t.4 = true;
                        ok(json!({"id":id}))
                    }
                    _ => err(404, 10200, "Not found"),
                },
                _ => err(404, 10200, "Unknown route"),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const ACCOUNT: &str = "0123456789abcdef0123456789abcdef";
    const API_TOKEN: &str = "cf-test-api-token-0123456789abcdef";
    fn open(temp: &Path) -> (Repositories, Arc<mock::MockArtifacts>) {
        let api = mock::MockArtifacts::new(ACCOUNT);
        let repos =
            Repositories::with_transport(temp, "https://api.example.test/client/v4", api.clone())
                .unwrap();
        (repos, api)
    }
    fn configure(repos: &Repositories) {
        repos
            .put_artifacts(
                &json!({"account_id":ACCOUNT,"namespace_prefix":"eb","api_token":API_TOKEN}),
            )
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

    #[test]
    fn backend_config_is_validated_redacted_and_owner_private() {
        let temp = tempfile::tempdir().unwrap();
        let (repos, _) = open(temp.path());
        assert_eq!(repos.get_artifacts()["backend"]["configured"], false);
        for bad in [
            json!({"account_id":"nope","api_token":API_TOKEN}),
            json!({"account_id":ACCOUNT,"api_token":"short"}),
            json!({"account_id":ACCOUNT,"api_token":API_TOKEN,"namespace_prefix":"Bad_Prefix"}),
            json!({"account_id":ACCOUNT,"api_token":API_TOKEN,"namespace_prefix":"fifteen-chars-x"}),
            json!({"account_id":ACCOUNT,"api_token":API_TOKEN,"extra":1}),
        ] {
            assert_eq!(
                status_of(&repos.put_artifacts(&bad).unwrap_err()).0,
                400,
                "{bad}"
            );
        }
        let put = repos
            .put_artifacts(&json!({"account_id":ACCOUNT,"api_token":API_TOKEN}))
            .unwrap();
        assert_eq!(put["backend"]["namespace_prefix"], "enginebox");
        assert_eq!(put["backend"]["api_token"], "[redacted]");
        assert!(!put.to_string().contains(API_TOKEN));
        let got = repos.get_artifacts();
        assert_eq!(got["backend"]["configured"], true);
        assert!(!got.to_string().contains(API_TOKEN));
        assert!(!format!("{:?}", repos.inner.artifacts.lock().unwrap()).contains(API_TOKEN));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = temp.path().join("repositories/backends/artifacts.json");
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "owner-only file");
        }
        // Reopened (a controller restart): still configured, still redacted.
        let (reopened, _) = open(temp.path());
        assert_eq!(reopened.get_artifacts()["backend"]["account_id"], ACCOUNT);
        let deleted = reopened.delete_artifacts().unwrap();
        assert_eq!(deleted["deleted"], true);
        assert_eq!(reopened.get_artifacts()["backend"]["configured"], false);
        assert!(
            !temp
                .path()
                .join("repositories/backends/artifacts.json")
                .exists()
        );
    }

    #[test]
    fn create_is_idempotent_on_name_and_never_returns_or_keeps_a_token() {
        let temp = tempfile::tempdir().unwrap();
        let (repos, api) = open(temp.path());
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
        assert_eq!(repository["project"], "alpha");
        assert_eq!(repository["name"], "game");
        assert_eq!(repository["backend"], "artifacts");
        assert_eq!(repository["default_branch"], "main");
        assert_eq!(
            repository["remote"],
            format!("https://{ACCOUNT}.artifacts.cloudflare.net/git/eb-alpha/game.git")
        );
        assert!(repository["created_at"].is_string());
        assert!(first.get("token").is_none() && repository.get("token").is_none());
        // The documented request: POST .../namespaces/eb-alpha/repos with the API token.
        {
            let state = api.state.lock().unwrap();
            let (method, url, bearer, body) = &state.calls[0];
            assert_eq!(method, "POST");
            assert_eq!(
                url,
                &format!(
                    "https://api.example.test/client/v4/accounts/{ACCOUNT}/artifacts/namespaces/eb-alpha/repos"
                )
            );
            assert_eq!(bearer, API_TOKEN);
            assert_eq!(body.as_ref().unwrap()["name"], "game");
            // The creation token was revoked straight away.
            assert!(state.tokens.values().all(|t| t.4));
        }
        let (created, again) = repos.create("alpha", &json!({"name":"game"})).unwrap();
        assert!(!created);
        assert_eq!(again["repository"]["remote"], repository["remote"]);
        // A repository created outside this controller is adopted, not duplicated.
        api.state.lock().unwrap().repos.insert(
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
        for bad in [
            json!({"name":"Game"}),
            json!({"name":"x.git"}),
            json!({"name":"ok","token":"x"}),
            json!({}),
        ] {
            assert_eq!(
                status_of(&repos.create("alpha", &bad).unwrap_err()).0,
                400,
                "{bad}"
            );
        }
        assert_eq!(
            status_of(
                &repos
                    .create("Not_A_Project", &json!({"name":"x"}))
                    .unwrap_err()
            )
            .0,
            400
        );
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
        assert_eq!(repos.list("beta").unwrap()["repositories"], json!([]));
        assert_eq!(
            repos.get("alpha", "tools").unwrap()["repository"]["last_push_at"],
            "2026-10-01T00:00:00Z"
        );
        assert_eq!(
            status_of(&repos.get("alpha", "missing").unwrap_err()).0,
            404
        );
        // Records persist across a restart, and no record holds a token.
        let (reopened, _) = open(temp.path());
        assert!(reopened.exists("alpha", "game") && reopened.exists("alpha", "tools"));
        for token in api.issued() {
            assert!(!files_contain(
                &temp.path().join("repositories/records"),
                &token
            ));
        }
        // The backend's account cannot change under existing repositories.
        let moved =
            repos.put_artifacts(&json!({"account_id":"f".repeat(32),"api_token":API_TOKEN}));
        assert_eq!(status_of(&moved.unwrap_err()).0, 409);
        assert!(repos.put_artifacts(&json!({"account_id":ACCOUNT,"namespace_prefix":"eb","api_token":"rotated-token-0123456789abcdef"})).is_ok());
    }

    #[test]
    fn token_minting_is_bounded_and_runs_get_revocable_write_tokens() {
        let temp = tempfile::tempdir().unwrap();
        let (repos, api) = open(temp.path());
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
        let (access, ttl) =
            Repositories::token_request(&json!({"access":"read","ttl_seconds":600})).unwrap();
        let minted = repos.mint("alpha", "game", access, ttl).unwrap();
        assert!(minted["token"].as_str().unwrap().starts_with("art_v1_"));
        assert_eq!(minted["access"], "read");
        assert_eq!(
            minted["remote"],
            format!("https://{ACCOUNT}.artifacts.cloudflare.net/git/eb-alpha/game.git")
        );
        assert!(
            chrono::DateTime::parse_from_rfc3339(minted["expires_at"].as_str().unwrap()).is_ok()
        );
        {
            let state = api.state.lock().unwrap();
            let (_, url, _, body) = state.calls.last().unwrap();
            assert!(url.ends_with("/namespaces/eb-alpha/tokens"));
            assert_eq!(
                body.as_ref().unwrap(),
                &json!({"repo":"game","scope":"read","ttl":600})
            );
        }
        assert_eq!(
            status_of(
                &repos
                    .mint("alpha", "missing", Access::Read, 600)
                    .unwrap_err()
            )
            .0,
            404
        );
        // A run's token: write scope, the run's TTL, revoked at the end.
        let credential = repos.run_credential("alpha/game", 720).unwrap();
        assert!(!format!("{credential:?}").contains(&credential.token));
        let id = credential.token_id.clone().unwrap();
        {
            let state = api.state.lock().unwrap();
            assert_eq!(state.tokens[&id].2, "write");
            let (_, _, _, body) = state.calls.last().unwrap();
            assert_eq!(body.as_ref().unwrap()["ttl"], 720);
        }
        repos.revoke_run_credential("alpha/game", &credential);
        assert!(
            api.state.lock().unwrap().tokens[&id].4,
            "revoked at run end"
        );
        assert!(repos.run_credential("alpha/missing", 720).is_err());
        // An unreachable backend is a 502 without detail.
        api.state.lock().unwrap().fail = true;
        let (status, message) =
            status_of(&repos.mint("alpha", "game", Access::Read, 600).unwrap_err());
        assert_eq!(status, 502);
        assert!(!message.contains(API_TOKEN));
        // Listing still answers from the records.
        assert_eq!(
            repos.list("alpha").unwrap()["repositories"][0]["last_push_at"],
            Value::Null
        );
    }
}
