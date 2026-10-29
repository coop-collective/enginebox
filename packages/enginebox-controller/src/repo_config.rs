//! Repository agent profiles and secret requirements (docs/PROJECT-CONFIG.md,
//! schema version 2 of `.enginebox/config.json`).
//!
//! Repository content is untrusted: any pull request or agent can edit it. A
//! repository therefore only **declares** what its runs want (secret names
//! with a qualifier and run kinds, catalog MCP connections by name, its own
//! MCP server URLs). It can never grant itself anything. The org catalog
//! **satisfies** a requirement only through an explicit mapping an owner or
//! admin approved for that repository, repository profile and requirement;
//! the approval records the qualifier it was granted for, so changing the
//! qualifier (staging -> production, read-only -> write) or a server's URL
//! invalidates it until someone approves it again. A requirement without an
//! approved mapping is unmet: the run starts without it and its record says
//! so, or the start is refused when the requirement is marked `required`.
use crate::connections::Connections;
use anyhow::{Context, Result, bail, ensure};
use enginebox_agents::{native, pools, profile};
use enginebox_core::state;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// The pseudo repository profile under which the `dev` section's secret
/// requirements are mapped.
pub const DEV_PROFILE: &str = ":dev";
const MAX_MAPPINGS: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requirement {
    pub name: String,
    pub qualifier: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub run_kinds: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Mcp {
    /// A connection of the org catalog, by name.
    Catalog(String),
    /// A server the repository declares: a URL and nothing else. It carries
    /// no headers, so it can never pull a run's secrets into a request.
    Server(ServerDecl),
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerDecl {
    pub name: String,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Command {
    /// `"dev"`: reuse the `dev` section's command, so it is defined once.
    Section(String),
    Argv(Vec<String>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CiJob {
    pub name: String,
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProfile {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub default: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<Command>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dev: Option<Command>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test: Option<Command>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp: Vec<Mcp>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<Requirement>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ci: Vec<CiJob>,
}

/// The parts of `.enginebox/config.json` the controller reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepoConfig {
    pub schema_version: u64,
    pub dev_secrets: Vec<Requirement>,
    pub profiles: BTreeMap<String, AgentProfile>,
}

fn valid_qualifier(q: &str) -> bool {
    (1..=64).contains(&q.len())
        && q.as_bytes()[0].is_ascii_lowercase()
        && q.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.:-".contains(&b))
}
fn valid_profile_name(n: &str) -> bool {
    crate::regex_free_profile_name(n)
}
fn argv(list: &[String], what: &str) -> Result<()> {
    ensure!(
        !list.is_empty()
            && list.len() <= 32
            && list.iter().all(|a| a.len() <= 1024 && !a.contains('\0')),
        "{what} must be 1-32 arguments of at most 1024 bytes."
    );
    Ok(())
}
fn requirements(list: &[Requirement], taken: &mut BTreeSet<String>) -> Result<()> {
    ensure!(
        list.len() <= 64,
        "At most 64 secret requirements per profile."
    );
    for r in list {
        ensure!(
            native::env_key_allowed(&r.name) && taken.insert(r.name.clone()),
            "Secret requirement names must be distinct variables a run may set."
        );
        ensure!(
            valid_qualifier(&r.qualifier),
            "Each secret requirement needs a qualifier matching ^[a-z][a-z0-9_.:-]{{0,63}}$ (for example staging or read-only)."
        );
        ensure!(
            r.run_kinds.len() <= 32 && r.run_kinds.iter().all(|k| profile::valid_run_kind(k)),
            "run_kinds must list run kinds."
        );
        ensure!(
            r.description.as_ref().is_none_or(|d| d.len() <= 512),
            "Requirement descriptions are at most 512 bytes."
        );
    }
    Ok(())
}

impl RepoConfig {
    /// Parse and validate a `.enginebox/config.json`. Version 1 files carry
    /// no agent profiles or requirements.
    pub fn parse(value: &Value) -> Result<Self> {
        let object = value
            .as_object()
            .context("The repository config must be an object.")?;
        let version = object
            .get("schema_version")
            .and_then(Value::as_u64)
            .context("schema_version is required.")?;
        match version {
            1 => {
                ensure!(
                    object
                        .keys()
                        .all(|k| ["schema_version", "dev"].contains(&k.as_str())),
                    "A version 1 config has only schema_version and dev."
                );
                return Ok(Self {
                    schema_version: 1,
                    ..Default::default()
                });
            }
            2 => {}
            _ => bail!("schema_version must be 1 or 2."),
        }
        ensure!(
            object
                .keys()
                .all(|k| ["schema_version", "dev", "agents"].contains(&k.as_str())),
            "A version 2 config has schema_version, dev and agents."
        );
        let mut config = Self {
            schema_version: 2,
            ..Default::default()
        };
        if let Some(dev) = object.get("dev") {
            let dev = dev.as_object().context("dev must be an object.")?;
            ensure!(
                dev.keys()
                    .all(|k| ["template", "secrets"].contains(&k.as_str())),
                "dev has template and secrets."
            );
            if let Some(secrets) = dev.get("secrets") {
                config.dev_secrets = serde_json::from_value(secrets.clone())
                    .context("dev.secrets lists {name, qualifier, run_kinds, required}.")?;
                requirements(&config.dev_secrets, &mut BTreeSet::new())?;
            }
        }
        if let Some(agents) = object.get("agents") {
            let agents = agents.as_object().context("agents must be an object.")?;
            ensure!(
                agents.keys().all(|k| k == "profiles"),
                "agents has profiles."
            );
            if let Some(profiles) = agents.get("profiles") {
                config.profiles = serde_json::from_value(profiles.clone())
                    .context("agents.profiles maps names to profiles.")?;
            }
        }
        ensure!(config.profiles.len() <= 16, "At most 16 agent profiles.");
        if !config.profiles.is_empty() {
            ensure!(
                config.profiles.values().filter(|p| p.default).count() == 1,
                "Exactly one agent profile is marked default: true."
            );
        }
        for (name, p) in &config.profiles {
            ensure!(
                valid_profile_name(name),
                "Agent profile names match ^[a-z][a-z0-9-]{{0,31}}$."
            );
            if let Some(t) = &p.template {
                enginebox_core::identifier(t, false)?;
            }
            if let Some(size) = &p.size {
                ensure!(
                    ["minimum", "recommended"].contains(&size.as_str()),
                    "size is minimum or recommended."
                );
            }
            for (what, command) in [("setup", &p.setup), ("dev", &p.dev), ("test", &p.test)] {
                match command {
                    None => {}
                    Some(Command::Section(s)) => ensure!(
                        s == "dev" && object.get("dev").is_some(),
                        "{what} may reference only \"dev\" (the dev section's command)."
                    ),
                    Some(Command::Argv(list)) => argv(list, what)?,
                }
            }
            native::validate_env_map(&p.env)?;
            let mut taken: BTreeSet<String> = p.env.keys().cloned().collect();
            requirements(&p.secrets, &mut taken)?;
            ensure!(p.mcp.len() <= 16, "At most 16 MCP servers per profile.");
            let mut names = BTreeSet::new();
            for m in &p.mcp {
                let name = match m {
                    Mcp::Catalog(n) => n,
                    Mcp::Server(ServerDecl { name, url }) => {
                        ensure!(
                            profile::valid_mcp_url(url),
                            "Repository MCP server urls must be https without credentials or fragment."
                        );
                        name
                    }
                };
                ensure!(
                    profile::valid_name(name)
                        && !name.starts_with("enginebox-")
                        && names.insert(name.clone()),
                    "MCP names must be distinct connection-style names."
                );
            }
            ensure!(p.ci.len() <= 16, "At most 16 CI jobs per profile.");
            for job in &p.ci {
                ensure!(
                    valid_profile_name(&job.name),
                    "CI job names match ^[a-z][a-z0-9-]{{0,31}}$."
                );
                argv(&job.command, "ci command")?;
            }
        }
        Ok(config)
    }
    /// The profile a run uses: the named one, else the default.
    pub fn select(&self, name: Option<&str>) -> Result<Option<(&str, &AgentProfile)>> {
        match name {
            Some(n) => self
                .profiles
                .get_key_value(n)
                .map(|(k, v)| Some((k.as_str(), v)))
                .with_context(|| format!("The repository declares no agent profile {n}.")),
            None => Ok(self
                .profiles
                .iter()
                .find(|(_, p)| p.default)
                .map(|(k, v)| (k.as_str(), v))),
        }
    }
}

/// What one requirement asks for: `kind` is `secret`, `mcp` (a catalog
/// connection) or `mcp_server` (a repository-declared server, qualified by
/// its URL).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Need {
    pub kind: &'static str,
    pub name: String,
    pub qualifier: String,
    pub run_kinds: Vec<String>,
    pub required: bool,
    pub url: Option<String>,
}
pub fn needs(profile: &AgentProfile) -> Vec<Need> {
    let mut out: Vec<Need> = profile
        .secrets
        .iter()
        .map(|r| Need {
            kind: "secret",
            name: r.name.clone(),
            qualifier: r.qualifier.clone(),
            run_kinds: r.run_kinds.clone(),
            required: r.required,
            url: None,
        })
        .collect();
    for m in &profile.mcp {
        out.push(match m {
            Mcp::Catalog(name) => Need {
                kind: "mcp",
                name: name.clone(),
                qualifier: "connection".into(),
                run_kinds: Vec::new(),
                required: false,
                url: None,
            },
            Mcp::Server(ServerDecl { name, url }) => Need {
                kind: "mcp_server",
                name: name.clone(),
                qualifier: url.clone(),
                run_kinds: Vec::new(),
                required: false,
                url: Some(url.clone()),
            },
        });
    }
    out
}

/// An approved (or proposed) mapping: repository + repository profile +
/// requirement -> what satisfies it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mapping {
    pub id: String,
    pub org_id: String,
    pub repository: String,
    pub repo_profile: String,
    pub kind: String,
    pub requirement: String,
    /// The qualifier (for `mcp_server`, the URL) this mapping was made for.
    pub qualifier: String,
    /// The catalog connection that satisfies it (`secret`: a secret source;
    /// `mcp`: an MCP connection). None for an approved `mcp_server`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<String>,
    /// `secret`: the value's name in the source (default: the requirement's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_name: Option<String>,
    pub status: String,
    pub proposed_by: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_at: Option<String>,
}

/// A repository config the controller has been given for one commit.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub repository: String,
    pub org_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    pub config: Value,
    pub registered_by: String,
    pub registered_at: String,
}

#[derive(Clone)]
pub struct RepoRequirements {
    root: PathBuf,
    mappings: Arc<Mutex<BTreeMap<String, Mapping>>>,
    /// repository -> (the head last read from its source, when).
    checked: Arc<Mutex<BTreeMap<String, (String, std::time::Instant)>>>,
}

/// Who registers a config read from the repository itself.
pub const SOURCE_ACTOR: &str = "repository";
/// How long a source read stands before the next run reads the head again.
const SOURCE_RECHECK: std::time::Duration = std::time::Duration::from_secs(30);

/// What the repository requirements contribute to one run.
#[derive(Default, Debug)]
pub struct Granted {
    pub env: BTreeMap<String, String>,
    pub catalog_mcp: Vec<String>,
    pub servers: Vec<profile::McpServer>,
    pub record: Value,
}

fn file_key(repository: &str) -> String {
    repository.replace('/', "__")
}

impl RepoRequirements {
    pub fn open(root: &Path) -> Result<Self> {
        let root = root.join("repo-config");
        state::private_dir(&root)?;
        state::private_dir(&root.join("mappings"))?;
        state::private_dir(&root.join("snapshots"))?;
        let mut mappings = BTreeMap::new();
        for path in state::json_files(&root.join("mappings"))? {
            let m: Mapping = serde_json::from_value(state::read_json(&path)?)?;
            ensure!(
                path.file_stem().and_then(|s| s.to_str()) == Some(m.id.as_str()),
                "Invalid repository mapping record"
            );
            mappings.insert(m.id.clone(), m);
        }
        Ok(Self {
            root,
            mappings: Arc::new(Mutex::new(mappings)),
            checked: Arc::default(),
        })
    }
    pub fn snapshot(&self, repository: &str) -> Option<Snapshot> {
        state::read_json(&self.snapshot_path(repository))
            .ok()
            .and_then(|v| serde_json::from_value(v).ok())
    }
    /// Bring the registered config up to date with the repository's default
    /// branch head before a run (`read` reads it, given the head already
    /// known). A failure keeps the last registered config and is logged
    /// without detail that could carry a token. A config removed from the
    /// repository unregisters one that came from it; one registered through
    /// the API stays until the repository has its own.
    pub fn refresh_from_source(
        &self,
        org: &str,
        repository: &str,
        read: impl FnOnce(Option<&str>) -> Result<crate::repo_source::Head>,
    ) {
        use crate::repo_source::Head;
        let known = {
            let checked = self.checked.lock().unwrap();
            match checked.get(repository) {
                Some((_, at)) if at.elapsed() < SOURCE_RECHECK => return,
                Some((commit, _)) => Some(commit.clone()),
                None => self
                    .snapshot(repository)
                    .filter(|s| s.org_id == org && s.registered_by == SOURCE_ACTOR)
                    .and_then(|s| s.commit),
            }
        };
        let outcome = read(known.as_deref()).and_then(|head| {
            let commit = match head {
                Head::Unchanged => known.clone().unwrap_or_default(),
                Head::Fetched {
                    commit,
                    config: Some(config),
                } => {
                    self.register(
                        org,
                        SOURCE_ACTOR,
                        repository,
                        &json!({"commit": commit, "config": config}),
                    )?;
                    commit
                }
                Head::Fetched {
                    commit,
                    config: None,
                } => {
                    if self
                        .snapshot(repository)
                        .is_some_and(|s| s.registered_by == SOURCE_ACTOR)
                    {
                        std::fs::remove_file(self.snapshot_path(repository))?;
                    }
                    commit
                }
            };
            self.checked
                .lock()
                .unwrap()
                .insert(repository.to_owned(), (commit, std::time::Instant::now()));
            Ok(())
        });
        if let Err(error) = outcome {
            // Retry on the next run only after the recheck interval.
            self.checked.lock().unwrap().insert(
                repository.to_owned(),
                (known.unwrap_or_default(), std::time::Instant::now()),
            );
            eprintln!(
                "{}",
                json!({"event":"enginebox.repo_config.source_read_failed","repository":repository,"reason":error.to_string().chars().take(200).collect::<String>()})
            );
        }
    }
    fn snapshot_path(&self, repository: &str) -> PathBuf {
        self.root
            .join("snapshots")
            .join(format!("{}.json", file_key(repository)))
    }
    /// Record the config of `repository` at `commit` (validated). Who sends
    /// it does not matter for security: a config only declares.
    pub fn register(
        &self,
        org: &str,
        actor: &str,
        repository: &str,
        body: &Value,
    ) -> Result<Value> {
        crate::keys(body, &["commit", "config"], &["config"])?;
        let commit = body.get("commit").and_then(Value::as_str);
        if let Some(c) = commit {
            ensure!(
                (7..=64).contains(&c.len()) && c.bytes().all(|b| b.is_ascii_hexdigit()),
                "commit must be a hexadecimal commit ID."
            );
        }
        ensure!(
            body["config"].to_string().len() <= 65536,
            "The repository config is at most 64 KiB."
        );
        RepoConfig::parse(&body["config"])?;
        let snapshot = Snapshot {
            repository: repository.to_owned(),
            org_id: org.to_owned(),
            commit: commit.map(str::to_owned),
            config: body["config"].clone(),
            registered_by: actor.to_owned(),
            registered_at: chrono::Utc::now().to_rfc3339(),
        };
        state::write_json(
            &self.snapshot_path(repository),
            &serde_json::to_value(&snapshot)?,
        )?;
        Ok(json!({"registered": repository, "commit": snapshot.commit}))
    }
    fn find(
        &self,
        org: &str,
        repository: &str,
        repo_profile: &str,
        kind: &str,
        requirement: &str,
    ) -> Vec<Mapping> {
        self.mappings
            .lock()
            .unwrap()
            .values()
            .filter(|m| {
                m.org_id == org
                    && m.repository == repository
                    && m.repo_profile == repo_profile
                    && m.kind == kind
                    && m.requirement == requirement
            })
            .cloned()
            .collect()
    }
    /// `met`, `needs_reapproval`, `proposed` or `unmet` for one need.
    fn status(
        &self,
        org: &str,
        repository: &str,
        repo_profile: &str,
        need: &Need,
    ) -> (String, Option<Mapping>) {
        let found = self.find(org, repository, repo_profile, need.kind, &need.name);
        if let Some(m) = found.iter().find(|m| m.status == "approved") {
            let status = if m.qualifier == need.qualifier {
                "met"
            } else {
                "needs_reapproval"
            };
            return (status.into(), Some(m.clone()));
        }
        match found.into_iter().find(|m| m.status == "proposed") {
            Some(m) => ("proposed".into(), Some(m)),
            None => ("unmet".into(), None),
        }
    }
    /// `GET .../requirements`: every declared requirement and its status.
    pub fn report(&self, org: &str, repository: &str) -> Result<Value> {
        let Some(snapshot) = self.snapshot(repository).filter(|s| s.org_id == org) else {
            return Ok(json!({"repository": repository, "configured": false, "profiles": {}}));
        };
        let config = RepoConfig::parse(&snapshot.config)?;
        let row = |profile: &str, need: &Need| {
            let (status, mapping) = self.status(org, repository, profile, need);
            json!({
                "kind": need.kind,
                "name": need.name,
                "qualifier": need.qualifier,
                "run_kinds": need.run_kinds,
                "required": need.required,
                "status": status,
                "mapping": mapping,
            })
        };
        let mut profiles = serde_json::Map::new();
        for (name, p) in &config.profiles {
            profiles.insert(
                name.clone(),
                json!({
                    "default": p.default,
                    "description": p.description,
                    "template": p.template,
                    "requirements": needs(p).iter().map(|n| row(name, n)).collect::<Vec<_>>(),
                }),
            );
        }
        let dev: Vec<Value> = config
            .dev_secrets
            .iter()
            .map(|r| {
                row(
                    DEV_PROFILE,
                    &Need {
                        kind: "secret",
                        name: r.name.clone(),
                        qualifier: r.qualifier.clone(),
                        run_kinds: r.run_kinds.clone(),
                        required: r.required,
                        url: None,
                    },
                )
            })
            .collect();
        Ok(json!({
            "repository": repository,
            "configured": true,
            "schema_version": config.schema_version,
            "commit": snapshot.commit,
            "registered_at": snapshot.registered_at,
            "profiles": profiles,
            "dev": dev,
        }))
    }
    fn declared(
        &self,
        org: &str,
        repository: &str,
        repo_profile: &str,
        kind: &str,
        requirement: &str,
    ) -> Result<Need> {
        let snapshot = self
            .snapshot(repository)
            .filter(|s| s.org_id == org)
            .context("Register the repository's config first.")?;
        let config = RepoConfig::parse(&snapshot.config)?;
        let list = if repo_profile == DEV_PROFILE {
            config
                .dev_secrets
                .iter()
                .map(|r| Need {
                    kind: "secret",
                    name: r.name.clone(),
                    qualifier: r.qualifier.clone(),
                    run_kinds: r.run_kinds.clone(),
                    required: r.required,
                    url: None,
                })
                .collect()
        } else {
            needs(config.profiles.get(repo_profile).with_context(|| {
                format!("The repository declares no agent profile {repo_profile}.")
            })?)
        };
        list.into_iter()
            .find(|n| n.kind == kind && n.name == requirement)
            .with_context(|| {
                format!("Profile {repo_profile} declares no {kind} requirement {requirement}.")
            })
    }
    /// Propose (`approve = false`, any caller with access to the repository)
    /// or approve (`approve = true`, owners and admins only) a mapping for
    /// a requirement as the repository currently declares it.
    pub fn map(
        &self,
        org: &str,
        actor: &str,
        repository: &str,
        body: &Value,
        approve: bool,
        connections: &Connections,
    ) -> Result<Value> {
        crate::keys(
            body,
            &[
                "repo_profile",
                "kind",
                "requirement",
                "connection",
                "source_name",
            ],
            &["repo_profile", "requirement"],
        )?;
        let repo_profile = crate::string(body, "repo_profile")?;
        let kind = body.get("kind").and_then(Value::as_str).unwrap_or("secret");
        let requirement = crate::string(body, "requirement")?;
        let need = self.declared(org, repository, repo_profile, kind, requirement)?;
        let connection = body
            .get("connection")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let source_name = body
            .get("source_name")
            .and_then(Value::as_str)
            .map(str::to_owned);
        match need.kind {
            "secret" => {
                let c = connection
                    .as_deref()
                    .context("A secret requirement maps to a secret_source connection.")?;
                let record = connections
                    .get(org, c)
                    .filter(|r| r.kind == crate::connections::Kind::SecretSource)
                    .with_context(|| format!("Unknown secret source {c}."))?;
                let value_name = source_name.clone().unwrap_or_else(|| need.name.clone());
                ensure!(native::env_key_allowed(&value_name), "Invalid source_name.");
                if record.source.as_ref().map(|s| s.provider)
                    == Some(crate::connections::SourceProvider::Kitbash)
                {
                    ensure!(
                        record.names.contains(&value_name),
                        "Secret source {c} does not yield {value_name}."
                    );
                }
            }
            "mcp" => {
                let c = connection.as_deref().unwrap_or(&need.name);
                let record = connections
                    .get(org, c)
                    .with_context(|| format!("Unknown connection {c}."))?;
                ensure!(
                    matches!(
                        record.kind,
                        crate::connections::Kind::McpHttp | crate::connections::Kind::McpStdio
                    ),
                    "An MCP requirement maps to an MCP connection."
                );
                ensure!(source_name.is_none(), "source_name applies to secrets.");
            }
            _ => ensure!(
                connection.is_none() && source_name.is_none(),
                "A repository-declared server is approved as declared; it maps to no connection."
            ),
        }
        let now = chrono::Utc::now().to_rfc3339();
        let mut mappings = self.mappings.lock().unwrap();
        ensure!(mappings.len() < MAX_MAPPINGS, "Mapping limit reached.");
        // One approved mapping per requirement; a proposal never replaces it.
        let existing: Vec<Mapping> = mappings
            .values()
            .filter(|m| {
                m.org_id == org
                    && m.repository == repository
                    && m.repo_profile == repo_profile
                    && m.kind == need.kind
                    && m.requirement == need.name
            })
            .cloned()
            .collect();
        if approve {
            for m in &existing {
                let _ =
                    std::fs::remove_file(self.root.join("mappings").join(format!("{}.json", m.id)));
                mappings.remove(&m.id);
            }
        } else {
            for m in existing.iter().filter(|m| m.status == "proposed") {
                let _ =
                    std::fs::remove_file(self.root.join("mappings").join(format!("{}.json", m.id)));
                mappings.remove(&m.id);
            }
        }
        let mapping = Mapping {
            id: uuid::Uuid::new_v4().simple().to_string(),
            org_id: org.to_owned(),
            repository: repository.to_owned(),
            repo_profile: repo_profile.to_owned(),
            kind: need.kind.to_owned(),
            requirement: need.name.clone(),
            qualifier: need.qualifier.clone(),
            connection: match need.kind {
                "mcp" => Some(connection.unwrap_or_else(|| need.name.clone())),
                _ => connection,
            },
            source_name,
            status: if approve { "approved" } else { "proposed" }.into(),
            proposed_by: actor.to_owned(),
            created_at: now.clone(),
            approved_by: approve.then(|| actor.to_owned()),
            approved_at: approve.then_some(now),
        };
        state::write_json(
            &self
                .root
                .join("mappings")
                .join(format!("{}.json", mapping.id)),
            &serde_json::to_value(&mapping)?,
        )?;
        mappings.insert(mapping.id.clone(), mapping.clone());
        Ok(json!({"mapping": mapping}))
    }
    /// Remove every mapping (approved or proposed) of one requirement.
    pub fn revoke(&self, org: &str, repository: &str, body: &Value) -> Result<Value> {
        crate::keys(
            body,
            &["repo_profile", "kind", "requirement"],
            &["repo_profile", "requirement"],
        )?;
        let repo_profile = crate::string(body, "repo_profile")?;
        let kind = body.get("kind").and_then(Value::as_str).unwrap_or("secret");
        let requirement = crate::string(body, "requirement")?;
        let mut mappings = self.mappings.lock().unwrap();
        let ids: Vec<String> = mappings
            .values()
            .filter(|m| {
                m.org_id == org
                    && m.repository == repository
                    && m.repo_profile == repo_profile
                    && m.kind == kind
                    && m.requirement == requirement
            })
            .map(|m| m.id.clone())
            .collect();
        ensure!(!ids.is_empty(), "No mapping for that requirement.");
        for id in &ids {
            let _ = std::fs::remove_file(self.root.join("mappings").join(format!("{id}.json")));
            mappings.remove(id);
        }
        Ok(json!({"revoked": ids.len()}))
    }
    /// What a run on `repository` gets from its repository profile.
    /// `Ok(None)` when the repository has no registered config or declares
    /// no agent profiles. A required requirement without an approved
    /// mapping (for this run kind) refuses the run.
    pub fn grant(
        &self,
        org: &str,
        repository: &str,
        repo_profile: Option<&str>,
        run_kind: Option<&str>,
        has_profile: bool,
        connections: &Connections,
    ) -> Result<Option<Granted>> {
        let Some(snapshot) = self.snapshot(repository).filter(|s| s.org_id == org) else {
            ensure!(
                repo_profile.is_none(),
                "The repository has no registered config with agent profiles."
            );
            return Ok(None);
        };
        let config = RepoConfig::parse(&snapshot.config)?;
        let Some((name, profile)) = config.select(repo_profile)? else {
            ensure!(
                repo_profile.is_none(),
                "The repository declares no agent profiles."
            );
            return Ok(None);
        };
        let mut granted = Granted {
            env: profile.env.clone(),
            ..Default::default()
        };
        let mut met = Vec::new();
        let mut unmet = Vec::new();
        for need in needs(profile) {
            if run_kind.is_some_and(|k| {
                !need.run_kinds.is_empty() && !need.run_kinds.iter().any(|r| r == k)
            }) || (run_kind.is_none() && !need.run_kinds.is_empty())
            {
                continue;
            }
            let (status, mapping) = self.status(org, repository, name, &need);
            let outcome: Result<()> = (|| {
                ensure!(status == "met", "{status}");
                let mapping = mapping.context("unmet")?;
                match need.kind {
                    "secret" => {
                        let source = mapping.connection.as_deref().context("unmet")?;
                        let value_name = mapping.source_name.as_deref().unwrap_or(&need.name);
                        let value = connections
                            .source_value(org, source, value_name)
                            .map_err(|_| anyhow::anyhow!("source_unavailable"))?;
                        granted.env.insert(need.name.clone(), value);
                    }
                    "mcp" => {
                        ensure!(has_profile, "needs_profile");
                        granted
                            .catalog_mcp
                            .push(mapping.connection.clone().context("unmet")?);
                    }
                    _ => {
                        ensure!(has_profile, "needs_profile");
                        granted.servers.push(profile::McpServer {
                            name: need.name.clone(),
                            command: None,
                            args: Vec::new(),
                            env_vars: Vec::new(),
                            url: need.url.clone(),
                            headers_from_env: BTreeMap::new(),
                        });
                    }
                }
                Ok(())
            })();
            match outcome {
                Ok(()) => met.push(json!({"kind": need.kind, "name": need.name})),
                Err(reason) => {
                    ensure!(
                        !need.required,
                        "Repository profile {name} requires {} {}, which is {reason}; an owner or admin must approve a mapping.",
                        need.kind,
                        need.name
                    );
                    unmet.push(
                        json!({"kind": need.kind, "name": need.name, "reason": reason.to_string()}),
                    );
                }
            }
        }
        granted.record = json!({
            "name": name,
            "commit": snapshot.commit,
            "met": met,
            "unmet": unmet,
        });
        Ok(Some(granted))
    }
}

/// Org check helper: repositories are `<project>/<name>`.
pub fn repository_ref(project: &str, name: &str) -> String {
    format!("{project}/{name}")
}
/// True when `org` may act on a repository of `owner`.
pub fn same_org(org: &str, owner: Option<String>) -> bool {
    owner.as_deref().unwrap_or(pools::DEFAULT_ORG) == org
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The Cloudflare Workers example from docs/PROJECT-CONFIG.md.
    pub(crate) fn workers_config() -> Value {
        json!({
          "schema_version": 2,
          "dev": {"template": "./templates/workers-dev.json",
                  "secrets": [{"name": "CLOUDFLARE_API_TOKEN", "qualifier": "read-only"}]},
          "agents": {"profiles": {
            "default": {"default": true, "template": "native-dev", "size": "recommended",
                        "setup": ["pnpm", "install", "--frozen-lockfile"], "dev": "dev", "test": ["pnpm", "test"],
                        "env": {"NODE_ENV": "development"}, "mcp": ["kitbash"],
                        "secrets": [{"name": "DATABASE_URL", "qualifier": "staging", "run_kinds": ["builder"]}]},
            "debug": {"description": "Read-only Cloudflare and database access.", "template": "native-dev",
                      "mcp": ["cf-readonly", {"name": "docs", "url": "https://developers.cloudflare.com/mcp"}],
                      "secrets": [{"name": "CLOUDFLARE_API_TOKEN", "qualifier": "read-only"},
                                  {"name": "DATABASE_URL", "qualifier": "staging-read-only"}]},
            "deploy": {"template": "native-dev", "test": ["pnpm", "test"],
                       "secrets": [{"name": "CLOUDFLARE_API_TOKEN", "qualifier": "deploy", "run_kinds": ["deploy"], "required": true}],
                       "ci": [{"name": "deploy", "command": ["pnpm", "wrangler", "deploy"]}]}
          }}
        })
    }

    #[test]
    fn parses_v1_and_v2_and_refuses_values_and_bad_shapes() {
        assert_eq!(
            RepoConfig::parse(&json!({"schema_version":1,"dev":{"template":"web@1"}}))
                .unwrap()
                .schema_version,
            1
        );
        let config = RepoConfig::parse(&workers_config()).unwrap();
        assert_eq!(config.select(None).unwrap().unwrap().0, "default");
        assert_eq!(config.select(Some("debug")).unwrap().unwrap().0, "debug");
        assert!(config.select(Some("nope")).is_err());
        assert_eq!(config.dev_secrets[0].name, "CLOUDFLARE_API_TOKEN");
        let mut two_defaults = workers_config();
        two_defaults["agents"]["profiles"]["debug"]["default"] = json!(true);
        let mut value = workers_config();
        value["agents"]["profiles"]["debug"]["secrets"][0]["value"] = json!("leak");
        let mut header = workers_config();
        header["agents"]["profiles"]["debug"]["mcp"][1]["headers_from_env"] =
            json!({"Authorization":"DATABASE_URL"});
        let mut denied = workers_config();
        denied["agents"]["profiles"]["default"]["env"] = json!({"PATH": "/tmp"});
        let mut clash = workers_config();
        clash["agents"]["profiles"]["default"]["env"] = json!({"DATABASE_URL": "x"});
        let mut no_qualifier = workers_config();
        no_qualifier["agents"]["profiles"]["deploy"]["secrets"][0]
            .as_object_mut()
            .unwrap()
            .remove("qualifier");
        for bad in [
            two_defaults,
            value,
            header,
            denied,
            clash,
            no_qualifier,
            json!({"schema_version":1,"agents":{}}),
            json!({"schema_version":3}),
        ] {
            assert!(RepoConfig::parse(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_source_head_registers_unregisters_and_survives_failures() {
        use crate::repo_source::Head;
        let t = tempfile::tempdir().unwrap();
        let reqs = RepoRequirements::open(t.path()).unwrap();
        let repo = "alpha/site";
        let fetched = |commit: &str, config: Option<Value>| {
            let commit = commit.to_owned();
            move |_: Option<&str>| Ok(Head::Fetched { commit, config })
        };
        reqs.refresh_from_source("o", repo, fetched("aaaaaaa", Some(workers_config())));
        let s = reqs.snapshot(repo).unwrap();
        assert_eq!(
            (s.commit.as_deref(), s.registered_by.as_str()),
            (Some("aaaaaaa"), SOURCE_ACTOR)
        );
        // Within the recheck interval the head is not read again.
        reqs.refresh_from_source("o", repo, |_| panic!("read again"));
        // A failed read keeps the registered config.
        reqs.checked.lock().unwrap().clear();
        reqs.refresh_from_source("o", repo, |known| {
            assert_eq!(known, Some("aaaaaaa"));
            anyhow::bail!("offline")
        });
        assert!(reqs.snapshot(repo).is_some());
        // The file removed from the repository unregisters it.
        reqs.checked.lock().unwrap().clear();
        reqs.refresh_from_source("o", repo, fetched("bbbbbbb", None));
        assert!(reqs.snapshot(repo).is_none());
        // A config registered through the API stays while the repository has none.
        reqs.register("o", "owner", repo, &json!({"config": workers_config()}))
            .unwrap();
        reqs.checked.lock().unwrap().clear();
        reqs.refresh_from_source("o", repo, fetched("ccccccc", None));
        assert_eq!(reqs.snapshot(repo).unwrap().registered_by, "owner");
    }
}
