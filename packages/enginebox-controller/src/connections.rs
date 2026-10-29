//! The org connections catalog (docs/CONNECTIONS.md).
//!
//! A connection is a named, reusable record that profiles reference by name:
//!
//! - `mcp_http`: a streamable HTTP MCP server; header values come from secrets
//!   or from the run's own env.
//! - `mcp_stdio`: an MCP server started inside the workspace from a binary the
//!   template installs (absolute path under an allowlisted prefix); its env
//!   comes from secrets or the run's env.
//! - `secret_source`: yields named values for a project and environment
//!   (`stored`: values written to this API; `kitbash`: values Kitbash delivers
//!   to the controller).
//! - `agent_account`: reserved for the credential-pool work; refused today.
//!
//! Secret values are write-only: they are stored in a separate 0600 file per
//! connection (`connections/secrets/<id>.json`), are never returned by any
//! API, MCP tool or UI, never logged, and never appear in an error message.
//! Reads show `secret_names`. Records carry who created and last changed them
//! and when a run last used them.
//!
//! Connection names are unique per org. Profiles resolve their connections in
//! their own org at launch ([`Connections::apply`]), so editing a connection
//! changes every profile that references it from the next run on.
use anyhow::{Context, Result, bail, ensure};
use enginebox_agents::{
    native, pools,
    profile::{self, McpServer, ProviderProfile},
};
use enginebox_core::state;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub const MAX_CONNECTIONS: usize = 1024;
const MAX_SECRETS: usize = 64;
const MAX_SECRET_BYTES: usize = 16 * 1024;
const MAX_HEADERS: usize = 16;
const MAX_ARGS: usize = 32;
/// `last_used_at` is written at most this often per connection.
const USE_WRITE_INTERVAL_SECONDS: i64 = 60;
/// Where a `mcp_stdio` command may live: binaries a template installs. A
/// connection never names a package runner that downloads code at run time.
pub const STDIO_PREFIXES: &[&str] = &["/usr/bin/", "/usr/local/bin/", "/opt/"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    McpHttp,
    McpStdio,
    SecretSource,
    /// Reserved for credential pools (docs/ORGS.md, "Agent credentials").
    AgentAccount,
}

/// Where one header or variable value comes from. Exactly one form:
///
/// - `{"env": "VAR"}`: the run's own env supplies it at start;
/// - `{"secret": "NAME"}`: this connection's write-only stored value;
/// - `{"secret_source": "<connection>", "name": "NAME"}`: a value another
///   `secret_source` connection of the same org yields.
///
/// `prefix` (for example `"Bearer "`) is prepended to a secret value.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValueRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceProvider {
    /// Values written once to this API (write-only).
    Stored,
    /// Values Kitbash delivers to the controller (docs/CONNECTIONS.md, "Kitbash").
    Kitbash,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub provider: SourceProvider,
    /// Kitbash: the enrolled delivery (integration) directory name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration: Option<String>,
    /// The project and environment the values belong to (Kitbash checks the
    /// delivery's own project and target against them).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    pub id: String,
    pub org_id: String,
    pub name: String,
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, ValueRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, ValueRef>,
    /// `mcp_stdio`: the template that installs `command`; a profile using the
    /// connection must name the same template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    /// `secret_source`: the names it yields (and puts in a run's env). For
    /// `stored` these are the stored names; for Kitbash the delivered ones.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<String>,
    pub revision: u64,
    pub created_by: String,
    pub created_at: String,
    pub updated_by: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
}

/// Fields a create or update request may set. `secrets` carries write-only
/// values; `remove_secrets` drops stored names.
const BODY_FIELDS: &[&str] = &[
    "name",
    "kind",
    "description",
    "url",
    "headers",
    "command",
    "args",
    "env",
    "template",
    "source",
    "names",
    "secrets",
    "remove_secrets",
];
/// The declarative fields (catalog files and comparisons): no values.
pub const SPEC_FIELDS: &[&str] = &[
    "name",
    "kind",
    "description",
    "url",
    "headers",
    "command",
    "args",
    "env",
    "template",
    "source",
    "names",
];

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}
fn valid_header(name: &str) -> bool {
    (1..=64).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}
fn valid_segment(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        && !value.starts_with('.')
}
/// A write-only value: 1-16 KiB, no NUL. The message never includes it.
fn valid_secret_value(value: &str) -> bool {
    (1..=MAX_SECRET_BYTES).contains(&value.len()) && !value.contains('\0')
}
pub fn valid_stdio_command(command: &str) -> bool {
    command.len() <= 256
        && STDIO_PREFIXES.iter().any(|p| command.starts_with(p))
        && command
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._+-".contains(&b))
        && !command.split('/').any(|c| c == ".." || c == ".")
        && !command.ends_with('/')
}

impl ValueRef {
    fn validate(&self) -> Result<()> {
        let forms = [
            self.env.is_some(),
            self.secret.is_some(),
            self.secret_source.is_some(),
        ];
        ensure!(
            forms.iter().filter(|f| **f).count() == 1,
            "Each value reference needs exactly one of env, secret or secret_source."
        );
        if let Some(var) = &self.env {
            ensure!(
                native::env_key_allowed(var),
                "env references must name a variable a run may set."
            );
            ensure!(
                self.prefix.is_none() && self.name.is_none(),
                "env references take no prefix or name; the run supplies the whole value."
            );
        }
        if let Some(secret) = &self.secret {
            ensure!(
                self.name.is_none(),
                "secret references take no name; use secret_source with name for another connection."
            );
            ensure!(
                native::env_key_allowed(secret),
                "Secret names must match ^[A-Z][A-Z0-9_]{{0,63}}$ and be names a run may set."
            );
        }
        if let Some(source) = &self.secret_source {
            ensure!(
                profile::valid_name(source),
                "secret_source must name a connection."
            );
            ensure!(
                self.name.as_deref().is_some_and(native::env_key_allowed),
                "secret_source references need the name of the value."
            );
        }
        if let Some(prefix) = &self.prefix {
            ensure!(
                prefix.len() <= 32 && prefix.bytes().all(|b| (32..=126).contains(&b)),
                "prefix must be at most 32 printable characters."
            );
        }
        Ok(())
    }
}

impl Connection {
    /// Validate the record. `stored` are the names of its stored secrets.
    pub fn validate(&self, stored: &BTreeSet<String>) -> Result<()> {
        ensure!(
            profile::valid_name(&self.name),
            "Connection names must match ^[a-z][a-z0-9-]{{0,62}}$."
        );
        ensure!(
            !self.name.starts_with("enginebox-"),
            "Connection names starting with enginebox- are reserved."
        );
        ensure!(pools::valid_org(&self.org_id), "Invalid org.");
        ensure!(
            self.description.len() <= 512 && !self.description.chars().any(char::is_control),
            "description must be at most 512 printable bytes."
        );
        ensure!(
            stored.len() <= MAX_SECRETS,
            "A connection stores at most {MAX_SECRETS} secrets."
        );
        let none = |present: bool, field: &str| -> Result<()> {
            ensure!(
                !present,
                "{field} does not apply to this kind of connection."
            );
            Ok(())
        };
        match self.kind {
            Kind::AgentAccount => bail!(
                "agent_account connections are reserved for credential pools and cannot be created yet."
            ),
            Kind::McpHttp => {
                let url = self
                    .url
                    .as_deref()
                    .context("mcp_http connections need a url.")?;
                ensure!(
                    profile::valid_mcp_url(url),
                    "url must be https, or http on localhost, without credentials or fragment."
                );
                ensure!(
                    self.headers.len() <= MAX_HEADERS,
                    "headers accepts at most {MAX_HEADERS} entries."
                );
                for (header, value) in &self.headers {
                    ensure!(
                        valid_header(header),
                        "Header names must match ^[A-Za-z0-9-]{{1,64}}$."
                    );
                    value.validate()?;
                }
                none(self.command.is_some(), "command")?;
                none(!self.args.is_empty(), "args")?;
                none(!self.env.is_empty(), "env")?;
                none(self.template.is_some(), "template")?;
                none(self.source.is_some(), "source")?;
                none(!self.names.is_empty(), "names")?;
            }
            Kind::McpStdio => {
                let command = self
                    .command
                    .as_deref()
                    .context("mcp_stdio connections need a command.")?;
                ensure!(
                    valid_stdio_command(command),
                    "command must be an absolute path to a binary the template installs, under {}.",
                    STDIO_PREFIXES.join(", ")
                );
                ensure!(
                    self.args.len() <= MAX_ARGS
                        && self
                            .args
                            .iter()
                            .all(|a| a.len() <= 1024 && !a.contains('\0')),
                    "args accepts at most {MAX_ARGS} values of 1024 bytes without NUL."
                );
                ensure!(
                    self.env.len() <= MAX_ARGS,
                    "env accepts at most {MAX_ARGS} variables."
                );
                for (var, value) in &self.env {
                    ensure!(
                        native::env_key_allowed(var),
                        "env names must be variables a run may set."
                    );
                    value.validate()?;
                }
                if let Some(template) = &self.template {
                    enginebox_core::identifier(template, false)?;
                }
                none(self.url.is_some(), "url")?;
                none(!self.headers.is_empty(), "headers")?;
                none(self.source.is_some(), "source")?;
                none(!self.names.is_empty(), "names")?;
            }
            Kind::SecretSource => {
                let source = self
                    .source
                    .as_ref()
                    .context("secret_source connections need a source.")?;
                for value in [&source.integration, &source.project, &source.environment]
                    .into_iter()
                    .flatten()
                {
                    ensure!(
                        valid_segment(value),
                        "integration, project and environment must be 1-64 characters of A-Z a-z 0-9 . _ -."
                    );
                }
                match source.provider {
                    SourceProvider::Stored => {
                        ensure!(
                            source.integration.is_none(),
                            "Only Kitbash sources name an integration."
                        );
                        ensure!(
                            self.names.is_empty(),
                            "A stored source yields exactly its stored secrets; do not list names."
                        );
                    }
                    SourceProvider::Kitbash => {
                        ensure!(
                            source.integration.is_some(),
                            "Kitbash sources need the delivery's integration name."
                        );
                        ensure!(
                            stored.is_empty(),
                            "Kitbash sources take their values from the delivery, not from secrets."
                        );
                        ensure!(
                            !self.names.is_empty() && self.names.len() <= MAX_SECRETS,
                            "Kitbash sources list 1-{MAX_SECRETS} names they yield."
                        );
                    }
                }
                let mut seen = BTreeSet::new();
                for name in &self.names {
                    ensure!(
                        native::env_key_allowed(name) && seen.insert(name),
                        "names must be distinct variables a run may set."
                    );
                }
                none(self.url.is_some(), "url")?;
                none(!self.headers.is_empty(), "headers")?;
                none(self.command.is_some(), "command")?;
                none(!self.args.is_empty(), "args")?;
                none(!self.env.is_empty(), "env")?;
                none(self.template.is_some(), "template")?;
            }
        }
        // Stored secrets must be referenced by this connection, or yielded by it.
        if self.kind != Kind::SecretSource {
            let referenced: BTreeSet<_> = self
                .headers
                .values()
                .chain(self.env.values())
                .filter_map(|v| v.secret.clone())
                .collect();
            for name in stored {
                ensure!(
                    native::env_key_allowed(name),
                    "Secret names must match ^[A-Z][A-Z0-9_]{{0,63}}$ and be names a run may set."
                );
                ensure!(
                    referenced.contains(name),
                    "Every stored secret must be referenced by a header or env entry."
                );
            }
        }
        Ok(())
    }
    /// The declarative part of the record (what a catalog file states).
    pub fn spec(&self) -> Value {
        let mut value = serde_json::to_value(self).unwrap_or(Value::Null);
        if let Some(map) = value.as_object_mut() {
            map.retain(|k, _| SPEC_FIELDS.contains(&k.as_str()));
        }
        value
    }
    /// Public view: never a secret value.
    pub fn public(&self, secret_names: &BTreeSet<String>, used_by: &[String]) -> Value {
        let mut value = serde_json::to_value(self).unwrap_or(Value::Null);
        if let Some(map) = value.as_object_mut() {
            map.insert("secret_names".into(), json!(secret_names));
            map.insert("used_by".into(), json!(used_by));
        }
        value
    }
    /// The names a `secret_source` yields into a run's env.
    pub fn yields(&self, stored: &BTreeSet<String>) -> Vec<String> {
        match self.source.as_ref().map(|s| s.provider) {
            Some(SourceProvider::Stored) => stored.iter().cloned().collect(),
            Some(SourceProvider::Kitbash) => self.names.clone(),
            None => Vec::new(),
        }
    }
}

/// Reads delivered values for `secret_source` connections whose provider is
/// not `stored` (Kitbash). Installed by the controller; tests use fixtures.
pub trait SourceReader: Send + Sync {
    /// The values `names` of `source` for `org`. Every name must be present.
    fn read(
        &self,
        org: &str,
        source: &Source,
        names: &[String],
    ) -> Result<BTreeMap<String, String>>;
}
struct NoReader;
impl SourceReader for NoReader {
    fn read(&self, _: &str, _: &Source, _: &[String]) -> Result<BTreeMap<String, String>> {
        bail!("No Kitbash delivery root is configured on this controller.")
    }
}

#[derive(Clone)]
pub struct Connections {
    root: PathBuf,
    records: Arc<Mutex<BTreeMap<String, Connection>>>,
    reader: Arc<dyn SourceReader>,
}

/// What [`Connections::apply`] merged into a provider block.
pub struct Applied {
    /// `{name, revision, kind}` per connection, plus `skipped` when the
    /// provider cannot use it. Never values.
    pub used: Vec<Value>,
}

impl Connections {
    pub fn open(root: &Path) -> Result<Self> {
        let dir = root.join("connections");
        state::private_dir(&dir)?;
        state::private_dir(&dir.join("secrets"))?;
        let mut records = BTreeMap::new();
        let mut names = BTreeSet::new();
        for path in state::json_files(&dir)? {
            let record: Connection = serde_json::from_value(state::read_json(&path)?)?;
            ensure!(
                path.file_stem().and_then(|s| s.to_str()) == Some(record.id.as_str())
                    && valid_id(&record.id)
                    && names.insert((record.org_id.clone(), record.name.clone())),
                "Invalid connection record"
            );
            records.insert(record.id.clone(), record);
        }
        Ok(Self {
            root: dir,
            records: Arc::new(Mutex::new(records)),
            reader: Arc::new(NoReader),
        })
    }
    pub fn with_reader(mut self, reader: Arc<dyn SourceReader>) -> Self {
        self.reader = reader;
        self
    }
    fn secrets_path(&self, id: &str) -> PathBuf {
        self.root.join("secrets").join(format!("{id}.json"))
    }
    fn secrets(&self, id: &str) -> Result<BTreeMap<String, String>> {
        let path = self.secrets_path(id);
        if !path.exists() {
            return Ok(BTreeMap::new());
        }
        let value = state::read_json(&path)?;
        serde_json::from_value(value["values"].clone()).context("Invalid connection secret store")
    }
    fn secret_names(&self, id: &str) -> BTreeSet<String> {
        self.secrets(id)
            .map(|m| m.into_keys().collect())
            .unwrap_or_default()
    }
    fn save(&self, record: &Connection) -> Result<()> {
        state::write_json(
            &self.root.join(format!("{}.json", record.id)),
            &serde_json::to_value(record)?,
        )
    }
    fn save_secrets(&self, id: &str, values: &BTreeMap<String, String>) -> Result<()> {
        if values.is_empty() {
            let _ = std::fs::remove_file(self.secrets_path(id));
            return Ok(());
        }
        state::write_json(&self.secrets_path(id), &json!({ "values": values }))
    }
    /// The record named `name` in `org`.
    pub fn get(&self, org: &str, name: &str) -> Option<Connection> {
        self.records
            .lock()
            .unwrap()
            .values()
            .find(|c| c.org_id == org && c.name == name)
            .cloned()
    }
    pub fn exists(&self, org: &str, name: &str) -> bool {
        self.get(org, name).is_some()
    }
    pub fn names(&self, org: &str) -> Vec<String> {
        self.records
            .lock()
            .unwrap()
            .values()
            .filter(|c| c.org_id == org)
            .map(|c| c.name.clone())
            .collect()
    }
    /// `GET /v1/connections`: the org's connections, metadata only.
    /// `used_by(name)` lists the profiles that reference one.
    pub fn list(&self, org: &str, used_by: &dyn Fn(&str) -> Vec<String>) -> Value {
        let records = self.records.lock().unwrap();
        let mut rows: Vec<_> = records.values().filter(|c| c.org_id == org).collect();
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        json!({"connections": rows
            .into_iter()
            .map(|c| c.public(&self.secret_names(&c.id), &used_by(&c.name)))
            .collect::<Vec<_>>()})
    }
    pub fn public(&self, org: &str, name: &str, used_by: &[String]) -> Result<Value> {
        let record = self.get(org, name).context("Unknown connection")?;
        Ok(json!({"connection": record.public(&self.secret_names(&record.id), used_by)}))
    }
    /// Parse the write-only `secrets` and `remove_secrets` of a body.
    fn secret_changes(body: &Value) -> Result<(BTreeMap<String, String>, Vec<String>)> {
        let mut set = BTreeMap::new();
        if let Some(secrets) = body.get("secrets") {
            let map = secrets
                .as_object()
                .context("secrets must map names to values.")?;
            for (name, value) in map {
                ensure!(
                    native::env_key_allowed(name),
                    "Secret names must match ^[A-Z][A-Z0-9_]{{0,63}}$ and be names a run may set."
                );
                let value = value.as_str().filter(|v| valid_secret_value(v));
                let Some(value) = value else {
                    bail!("Secret values must be strings of 1-16384 bytes without NUL.");
                };
                set.insert(name.clone(), value.to_owned());
            }
        }
        let remove = match body.get("remove_secrets") {
            None => Vec::new(),
            Some(list) => list
                .as_array()
                .context("remove_secrets must list names.")?
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .context("remove_secrets must list names.")
                })
                .collect::<Result<_>>()?,
        };
        Ok((set, remove))
    }
    /// Build a record from the declarative fields of `body`.
    fn from_body(body: &Value, base: Value) -> Result<Connection> {
        let object = body.as_object().context("Expected object")?;
        ensure!(
            object.keys().all(|k| BODY_FIELDS.contains(&k.as_str())),
            "Unsupported connection fields."
        );
        let mut merged = base;
        for key in SPEC_FIELDS {
            if let Some(value) = object.get(*key) {
                merged[*key] = value.clone();
            }
        }
        serde_json::from_value(merged).context("Connection fields have the wrong shape.")
    }
    /// `POST /v1/connections`: create a connection in `org`.
    pub fn create(&self, org: &str, actor: &str, body: &Value) -> Result<Value> {
        let name = body["name"].as_str().context("name is required.")?;
        ensure!(body.get("kind").is_some(), "kind is required.");
        ensure!(
            !self.exists(org, name),
            "A connection with that name already exists."
        );
        let at = now();
        let base = json!({
            "id": uuid::Uuid::new_v4().simple().to_string(),
            "org_id": org,
            "revision": 1,
            "created_by": actor,
            "created_at": at,
            "updated_by": actor,
            "updated_at": at,
        });
        let record = Self::from_body(body, base)?;
        let (secrets, remove) = Self::secret_changes(body)?;
        ensure!(remove.is_empty(), "remove_secrets applies only to updates.");
        record.validate(&secrets.keys().cloned().collect())?;
        let mut records = self.records.lock().unwrap();
        ensure!(records.len() < MAX_CONNECTIONS, "Connection limit reached.");
        ensure!(
            !records
                .values()
                .any(|c| c.org_id == org && c.name == record.name),
            "A connection with that name already exists."
        );
        self.save_secrets(&record.id, &secrets)?;
        self.save(&record)?;
        let public = record.public(&secrets.keys().cloned().collect(), &[]);
        records.insert(record.id.clone(), record);
        Ok(json!({"connection": public, "secrets_set": secrets.keys().collect::<Vec<_>>()}))
    }
    /// `POST /v1/connections/{name}`: change fields and secrets. Name and kind
    /// never change; secrets not named are kept.
    pub fn update(&self, org: &str, name: &str, actor: &str, body: &Value) -> Result<Value> {
        let existing = self.get(org, name).context("Unknown connection")?;
        if let Some(n) = body.get("name") {
            ensure!(n.as_str() == Some(name), "A connection cannot be renamed.");
        }
        if let Some(k) = body.get("kind") {
            ensure!(
                *k == json!(existing.kind),
                "A connection's kind cannot change; create a new connection."
            );
        }
        let mut record = Self::from_body(body, serde_json::to_value(&existing)?)?;
        let (set, remove) = Self::secret_changes(body)?;
        let mut secrets = self.secrets(&existing.id)?;
        for name in &remove {
            ensure!(
                secrets.remove(name).is_some(),
                "remove_secrets names an unknown secret."
            );
        }
        secrets.extend(set.clone());
        record.validate(&secrets.keys().cloned().collect())?;
        let changed = record.spec() != existing.spec() || !set.is_empty() || !remove.is_empty();
        if changed {
            record.revision = existing.revision + 1;
            record.updated_by = actor.to_owned();
            record.updated_at = now();
        }
        let mut records = self.records.lock().unwrap();
        ensure!(
            records
                .get(&existing.id)
                .is_some_and(|c| c.revision == existing.revision),
            "The connection changed concurrently; retry."
        );
        if changed {
            self.save_secrets(&record.id, &secrets)?;
            self.save(&record)?;
        }
        let public = record.public(&secrets.keys().cloned().collect(), &[]);
        records.insert(record.id.clone(), record);
        Ok(json!({
            "connection": public,
            "changed": changed,
            "secrets_set": set.keys().collect::<Vec<_>>(),
            "secrets_removed": remove,
        }))
    }
    /// The record a declarative `spec` describes (no values), based on the
    /// existing one when present. Fields the spec leaves out are cleared.
    fn desired(
        &self,
        org: &str,
        actor: &str,
        spec: &Value,
    ) -> Result<(Connection, Option<Connection>)> {
        let name = spec["name"]
            .as_str()
            .context("Each connection needs a name.")?;
        let existing = self.get(org, name);
        let at = now();
        let mut base = match &existing {
            Some(e) => serde_json::to_value(e)?,
            None => json!({
                "id": uuid::Uuid::new_v4().simple().to_string(),
                "org_id": org,
                "revision": 1,
                "created_by": actor,
                "created_at": at,
                "updated_by": actor,
                "updated_at": at,
            }),
        };
        if let Some(map) = base.as_object_mut() {
            map.retain(|k, _| !SPEC_FIELDS.contains(&k.as_str()));
        }
        let desired = Self::from_body(spec, base)?;
        if let Some(e) = &existing {
            ensure!(
                e.kind == desired.kind,
                "Connection {name} exists with another kind; delete it first."
            );
        }
        let stored = existing
            .as_ref()
            .map(|e| self.secret_names(&e.id))
            .unwrap_or_default();
        desired.validate(&stored)?;
        Ok((desired, existing))
    }
    /// What applying `spec` would do: `create`, `update` or `unchanged`.
    pub fn plan(&self, org: &str, spec: &Value) -> Result<&'static str> {
        let (desired, existing) = self.desired(org, "owner", spec)?;
        Ok(match existing {
            None => "create",
            Some(e) if e.spec() == desired.spec() => "unchanged",
            Some(_) => "update",
        })
    }
    /// Create or replace a connection's declarative fields from `spec`,
    /// keeping its stored secrets.
    pub fn put_spec(&self, org: &str, actor: &str, spec: &Value) -> Result<()> {
        let (mut desired, existing) = self.desired(org, actor, spec)?;
        let mut records = self.records.lock().unwrap();
        match &existing {
            Some(e) => {
                if e.spec() == desired.spec() {
                    return Ok(());
                }
                ensure!(
                    records.get(&e.id).is_some_and(|c| c.revision == e.revision),
                    "The connection changed concurrently; retry."
                );
                desired.revision = e.revision + 1;
                desired.updated_by = actor.to_owned();
                desired.updated_at = now();
            }
            None => {
                ensure!(records.len() < MAX_CONNECTIONS, "Connection limit reached.");
                ensure!(
                    !records
                        .values()
                        .any(|c| c.org_id == org && c.name == desired.name),
                    "A connection with that name already exists."
                );
            }
        }
        self.save(&desired)?;
        records.insert(desired.id.clone(), desired);
        Ok(())
    }
    /// `POST /v1/connections/{name}/delete`. `used_by` must be empty: a
    /// profile that references a connection keeps it.
    pub fn delete(&self, org: &str, name: &str, used_by: &[String]) -> Result<Value> {
        ensure!(
            used_by.is_empty(),
            "Detach the connection from its profiles first: {}.",
            used_by.join(", ")
        );
        let mut records = self.records.lock().unwrap();
        let id = records
            .values()
            .find(|c| c.org_id == org && c.name == name)
            .map(|c| c.id.clone())
            .context("Unknown connection")?;
        let _ = std::fs::remove_file(self.secrets_path(&id));
        std::fs::remove_file(self.root.join(format!("{id}.json")))?;
        records.remove(&id);
        Ok(json!({"deleted": name}))
    }
    /// The values a `secret_source` connection yields (all of them).
    fn source_values(&self, record: &Connection) -> Result<BTreeMap<String, String>> {
        let source = record.source.as_ref().context("Not a secret source")?;
        match source.provider {
            SourceProvider::Stored => self.secrets(&record.id),
            SourceProvider::Kitbash => {
                let values = self.reader.read(&record.org_id, source, &record.names)?;
                ensure!(
                    record
                        .names
                        .iter()
                        .all(|n| values.get(n).is_some_and(|v| valid_secret_value(v))),
                    "The Kitbash delivery for connection {} is missing a value it should yield.",
                    record.name
                );
                Ok(values)
            }
        }
    }
    /// One value a `secret_source` connection yields, for controller-side
    /// consumers such as GitHub Actions secret writes. Never returned to a caller.
    pub fn source_value(&self, org: &str, connection: &str, name: &str) -> Result<String> {
        let record = self
            .get(org, connection)
            .filter(|c| c.kind == Kind::SecretSource)
            .with_context(|| format!("Unknown secret source {connection}."))?;
        let stored = self.secret_names(&record.id);
        ensure!(
            record.yields(&stored).iter().any(|n| n == name),
            "Secret source {connection} does not yield {name}."
        );
        self.source_values(&record)?
            .remove(name)
            .with_context(|| format!("Secret source {connection} is missing {name}."))
    }
    /// Resolve one reference to a value. `Ok(None)` for `env` references (the
    /// run supplies them).
    fn resolve_ref(
        &self,
        record: &Connection,
        reference: &ValueRef,
        own: &BTreeMap<String, String>,
        sources: &mut BTreeMap<String, BTreeMap<String, String>>,
    ) -> Result<Option<String>> {
        let prefix = reference.prefix.clone().unwrap_or_default();
        if reference.env.is_some() {
            return Ok(None);
        }
        if let Some(secret) = &reference.secret {
            let value = own.get(secret).with_context(|| {
                format!("Connection {} has no stored secret {secret}.", record.name)
            })?;
            return Ok(Some(format!("{prefix}{value}")));
        }
        let source = reference
            .secret_source
            .as_deref()
            .context("Invalid reference")?;
        let name = reference.name.as_deref().context("Invalid reference")?;
        if !sources.contains_key(source) {
            let other = self
                .get(&record.org_id, source)
                .filter(|c| c.kind == Kind::SecretSource)
                .with_context(|| {
                    format!(
                        "Connection {} references unknown secret source {source}.",
                        record.name
                    )
                })?;
            sources.insert(source.to_owned(), self.source_values(&other)?);
        }
        let value = sources[source]
            .get(name)
            .with_context(|| format!("Secret source {source} does not yield {name}."))?;
        Ok(Some(format!("{prefix}{value}")))
    }
    /// The env variable that carries a header's resolved value.
    fn header_var(connection: &str, header: &str) -> String {
        let var = format!("CONN_{connection}_{header}")
            .to_ascii_uppercase()
            .replace('-', "_");
        if var.len() <= 64 {
            var
        } else {
            format!(
                "CONN_{}",
                &profile::sha256_hex(format!("{connection}\0{header}").as_bytes())[..32]
            )
            .to_ascii_uppercase()
        }
    }
    /// Merge the named connections of `org` into `block` for one run on
    /// `provider`: MCP servers into its generated config, secret values into
    /// its env (never into a prompt). Fails if a connection is unknown, a
    /// value is missing, or a variable or server name clashes with the
    /// profile's own; the caller turns that into a refused launch.
    pub fn apply(
        &self,
        org: &str,
        names: &[String],
        provider: &str,
        template: Option<&str>,
        block: &mut ProviderProfile,
    ) -> Result<Applied> {
        let mut used = Vec::new();
        let mut env = BTreeMap::new();
        let mut sources = BTreeMap::new();
        let mcp_capable = provider != "cursor";
        for name in names {
            let record = self
                .get(org, name)
                .with_context(|| format!("The profile references unknown connection {name}."))?;
            let mut row =
                json!({"name": record.name, "revision": record.revision, "kind": record.kind});
            let own = self.secrets(&record.id)?;
            match record.kind {
                Kind::McpHttp | Kind::McpStdio if !mcp_capable => {
                    row["skipped"] = json!("provider_has_no_mcp");
                }
                Kind::McpHttp => {
                    let mut headers_from_env = BTreeMap::new();
                    for (header, reference) in &record.headers {
                        let var = match self.resolve_ref(&record, reference, &own, &mut sources)? {
                            None => reference.env.clone().unwrap_or_default(),
                            Some(value) => {
                                let var = Self::header_var(&record.name, header);
                                set_once(&mut env, &var, value)?;
                                var
                            }
                        };
                        headers_from_env.insert(header.clone(), var);
                    }
                    ensure!(
                        !block.mcp_servers.iter().any(|s| s.name == record.name),
                        "Connection {} has the name of one of the profile's own MCP servers.",
                        record.name
                    );
                    block.mcp_servers.push(McpServer {
                        name: record.name.clone(),
                        command: None,
                        args: Vec::new(),
                        env_vars: Vec::new(),
                        url: record.url.clone(),
                        headers_from_env,
                    });
                }
                Kind::McpStdio => {
                    if let Some(required) = &record.template {
                        ensure!(
                            template == Some(required.as_str()),
                            "Connection {} needs workspaces from template {required}; set it as the profile's template.",
                            record.name
                        );
                    }
                    let mut env_vars = Vec::new();
                    for (var, reference) in &record.env {
                        match self.resolve_ref(&record, reference, &own, &mut sources)? {
                            None => env_vars.push(reference.env.clone().unwrap_or_default()),
                            Some(value) => {
                                set_once(&mut env, var, value)?;
                                env_vars.push(var.clone());
                            }
                        }
                    }
                    ensure!(
                        !block.mcp_servers.iter().any(|s| s.name == record.name),
                        "Connection {} has the name of one of the profile's own MCP servers.",
                        record.name
                    );
                    block.mcp_servers.push(McpServer {
                        name: record.name.clone(),
                        command: record.command.clone(),
                        args: record.args.clone(),
                        env_vars,
                        url: None,
                        headers_from_env: BTreeMap::new(),
                    });
                }
                Kind::SecretSource => {
                    let values = match sources.get(&record.name) {
                        Some(v) => v.clone(),
                        None => {
                            let v = self.source_values(&record)?;
                            sources.insert(record.name.clone(), v.clone());
                            v
                        }
                    };
                    let yielded = record.yields(&own.keys().cloned().collect());
                    for name in &yielded {
                        let value = values.get(name).with_context(|| {
                            format!("Secret source {} is missing {name}.", record.name)
                        })?;
                        set_once(&mut env, name, value.clone())?;
                    }
                    row["names"] = json!(yielded);
                }
                Kind::AgentAccount => bail!("agent_account connections are reserved."),
            }
            used.push(row);
        }
        for (var, value) in env {
            ensure!(
                native::env_key_allowed(&var),
                "A connection yields {var}, a variable Engine Box controls."
            );
            ensure!(
                !block.env.contains_key(&var),
                "A connection sets {var}, which the profile's env already sets."
            );
            block.env.insert(var, value);
        }
        native::validate_env_map(&block.env)?;
        self.touch(org, names);
        Ok(Applied { used })
    }
    /// Record that a run used these connections (`last_used_at`), at most
    /// once a minute per connection.
    fn touch(&self, org: &str, names: &[String]) {
        let at = chrono::Utc::now();
        let mut records = self.records.lock().unwrap();
        for record in records
            .values_mut()
            .filter(|c| c.org_id == org && names.contains(&c.name))
        {
            let stale = record
                .last_used_at
                .as_deref()
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .is_none_or(|t| {
                    (at - t.with_timezone(&chrono::Utc)).num_seconds() >= USE_WRITE_INTERVAL_SECONDS
                });
            if stale {
                record.last_used_at = Some(at.to_rfc3339());
                let _ = state::write_json(
                    &self.root.join(format!("{}.json", record.id)),
                    &serde_json::to_value(&*record).unwrap_or(Value::Null),
                );
            }
        }
    }
    /// `POST /v1/connections/{name}/test`: check that every value resolves
    /// and, for `mcp_http`, that the server answers an MCP `initialize` and
    /// accepts the credentials. Reports names and statuses only.
    pub fn test(&self, org: &str, name: &str, probe: &dyn HttpProbe) -> Result<Value> {
        let record = self.get(org, name).context("Unknown connection")?;
        let own = self.secrets(&record.id)?;
        let mut sources = BTreeMap::new();
        let mut missing = Vec::new();
        let mut run_env = Vec::new();
        let mut headers = BTreeMap::new();
        for (key, reference) in record.headers.iter().chain(record.env.iter()) {
            match self.resolve_ref(&record, reference, &own, &mut sources) {
                Ok(Some(value)) => {
                    headers.insert(key.clone(), value);
                }
                Ok(None) => run_env.push(reference.env.clone().unwrap_or_default()),
                Err(_) => missing.push(key.clone()),
            }
        }
        let mut result = json!({
            "name": record.name,
            "kind": record.kind,
            "values_resolved": missing.is_empty(),
            "unresolved": missing,
            "needs_run_env": run_env,
        });
        match record.kind {
            Kind::SecretSource => {
                let outcome = self.source_values(&record);
                let yielded = record.yields(&own.keys().cloned().collect());
                let present: Vec<_> = match &outcome {
                    Ok(values) => yielded
                        .iter()
                        .filter(|n| values.contains_key(*n))
                        .cloned()
                        .collect(),
                    Err(_) => Vec::new(),
                };
                let absent: Vec<_> = yielded
                    .iter()
                    .filter(|n| !present.contains(n))
                    .cloned()
                    .collect();
                result["ok"] = json!(outcome.is_ok() && absent.is_empty());
                result["present"] = json!(present);
                result["missing"] = json!(absent);
                if let Err(error) = outcome {
                    result["error"] = json!(error.to_string());
                }
            }
            Kind::McpStdio => {
                result["ok"] = result["values_resolved"].clone();
                result["reachable"] = Value::Null;
                result["note"] = json!(
                    "stdio servers start inside the workspace; the command is checked when a run starts."
                );
            }
            Kind::McpHttp => {
                let complete = result["values_resolved"] == true;
                let url = record.url.clone().unwrap_or_default();
                let outcome = if complete && run_env.is_empty() {
                    probe.initialize(org, &url, &headers)
                } else {
                    // Without every header value the server cannot judge auth;
                    // still check that it answers.
                    probe.initialize(org, &url, &BTreeMap::new())
                };
                match outcome {
                    Ok(status) => {
                        result["reachable"] = json!(true);
                        result["http_status"] = json!(status);
                        result["auth"] = json!(match status {
                            200..=299 if complete && run_env.is_empty() => "accepted",
                            401 | 403 => "rejected",
                            _ => "unknown",
                        });
                        result["ok"] = json!(complete && (200..=299).contains(&status));
                    }
                    Err(error) => {
                        result["reachable"] = json!(false);
                        result["ok"] = json!(false);
                        result["error"] = json!(error.to_string());
                    }
                }
            }
            Kind::AgentAccount => bail!("agent_account connections are reserved."),
        }
        Ok(json!({"test": result}))
    }
}

/// Sends one MCP `initialize` to a URL with headers and returns the HTTP status.
pub trait HttpProbe {
    fn initialize(&self, org: &str, url: &str, headers: &BTreeMap<String, String>) -> Result<u16>;
}

/// The controller's probe: HTTPS only (HTTP on loopback for the platform
/// org), no redirects, 10 s, and for orgs other than the platform org never an
/// address on a loopback, private or link-local network. Reports a status,
/// never a response body.
pub struct NetworkProbe;
impl HttpProbe for NetworkProbe {
    fn initialize(&self, org: &str, url: &str, headers: &BTreeMap<String, String>) -> Result<u16> {
        let parsed = url::Url::parse(url)?;
        let host = parsed
            .host_str()
            .context("The URL has no host.")?
            .to_owned();
        let port = parsed
            .port_or_known_default()
            .context("The URL has no port.")?;
        if org != pools::DEFAULT_ORG {
            use std::net::ToSocketAddrs;
            let addresses: Vec<_> = (host.trim_matches(['[', ']']), port)
                .to_socket_addrs()
                .context("The server's host name does not resolve.")?
                .collect();
            ensure!(
                !addresses.is_empty() && addresses.iter().all(|a| public_address(&a.ip())),
                "The server resolves to a private or local address; only the platform org may test those."
            );
        }
        let client = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(10))
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()?;
        let mut request = client
            .post(parsed)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let response = request
            .body(
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"enginebox-connection-test","version":env!("CARGO_PKG_VERSION")}}})
                    .to_string(),
            )
            .send()
            .map_err(|_| anyhow::anyhow!("The server did not answer."))?;
        Ok(response.status().as_u16())
    }
}

fn public_address(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]))
        }
        std::net::IpAddr::V6(v6) => {
            !(v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| !public_address(&std::net::IpAddr::V4(v4))))
        }
    }
}

/// Two connections never set the same variable.
fn set_once(env: &mut BTreeMap<String, String>, var: &str, value: String) -> Result<()> {
    ensure!(
        env.insert(var.to_owned(), value).is_none(),
        "Two connections set the variable {var}."
    );
    Ok(())
}

fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn open() -> (tempfile::TempDir, Connections) {
        let dir = tempfile::tempdir().unwrap();
        let c = Connections::open(dir.path()).unwrap();
        (dir, c)
    }
    const TOKEN: &str = "kb-test-value-0123456789";

    fn kitbash_http(c: &Connections, org: &str) {
        c.create(
            org,
            "owner",
            &json!({"name":"kitbash","kind":"mcp_http","url":"https://kitbash.example.com/mcp",
                    "headers":{"Authorization":{"secret":"KITBASH_TOKEN","prefix":"Bearer "}},
                    "secrets":{"KITBASH_TOKEN":TOKEN}}),
        )
        .unwrap();
    }

    #[test]
    fn crud_is_org_scoped_and_audit_fields_are_kept() {
        let (_d, c) = open();
        kitbash_http(&c, "default");
        kitbash_http(&c, "org_aaaaaaaaaaaaaaaaaaaaaaaa");
        assert!(
            c.create(
                "default",
                "owner",
                &json!({"name":"kitbash","kind":"mcp_http","url":"https://x.example.com"})
            )
            .is_err()
        );
        let listed = c.list("org_aaaaaaaaaaaaaaaaaaaaaaaa", &|_| vec![]);
        assert_eq!(listed["connections"].as_array().unwrap().len(), 1);
        assert_eq!(
            listed["connections"][0]["org_id"],
            "org_aaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(listed["connections"][0]["created_by"], "owner");
        assert!(c.get("org_bbbbbbbbbbbbbbbbbbbbbbbb", "kitbash").is_none());
        let updated = c
            .update(
                "org_aaaaaaaaaaaaaaaaaaaaaaaa",
                "kitbash",
                "usr_0123456789abcdef01234567",
                &json!({"description":"Kitbash MCP"}),
            )
            .unwrap();
        assert_eq!(updated["connection"]["revision"], 2);
        assert_eq!(
            updated["connection"]["updated_by"],
            "usr_0123456789abcdef01234567"
        );
        assert!(
            c.update(
                "org_aaaaaaaaaaaaaaaaaaaaaaaa",
                "kitbash",
                "owner",
                &json!({"name":"other"})
            )
            .is_err()
        );
        assert!(
            c.update(
                "org_aaaaaaaaaaaaaaaaaaaaaaaa",
                "kitbash",
                "owner",
                &json!({"kind":"mcp_stdio"})
            )
            .is_err()
        );
        assert!(
            c.delete("org_aaaaaaaaaaaaaaaaaaaaaaaa", "kitbash", &["coder".into()])
                .is_err()
        );
        c.delete("org_aaaaaaaaaaaaaaaaaaaaaaaa", "kitbash", &[])
            .unwrap();
        assert!(c.get("org_aaaaaaaaaaaaaaaaaaaaaaaa", "kitbash").is_none());
        assert!(c.get("default", "kitbash").is_some());
        // Reopening reads the same records.
        let again = Connections::open(c.root.parent().unwrap()).unwrap();
        assert!(again.get("default", "kitbash").is_some());
    }

    #[test]
    fn secret_values_are_write_only() {
        let (dir, c) = open();
        kitbash_http(&c, "default");
        let created_again = c.public("default", "kitbash", &[]).unwrap().to_string();
        let listed = c.list("default", &|_| vec![]).to_string();
        let updated = c
            .update(
                "default",
                "kitbash",
                "owner",
                &json!({"secrets":{"KITBASH_TOKEN":"another-secret-value-99"}}),
            )
            .unwrap()
            .to_string();
        for text in [&created_again, &listed, &updated] {
            assert!(!text.contains(TOKEN) && !text.contains("another-secret-value-99"));
        }
        assert!(listed.contains("KITBASH_TOKEN"));
        // A malformed value is refused without echoing it.
        let error = c
            .update(
                "default",
                "kitbash",
                "owner",
                &json!({"secrets":{"KITBASH_TOKEN":"bad\u{0}value-secret"}}),
            )
            .unwrap_err()
            .to_string();
        assert!(!error.contains("value-secret"));
        // The record file never holds the value; the secret file is private.
        let record = std::fs::read_dir(dir.path().join("connections"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .map(|e| std::fs::read_to_string(e.path()).unwrap())
            .collect::<String>();
        assert!(!record.contains("another-secret-value-99"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for entry in std::fs::read_dir(dir.path().join("connections/secrets")).unwrap() {
                let mode = entry.unwrap().metadata().unwrap().permissions().mode();
                assert_eq!(mode & 0o077, 0);
            }
        }
        // A stored secret nothing references is refused.
        assert!(c.create("default", "owner", &json!({"name":"x","kind":"mcp_http","url":"https://x.example.com","secrets":{"UNUSED":"value-12345678"}})).is_err());
        // Agent accounts are reserved.
        assert!(
            c.create(
                "default",
                "owner",
                &json!({"name":"acct","kind":"agent_account"})
            )
            .is_err()
        );
    }

    #[test]
    fn resolution_merges_mcp_config_and_env() {
        let (_d, c) = open();
        kitbash_http(&c, "default");
        c.create(
            "default",
            "owner",
            &json!({"name":"staging-secrets","kind":"secret_source","source":{"provider":"stored","project":"canvalytics","environment":"staging"},
                    "secrets":{"DATABASE_URL":"postgres://staging-value"}}),
        )
        .unwrap();
        c.create(
            "default",
            "owner",
            &json!({"name":"d1-readonly","kind":"mcp_stdio","command":"/usr/local/bin/d1-mcp","args":["--read-only"],
                    "env":{"CLOUDFLARE_API_TOKEN":{"secret_source":"staging-secrets","name":"DATABASE_URL"},"HQ_ORG":{"env":"HQ_ORG"}}}),
        )
        .unwrap();
        c.create(
            "default",
            "owner",
            &json!({"name":"hq","kind":"mcp_http","url":"https://ideator.example.com/mcp","headers":{"Authorization":{"env":"HQ_AUTHORIZATION"}}}),
        )
        .unwrap();
        let mut block = ProviderProfile::default();
        block.env.insert("CHECKS_MODE".into(), "headless".into());
        let names: Vec<String> = ["kitbash", "staging-secrets", "d1-readonly", "hq"]
            .map(String::from)
            .to_vec();
        let applied = c
            .apply("default", &names, "claude", None, &mut block)
            .unwrap();
        assert_eq!(applied.used.len(), 4);
        let kitbash = block
            .mcp_servers
            .iter()
            .find(|s| s.name == "kitbash")
            .unwrap();
        assert_eq!(
            kitbash.url.as_deref(),
            Some("https://kitbash.example.com/mcp")
        );
        let var = &kitbash.headers_from_env["Authorization"];
        assert_eq!(block.env[var], format!("Bearer {TOKEN}"));
        assert_eq!(block.env["DATABASE_URL"], "postgres://staging-value");
        let d1 = block
            .mcp_servers
            .iter()
            .find(|s| s.name == "d1-readonly")
            .unwrap();
        assert_eq!(
            d1.env_vars,
            vec!["CLOUDFLARE_API_TOKEN".to_string(), "HQ_ORG".to_string()]
        );
        assert_eq!(
            block.env["CLOUDFLARE_API_TOKEN"],
            "postgres://staging-value"
        );
        let hq = block.mcp_servers.iter().find(|s| s.name == "hq").unwrap();
        assert_eq!(hq.headers_from_env["Authorization"], "HQ_AUTHORIZATION");
        assert!(!block.env.contains_key("HQ_AUTHORIZATION"));
        assert_eq!(block.env["CHECKS_MODE"], "headless");
        // The used rows never carry a value.
        assert!(!json!(applied.used).to_string().contains(TOKEN));
        // Cursor gets secrets but no MCP servers.
        let mut cursor = ProviderProfile::default();
        let applied = c
            .apply("default", &names, "cursor", None, &mut cursor)
            .unwrap();
        assert!(cursor.mcp_servers.is_empty());
        assert_eq!(applied.used[0]["skipped"], "provider_has_no_mcp");
        assert!(cursor.env.contains_key("DATABASE_URL"));
        // Another org cannot resolve these names; clashes are refused.
        assert!(
            c.apply(
                "org_aaaaaaaaaaaaaaaaaaaaaaaa",
                &names,
                "claude",
                None,
                &mut ProviderProfile::default()
            )
            .is_err()
        );
        let mut clash = ProviderProfile::default();
        clash.env.insert("DATABASE_URL".into(), "x".into());
        assert!(
            c.apply("default", &names, "claude", None, &mut clash)
                .is_err()
        );
        // last_used_at was recorded.
        assert!(c.get("default", "kitbash").unwrap().last_used_at.is_some());
    }

    #[test]
    fn stdio_commands_are_allowlisted_and_template_bound() {
        let (_d, c) = open();
        for bad in ["npx", "/tmp/x", "/usr/bin/../../tmp/x", "/opt/"] {
            assert!(
                c.create(
                    "default",
                    "owner",
                    &json!({"name":"s","kind":"mcp_stdio","command":bad})
                )
                .is_err(),
                "{bad}"
            );
        }
        c.create("default", "owner", &json!({"name":"s","kind":"mcp_stdio","command":"/opt/tools/bin/server","template":"engine-godot"})).unwrap();
        let names = vec!["s".to_string()];
        assert!(
            c.apply(
                "default",
                &names,
                "codex",
                None,
                &mut ProviderProfile::default()
            )
            .is_err()
        );
        c.apply(
            "default",
            &names,
            "codex",
            Some("engine-godot"),
            &mut ProviderProfile::default(),
        )
        .unwrap();
    }

    struct Fixed(u16);
    impl HttpProbe for Fixed {
        fn initialize(&self, _: &str, _: &str, headers: &BTreeMap<String, String>) -> Result<u16> {
            Ok(
                if headers
                    .get("Authorization")
                    .is_some_and(|v| v == &format!("Bearer {TOKEN}"))
                {
                    self.0
                } else {
                    401
                },
            )
        }
    }
    #[test]
    fn test_reports_reachability_and_auth_without_values() {
        let (_d, c) = open();
        kitbash_http(&c, "default");
        let result = c.test("default", "kitbash", &Fixed(200)).unwrap();
        assert_eq!(result["test"]["auth"], "accepted");
        assert_eq!(result["test"]["ok"], true);
        assert!(!result.to_string().contains(TOKEN));
        c.update(
            "default",
            "kitbash",
            "owner",
            &json!({"secrets":{"KITBASH_TOKEN":"wrong-token-value-1234"}}),
        )
        .unwrap();
        let result = c.test("default", "kitbash", &Fixed(200)).unwrap();
        assert_eq!(result["test"]["auth"], "rejected");
        assert!(!result.to_string().contains("wrong-token-value-1234"));
    }

    #[test]
    fn header_variables_are_valid_env_names() {
        assert_eq!(
            Connections::header_var("kitbash", "Authorization"),
            "CONN_KITBASH_AUTHORIZATION"
        );
        let long = Connections::header_var(&"a".repeat(60), "X-Long-Header");
        assert!(native::env_key_allowed(&long));
        assert!(!public_address(&"10.0.0.1".parse().unwrap()));
        assert!(!public_address(&"::1".parse().unwrap()));
        assert!(public_address(&"1.1.1.1".parse().unwrap()));
    }
}
