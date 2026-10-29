//! Workspace profiles: an owner-managed, named description of how one native
//! CLI runs (turn limit, permission profile, tools, plugins, skills, MCP
//! servers, setting sources, default env and an optional template).
//!
//! The safe flag table lives here once. A profile can never name a CLI flag:
//! it picks a `permission_profile` from a fixed enum, and every other field is
//! data that this module translates into the CLI's own documented options.
//! Plugins and skills are content-addressed files (path + sha256) that the
//! controller stores and delivers; nothing is fetched from a URL at run time.
use crate::{native, security};
use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

/// Highest turn limit a profile may set (Claude `--max-turns`).
pub const MAX_TURNS: u32 = 500;
/// Turn limit used when a Claude profile does not set one (today's behaviour).
pub const DEFAULT_MAX_TURNS: u32 = 20;
/// Largest stored profile file.
pub const MAX_FILE_BYTES: usize = 1024 * 1024;
/// Most files one provider block may materialize, and their total size.
pub const MAX_FILES: usize = 256;
pub const MAX_BUNDLE_BYTES: usize = 8 * 1024 * 1024;
const MAX_BUNDLES: usize = 32;
const MAX_MCP_SERVERS: usize = 32;
const MAX_TOOLS: usize = 64;
const MAX_ARGS: usize = 32;
/// Owner of every profile until tenants exist. Kept on the record so tenant
/// ownership is an additive change.
pub const OWNER: &str = "owner";
/// Tools today's fixed Claude policy preapproves (the `edit` default).
pub const CLAUDE_EDIT_TOOLS: &str = "Read,Write,Edit,Bash,mcp__enginebox-browser__*";
/// Tools a `read_only` Claude profile preapproves by default.
pub const CLAUDE_READ_ONLY_TOOLS: &str = "Read,Glob,Grep";
/// Tools refused in a `read_only` profile (and denied explicitly on argv).
const CLAUDE_WRITE_TOOLS: &[&str] = &["Write", "Edit", "MultiEdit", "NotebookEdit", "Bash"];
/// Where a run's materialized profile lives, relative to the native home.
pub const RUNS_DIR: &str = ".local/state/enginebox/agent-runs/profiles";
/// Codex features that reach past the sandbox or the profile's MCP boundary
/// (apps/connectors, computer and browser use, remote plugins, installing a
/// skill's MCP dependencies, tool suggestions). Off in every generated config.
pub const CODEX_DISABLED_FEATURES: &[&str] = &[
    "apps",
    "browser_use",
    "browser_use_external",
    "browser_use_full_cdp_access",
    "computer_use",
    "plugins",
    "remote_plugin",
    "plugin_sharing",
    "skill_mcp_dependency_install",
    "tool_suggest",
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionProfile {
    /// Today's behaviour: edits inside the workspace, sandboxed commands.
    #[default]
    Edit,
    /// No edits: Claude preapproves only read tools and denies the rest,
    /// Codex runs in its read-only sandbox, Cursor runs in ask mode.
    ReadOnly,
}

/// One content-addressed file: where it lands inside its bundle, and its digest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileRef {
    pub path: String,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub executable: bool,
}

/// A plugin or skill: a named directory of content-addressed files.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bundle {
    pub name: String,
    pub files: Vec<FileRef>,
}

/// One MCP server: exactly one of `command` (stdio) or `url` (streamable HTTP).
/// Header and variable values always come from the run's environment, never
/// from the profile itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServer {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Variable names passed through to a stdio server.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_vars: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// HTTP header name -> the variable that carries its value.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers_from_env: BTreeMap<String, String>,
}

/// How one provider's CLI runs under a profile.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderProfile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub permission_profile: PermissionProfile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plugins: Vec<Bundle>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<Bundle>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<McpServer>,
    /// Use only the profile's MCP servers, not the workspace's own.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub strict_mcp: bool,
    /// Codex only, with the edit profile: let sandboxed commands reach the
    /// network (git clone/push to the run's repository, package installs).
    /// Off by default; the workspace sandbox otherwise has no network.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub network_access: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setting_sources: Option<Vec<String>>,
    /// Default variables; a run's own `env` wins on a name clash. Values are
    /// write-only: public views list names.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

/// A named profile record.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub name: String,
    #[serde(default = "owner")]
    pub owner: String,
    /// The org that owns the profile. Absent (and never written) for the
    /// platform org, so records stay readable by releases before orgs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_id: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Template the workspace must have been created from (for example one
    /// with a game engine installed). Checked at launch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    pub providers: BTreeMap<String, ProviderProfile>,
    /// Org catalog connections (docs/CONNECTIONS.md) by name, resolved at
    /// launch into each provider's MCP config and the run's env. Omitted when
    /// empty so profiles without connections stay readable by older releases.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connections: Vec<String>,
    /// Run kind (or `*`) -> the subset of `connections` that applies to runs
    /// of that kind. A kind not listed uses `*`, else every connection.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub connection_policy: BTreeMap<String, Vec<String>>,
    /// HMAC key for run callbacks started by the owner with this profile.
    /// Write-only: never returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_secret: Option<String>,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
}
impl std::fmt::Debug for Profile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Profile")
            .field("name", &self.name)
            .field("revision", &self.revision)
            .finish()
    }
}
fn owner() -> String {
    OWNER.into()
}

/// Most connections one profile may reference.
pub const MAX_CONNECTIONS: usize = 32;
/// A run kind as a start request names it (`planner`, `builder`, `debug`...).
pub fn valid_run_kind(kind: &str) -> bool {
    regex::Regex::new(r"^[a-z][a-z0-9_-]{0,31}$")
        .unwrap()
        .is_match(kind)
}
/// The connections that apply to a run of `run_kind` (docs/CONNECTIONS.md,
/// "Policy per kind of run"). No kind, or a kind the policy does not list and
/// no `*` entry: every connection of the profile.
pub fn connections_for<'a>(
    connections: &'a [String],
    policy: &'a BTreeMap<String, Vec<String>>,
    run_kind: Option<&str>,
) -> &'a [String] {
    match run_kind.and_then(|k| policy.get(k).or_else(|| policy.get("*"))) {
        Some(subset) => subset,
        None => connections,
    }
}

pub fn valid_name(name: &str) -> bool {
    regex::Regex::new(r"^[a-z][a-z0-9-]{0,62}$")
        .unwrap()
        .is_match(name)
}
pub fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}
/// Callback HMAC keys: 32-256 printable ASCII characters without spaces.
pub fn valid_secret(secret: &str) -> bool {
    (32..=256).contains(&secret.len()) && secret.bytes().all(|b| (33..=126).contains(&b))
}
fn valid_bundle_name(name: &str) -> bool {
    regex::Regex::new(r"^[a-z0-9][a-z0-9._-]{0,63}$")
        .unwrap()
        .is_match(name)
}
/// A relative path of plain components; no `.`/`..`, no absolute or hidden tricks.
fn valid_file_path(path: &str) -> bool {
    let component = regex::Regex::new(r"^[A-Za-z0-9_.+-]{1,128}$").unwrap();
    !path.is_empty()
        && path.len() <= 512
        && path.split('/').count() <= 16
        && path
            .split('/')
            .all(|c| component.is_match(c) && c != "." && c != "..")
}
fn valid_tool(tool: &str) -> bool {
    // A bare tool name with an optional argument pattern, or an MCP tool
    // (wildcard allowed). Never starts with `-`, so it can never be a flag.
    let lower = tool.to_ascii_lowercase();
    regex::Regex::new(r"^(mcp__[A-Za-z0-9_-]{1,64}__(\*|[A-Za-z0-9_-]{1,128})|[A-Z][A-Za-z0-9]{0,63}(\([^()\x00-\x1f]{1,200}\))?)$")
        .unwrap()
        .is_match(tool)
        && !lower.contains("dangerous")
        && !lower.contains("bypass")
        && !lower.contains("yolo")
}
fn bare_tool(tool: &str) -> &str {
    tool.split('(').next().unwrap_or(tool)
}
pub fn valid_mcp_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    let local = matches!(parsed.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    url.len() <= 2048
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.fragment().is_none()
        && parsed.host_str().is_some()
        && (parsed.scheme() == "https" || (parsed.scheme() == "http" && local))
}

impl Profile {
    /// Validate the record. `file_exists` answers whether a digest is stored.
    pub fn validate(&self, file_exists: &dyn Fn(&str) -> bool) -> Result<()> {
        ensure!(
            valid_name(&self.name),
            "Profile name must match ^[a-z][a-z0-9-]{{0,62}}$."
        );
        ensure!(self.owner == OWNER, "Profiles are owner-managed.");
        if let Some(org) = &self.org_id {
            ensure!(
                crate::pools::valid_org(org) && org != crate::pools::DEFAULT_ORG,
                "org_id must be an org ID other than the platform org (omit it for the platform org)."
            );
        }
        ensure!(
            self.description.len() <= 512 && !self.description.chars().any(char::is_control),
            "description must be at most 512 printable bytes."
        );
        if let Some(template) = &self.template {
            enginebox_core::identifier(template, false)?;
        }
        if let Some(secret) = &self.callback_secret {
            ensure!(
                valid_secret(secret),
                "callback_secret must be 32-256 printable ASCII characters without spaces."
            );
        }
        ensure!(
            !self.providers.is_empty(),
            "providers must configure at least one native client."
        );
        ensure!(
            self.connections.len() <= MAX_CONNECTIONS,
            "connections accepts at most {MAX_CONNECTIONS} names."
        );
        let mut seen = BTreeSet::new();
        for name in &self.connections {
            ensure!(
                valid_name(name) && seen.insert(name.as_str()),
                "connections must be distinct connection names."
            );
        }
        ensure!(
            self.connection_policy.len() <= 32,
            "connection_policy accepts at most 32 run kinds."
        );
        for (kind, subset) in &self.connection_policy {
            ensure!(
                kind == "*" || valid_run_kind(kind),
                "connection_policy keys must be run kinds matching ^[a-z][a-z0-9_-]{{0,31}}$ or *."
            );
            let mut once = BTreeSet::new();
            ensure!(
                subset
                    .iter()
                    .all(|c| seen.contains(c.as_str()) && once.insert(c.as_str())),
                "connection_policy may list only the profile's own connections, once each."
            );
        }
        for (provider, block) in &self.providers {
            crate::provider(provider)?;
            block.validate(provider, file_exists)?;
        }
        Ok(())
    }
    /// Public view: never the callback secret or env values.
    pub fn public(&self) -> Value {
        let providers: serde_json::Map<String, Value> = self
            .providers
            .iter()
            .map(|(p, block)| {
                let mut v = serde_json::to_value(block).unwrap_or(Value::Null);
                if let Some(map) = v.as_object_mut() {
                    map.remove("env");
                    map.insert(
                        "env_keys".into(),
                        json!(block.env.keys().collect::<Vec<_>>()),
                    );
                }
                (p.clone(), v)
            })
            .collect();
        json!({
            "name":self.name,
            "owner":self.owner,
            "org_id":self.org_id.as_deref().unwrap_or(crate::pools::DEFAULT_ORG),
            "description":self.description,
            "template":self.template,
            "providers":providers,
            "connections":self.connections,
            "connection_policy":self.connection_policy,
            "has_callback_secret":self.callback_secret.is_some(),
            "revision":self.revision,
            "created_at":self.created_at,
            "updated_at":self.updated_at,
        })
    }
}

impl ProviderProfile {
    /// Every digest this block references, deduplicated.
    pub fn digests(&self) -> BTreeSet<String> {
        self.plugins
            .iter()
            .chain(&self.skills)
            .flat_map(|b| b.files.iter().map(|f| f.sha256.clone()))
            .collect()
    }
    /// Strict validation; errors name the rule, never a value.
    pub fn validate(&self, provider: &str, file_exists: &dyn Fn(&str) -> bool) -> Result<()> {
        crate::provider(provider)?;
        let unsupported =
            |field: &str| -> Result<()> { bail!("{provider} profiles do not support {field}.") };
        if let Some(turns) = self.max_turns {
            ensure!(
                (1..=MAX_TURNS).contains(&turns),
                "max_turns must be 1-{MAX_TURNS}."
            );
            if provider != "claude" {
                unsupported("max_turns; bound the run with timeout_seconds")?;
            }
        }
        if let Some(tools) = &self.allowed_tools {
            if provider != "claude" {
                unsupported("allowed_tools")?;
            }
            ensure!(
                !tools.is_empty() && tools.len() <= MAX_TOOLS,
                "allowed_tools must list 1-{MAX_TOOLS} tools."
            );
            let mut seen = BTreeSet::new();
            for tool in tools {
                ensure!(
                    valid_tool(tool),
                    "allowed_tools entries must be tool names such as Read, Bash(git status) or mcp__server__tool."
                );
                ensure!(seen.insert(tool), "allowed_tools must not repeat a tool.");
                if self.permission_profile == PermissionProfile::ReadOnly {
                    ensure!(
                        !CLAUDE_WRITE_TOOLS.contains(&bare_tool(tool)),
                        "read_only profiles cannot allow Write, Edit, MultiEdit, NotebookEdit or Bash."
                    );
                }
            }
        }
        if let Some(sources) = &self.setting_sources {
            if provider != "claude" {
                unsupported("setting_sources")?;
            }
            let unique: BTreeSet<_> = sources.iter().collect();
            ensure!(
                !sources.is_empty()
                    && unique.len() == sources.len()
                    && sources
                        .iter()
                        .all(|s| ["user", "project", "local"].contains(&s.as_str())),
                "setting_sources must list distinct values from user, project, local."
            );
        }
        if !self.skills.is_empty() && provider == "cursor" {
            unsupported("skills")?;
        }
        if !self.plugins.is_empty() && provider == "codex" {
            unsupported("plugins; use skills and mcp_servers")?;
        }
        if provider == "cursor" && (!self.mcp_servers.is_empty() || self.strict_mcp) {
            unsupported("mcp_servers or strict_mcp")?;
        }
        if self.network_access {
            if provider != "codex" {
                unsupported("network_access")?;
            }
            ensure!(
                self.permission_profile == PermissionProfile::Edit,
                "network_access needs the edit permission profile."
            );
        }
        ensure!(
            self.plugins.len() <= MAX_BUNDLES && self.skills.len() <= MAX_BUNDLES,
            "plugins and skills accept at most {MAX_BUNDLES} bundles each."
        );
        let mut names = BTreeSet::new();
        let mut files = 0;
        for (kind, bundle) in self
            .plugins
            .iter()
            .map(|b| ("plugin", b))
            .chain(self.skills.iter().map(|b| ("skill", b)))
        {
            ensure!(
                valid_bundle_name(&bundle.name),
                "{kind} names must match ^[a-z0-9][a-z0-9._-]{{0,63}}$."
            );
            ensure!(
                names.insert((kind, bundle.name.as_str())),
                "{kind} names must be unique."
            );
            ensure!(
                !bundle.files.is_empty(),
                "Each {kind} must contain at least one file."
            );
            let mut paths = BTreeSet::new();
            for file in &bundle.files {
                ensure!(
                    valid_file_path(&file.path),
                    "File paths must be relative paths of plain components."
                );
                ensure!(
                    paths.insert(file.path.as_str()),
                    "A bundle must not repeat a path."
                );
                // A file and a directory of the same name cannot coexist.
                ensure!(
                    !bundle
                        .files
                        .iter()
                        .any(|other| other.path.starts_with(&format!("{}/", file.path))),
                    "A bundle path cannot be both a file and a directory."
                );
                ensure!(
                    valid_sha256(&file.sha256),
                    "sha256 must be 64 lowercase hex characters."
                );
                ensure!(
                    file_exists(&file.sha256),
                    "Upload every referenced file before saving the profile."
                );
            }
            if kind == "skill" {
                ensure!(
                    paths.contains("SKILL.md"),
                    "Each skill must contain SKILL.md at its root."
                );
            }
            files += bundle.files.len();
        }
        ensure!(
            files <= MAX_FILES,
            "A provider block materializes at most {MAX_FILES} files."
        );
        ensure!(
            self.mcp_servers.len() <= MAX_MCP_SERVERS,
            "mcp_servers accepts at most {MAX_MCP_SERVERS} servers."
        );
        let mut servers = BTreeSet::new();
        let name_pattern = regex::Regex::new(r"^[A-Za-z0-9_-]{1,64}$")?;
        let command_pattern = regex::Regex::new(r"^[A-Za-z0-9_./+-]{1,256}$")?;
        let header_pattern = regex::Regex::new(r"^[A-Za-z0-9-]{1,64}$")?;
        for server in &self.mcp_servers {
            ensure!(
                name_pattern.is_match(&server.name) && servers.insert(server.name.as_str()),
                "MCP server names must be unique and match ^[A-Za-z0-9_-]{{1,64}}$."
            );
            ensure!(
                !server.name.starts_with("enginebox-"),
                "MCP server names starting with enginebox- are reserved."
            );
            match (&server.command, &server.url) {
                (Some(command), None) => {
                    ensure!(
                        command_pattern.is_match(command)
                            && !command.starts_with('-')
                            && !command.split('/').any(|c| c == ".."),
                        "MCP command must be a program name or absolute path."
                    );
                    ensure!(
                        server.args.len() <= MAX_ARGS
                            && server
                                .args
                                .iter()
                                .all(|a| a.len() <= 1024 && !a.contains('\0')),
                        "MCP args accept at most {MAX_ARGS} values of 1024 bytes without NUL."
                    );
                    ensure!(
                        server.headers_from_env.is_empty(),
                        "headers_from_env applies only to url servers."
                    );
                }
                (None, Some(url)) => {
                    ensure!(
                        valid_mcp_url(url),
                        "MCP url must be https, or http on localhost, without credentials or fragment."
                    );
                    ensure!(
                        server.args.is_empty() && server.env_vars.is_empty(),
                        "args and env_vars apply only to command servers."
                    );
                }
                _ => bail!("Each MCP server needs exactly one of command or url."),
            }
            ensure!(
                server.env_vars.len() <= MAX_ARGS
                    && server.env_vars.iter().all(|k| native::env_key_allowed(k)),
                "MCP env_vars must be names a run may set."
            );
            ensure!(
                server.headers_from_env.len() <= 16
                    && server
                        .headers_from_env
                        .iter()
                        .all(|(h, k)| header_pattern.is_match(h) && native::env_key_allowed(k)),
                "headers_from_env maps header names to variable names a run may set."
            );
        }
        native::validate_env_map(&self.env)?;
        Ok(())
    }
}

/// What the guest receives for a profile run: the request, the provider block
/// and every referenced file's bytes (base64), keyed by digest.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub request: native::RunRequest,
    pub profile: String,
    pub block: ProviderProfile,
    pub files: BTreeMap<String, String>,
}
/// Largest profile envelope the guest accepts on stdin.
pub const MAX_ENVELOPE_BYTES: usize = 12 * 1024 * 1024;

impl Envelope {
    /// Re-validate everything in the guest: the request, the block for the
    /// request's provider, and that the files are exactly the referenced ones
    /// with matching digests. Returns the decoded file bytes.
    pub fn validate(&self) -> Result<BTreeMap<String, Vec<u8>>> {
        use base64::Engine;
        self.request.validate()?;
        ensure!(valid_name(&self.profile), "Invalid profile name.");
        self.block
            .validate(&self.request.provider, &|sha| self.files.contains_key(sha))?;
        let wanted = self.block.digests();
        ensure!(
            self.files.keys().cloned().collect::<BTreeSet<_>>() == wanted,
            "Profile files do not match the profile."
        );
        let mut decoded = BTreeMap::new();
        let mut total = 0;
        for (sha, encoded) in &self.files {
            let bytes = base64::engine::general_purpose::STANDARD.decode(encoded)?;
            ensure!(
                bytes.len() <= MAX_FILE_BYTES && sha256_hex(&bytes) == *sha,
                "Profile file digest mismatch."
            );
            total += bytes.len();
            decoded.insert(sha.clone(), bytes);
        }
        ensure!(
            total <= MAX_BUNDLE_BYTES,
            "Profile files exceed {MAX_BUNDLE_BYTES} bytes."
        );
        merged_env(&self.block, &self.request)?;
        Ok(decoded)
    }
}

/// Profile defaults overlaid by the run's own variables, within run bounds.
pub fn merged_env(
    block: &ProviderProfile,
    request: &native::RunRequest,
) -> Result<BTreeMap<String, String>> {
    let mut env = block.env.clone();
    env.extend(request.env.clone());
    native::validate_env_map(&env)?;
    Ok(env)
}

/// A run's materialized profile: its private directory and what the CLI needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Materialized {
    pub root: PathBuf,
    pub plugin_dirs: Vec<PathBuf>,
    pub mcp_config: Option<PathBuf>,
    pub codex_home: Option<PathBuf>,
}

pub fn run_dir(home: &Path, run_id: &str) -> Result<PathBuf> {
    native::validate_run_id(run_id)?;
    Ok(home.join(RUNS_DIR).join(run_id))
}

/// Write the profile's files and configuration into this run's private
/// directory under the native home, and nowhere else. Codex additionally gets
/// a copy of the delegated access file and a link to the native session store
/// so `resume` keeps working.
pub fn materialize(
    home: &Path,
    run_id: &str,
    provider: &str,
    block: &ProviderProfile,
    files: &BTreeMap<String, Vec<u8>>,
) -> Result<Materialized> {
    crate::provider(provider)?;
    let result = materialize_inner(home, run_id, provider, block, files);
    if result.is_err() {
        let _ = remove_run_dir(home, run_id);
    }
    result
}

fn materialize_inner(
    home: &Path,
    run_id: &str,
    provider: &str,
    block: &ProviderProfile,
    files: &BTreeMap<String, Vec<u8>>,
) -> Result<Materialized> {
    let root = run_dir(home, run_id)?;
    security::safe_path(home, &format!("{RUNS_DIR}/{run_id}/placeholder"), true)?;
    remove_run_dir(home, run_id)?;
    security::private_dir(&root)?;
    let write = |relative: &str, bytes: &[u8], executable: bool| -> Result<()> {
        security::write_bytes(&root, relative, bytes, MAX_FILE_BYTES)?;
        #[cfg(unix)]
        if executable {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root.join(relative), std::fs::Permissions::from_mode(0o700))?;
        }
        #[cfg(not(unix))]
        let _ = executable;
        Ok(())
    };
    let put_bundle = |prefix: &str, bundle: &Bundle| -> Result<()> {
        for file in &bundle.files {
            let bytes = files
                .get(&file.sha256)
                .ok_or_else(|| anyhow::anyhow!("Profile file missing."))?;
            write(&format!("{prefix}/{}", file.path), bytes, file.executable)?;
        }
        Ok(())
    };
    let mut out = Materialized {
        root: root.clone(),
        plugin_dirs: vec![],
        mcp_config: None,
        codex_home: None,
    };
    match provider {
        "claude" => {
            for plugin in &block.plugins {
                put_bundle(&format!("plugins/{}", plugin.name), plugin)?;
                out.plugin_dirs
                    .push(root.join("plugins").join(&plugin.name));
            }
            if !block.skills.is_empty() {
                // Claude loads skills from a plugin; wrap them in one.
                write(
                    "skills-plugin/.claude-plugin/plugin.json",
                    &serde_json::to_vec_pretty(&json!({
                        "name":"profile-skills",
                        "description":"Skills from this run's Engine Box profile.",
                        "version":"1.0.0"
                    }))?,
                    false,
                )?;
                for skill in &block.skills {
                    put_bundle(&format!("skills-plugin/skills/{}", skill.name), skill)?;
                }
                out.plugin_dirs.push(root.join("skills-plugin"));
            }
            if !block.mcp_servers.is_empty() {
                write("mcp.json", &claude_mcp_config(block)?, false)?;
                out.mcp_config = Some(root.join("mcp.json"));
            }
        }
        "codex" => {
            let codex = root.join("codex-home");
            security::private_dir(&codex)?;
            for skill in &block.skills {
                put_bundle(&format!("codex-home/skills/{}", skill.name), skill)?;
            }
            let browser = !block.strict_mcp && codex_home_has_browser(home);
            write(
                "codex-home/config.toml",
                codex_config(block, browser).as_bytes(),
                false,
            )?;
            if let Some(auth) =
                security::read_bytes(home, ".codex/auth.json", security::LIMIT, false)?
            {
                write("codex-home/auth.json", &auth, false)?;
            }
            #[cfg(unix)]
            {
                let sessions = home.join(".codex/sessions");
                if std::fs::symlink_metadata(&sessions).is_ok_and(|m| m.is_dir()) {
                    std::os::unix::fs::symlink(&sessions, codex.join("sessions"))?;
                }
            }
            out.codex_home = Some(codex);
        }
        _ => {
            for plugin in &block.plugins {
                put_bundle(&format!("plugins/{}", plugin.name), plugin)?;
                out.plugin_dirs
                    .push(root.join("plugins").join(&plugin.name));
            }
        }
    }
    Ok(out)
}

/// Remove a run's materialized profile. Links are removed, never followed.
pub fn remove_run_dir(home: &Path, run_id: &str) -> Result<()> {
    let root = run_dir(home, run_id)?;
    security::check_components(&root)?;
    match std::fs::symlink_metadata(&root) {
        Ok(m) => {
            ensure!(
                m.is_dir() && !m.file_type().is_symlink(),
                "Invalid run profile directory."
            );
            std::fs::remove_dir_all(&root)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

fn claude_mcp_config(block: &ProviderProfile) -> Result<Vec<u8>> {
    let mut servers = serde_json::Map::new();
    for s in &block.mcp_servers {
        let entry = if let Some(command) = &s.command {
            let env: serde_json::Map<String, Value> = s
                .env_vars
                .iter()
                .map(|k| (k.clone(), json!(format!("${{{k}}}"))))
                .collect();
            json!({"type":"stdio","command":command,"args":s.args,"env":env})
        } else {
            let headers: serde_json::Map<String, Value> = s
                .headers_from_env
                .iter()
                .map(|(h, k)| (h.clone(), json!(format!("${{{k}}}"))))
                .collect();
            json!({"type":"http","url":s.url,"headers":headers})
        };
        servers.insert(s.name.clone(), entry);
    }
    Ok(serde_json::to_vec_pretty(
        &json!({"mcpServers":Value::Object(servers)}),
    )?)
}

fn toml_string(value: &str) -> String {
    toml::Value::String(value.into()).to_string()
}
fn toml_array(values: &[String]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(|v| toml_string(v))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// The generated CODEX_HOME config: sandbox from the permission profile, no
/// approvals, risky features and bundled skills off, and only the profile's
/// MCP servers (plus the workspace browser server unless `strict_mcp`).
pub fn codex_config(block: &ProviderProfile, browser: bool) -> String {
    let sandbox = match block.permission_profile {
        PermissionProfile::Edit => "workspace-write",
        PermissionProfile::ReadOnly => "read-only",
    };
    let mut lines = vec![
        "# Generated by Engine Box for one profile run. Not the workspace's ~/.codex/config.toml."
            .to_string(),
        "approval_policy = \"never\"".into(),
        format!("sandbox_mode = {}", toml_string(sandbox)),
        String::new(),
        "[features]".into(),
    ];
    lines.extend(
        CODEX_DISABLED_FEATURES
            .iter()
            .map(|f| format!("{f} = false")),
    );
    lines.extend([
        String::new(),
        "[skills.bundled]".into(),
        "enabled = false".into(),
    ]);
    if block.network_access && block.permission_profile == PermissionProfile::Edit {
        lines.extend([
            String::new(),
            "[sandbox_workspace_write]".into(),
            "network_access = true".into(),
        ]);
    }
    if browser {
        lines.extend([
            String::new(),
            "[mcp_servers.enginebox-browser]".into(),
            "command = \"/usr/local/bin/enginebox-guest\"".into(),
            "args = [\"browser\", \"--client\", \"codex\"]".into(),
            "default_tools_approval_mode = \"approve\"".into(),
        ]);
    }
    for s in &block.mcp_servers {
        lines.push(String::new());
        // Names are validated to [A-Za-z0-9_-], which TOML accepts as bare keys.
        lines.push(format!("[mcp_servers.{}]", s.name));
        if let Some(command) = &s.command {
            lines.push(format!("command = {}", toml_string(command)));
            lines.push(format!("args = {}", toml_array(&s.args)));
            if !s.env_vars.is_empty() {
                lines.push(format!("env_vars = {}", toml_array(&s.env_vars)));
            }
        } else if let Some(url) = &s.url {
            lines.push(format!("url = {}", toml_string(url)));
        }
        // Under approval_policy = "never" Codex rejects MCP calls that have no
        // approval mode; the owner chose these servers for this profile.
        lines.push("default_tools_approval_mode = \"approve\"".into());
        if !s.headers_from_env.is_empty() {
            lines.push(String::new());
            lines.push(format!("[mcp_servers.{}.env_http_headers]", s.name));
            for (h, k) in &s.headers_from_env {
                lines.push(format!("{} = {}", toml_string(h), toml_string(k)));
            }
        }
    }
    format!("{}\n", lines.join("\n"))
}

/// True when the workspace's own Codex config carries the Engine Box browser
/// server (installed by the template), so a non-strict profile keeps it.
fn codex_home_has_browser(home: &Path) -> bool {
    let Ok(Some(bytes)) = security::read_bytes(home, ".codex/config.toml", 1024 * 1024, false)
    else {
        return false;
    };
    let Ok(config) = toml::from_str::<toml::Value>(&String::from_utf8_lossy(&bytes)) else {
        return false;
    };
    config
        .get("mcp_servers")
        .and_then(|s| s.get("enginebox-browser"))
        .and_then(|e| e.get("command"))
        .and_then(toml::Value::as_str)
        == Some("/usr/local/bin/enginebox-guest")
}

/// The native command line for a profile run. Starts from the same fixed
/// structure as `native_command`; only the documented, profile-controlled
/// options differ. No path through here emits a sandbox-bypass flag.
pub fn command(
    r: &native::RunRequest,
    exe: &str,
    block: &ProviderProfile,
    m: &Materialized,
) -> Result<Vec<String>> {
    let mut args = vec![exe.to_string()];
    let path = |p: &Path| -> Result<String> {
        p.to_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow::anyhow!("Invalid profile path."))
    };
    match r.provider.as_str() {
        "codex" => {
            args.push("exec".into());
            if let Some(id) = &r.resume_id {
                args.extend(["resume".into(), id.clone()]);
            }
            let sandbox = match block.permission_profile {
                PermissionProfile::Edit => "sandbox_mode=\"workspace-write\"",
                PermissionProfile::ReadOnly => "sandbox_mode=\"read-only\"",
            };
            args.extend(
                [
                    "--json",
                    "--skip-git-repo-check",
                    "-c",
                    "approval_policy=\"never\"",
                    "-c",
                    sandbox,
                ]
                .map(str::to_owned),
            );
        }
        "claude" => {
            let (mode, default_tools) = match block.permission_profile {
                PermissionProfile::Edit => ("acceptEdits", CLAUDE_EDIT_TOOLS),
                // dontAsk denies every tool that is not preapproved.
                PermissionProfile::ReadOnly => ("dontAsk", CLAUDE_READ_ONLY_TOOLS),
            };
            let tools = block
                .allowed_tools
                .as_ref()
                .map(|t| t.join(","))
                .unwrap_or_else(|| default_tools.into());
            args.extend(
                [
                    "-p",
                    "--output-format",
                    "stream-json",
                    "--verbose",
                    "--permission-mode",
                    mode,
                    "--permission-prompts",
                    "none",
                    "--allowedTools",
                ]
                .map(str::to_owned),
            );
            args.push(tools);
            if block.permission_profile == PermissionProfile::ReadOnly {
                args.extend(["--disallowedTools".into(), CLAUDE_WRITE_TOOLS.join(",")]);
            }
            args.extend([
                "--max-turns".into(),
                block.max_turns.unwrap_or(DEFAULT_MAX_TURNS).to_string(),
            ]);
            if let Some(sources) = &block.setting_sources {
                args.extend(["--setting-sources".into(), sources.join(",")]);
            }
            if let Some(config) = &m.mcp_config {
                args.extend(["--mcp-config".into(), path(config)?]);
            }
            if block.strict_mcp {
                args.push("--strict-mcp-config".into());
            }
            for dir in &m.plugin_dirs {
                args.extend(["--plugin-dir".into(), path(dir)?]);
            }
            if let Some(id) = &r.resume_id {
                args.extend(["--resume".into(), id.clone()]);
            }
        }
        _ => {
            args.extend(
                [
                    "-p",
                    "--output-format",
                    "stream-json",
                    "--trust",
                    "--sandbox",
                    "enabled",
                ]
                .map(str::to_owned),
            );
            if block.permission_profile == PermissionProfile::ReadOnly {
                args.extend(["--mode".into(), "ask".into()]);
            }
            for dir in &m.plugin_dirs {
                args.extend(["--plugin-dir".into(), path(dir)?]);
            }
            if let Some(id) = &r.resume_id {
                args.extend(["--resume".into(), id.clone()]);
            }
        }
    }
    if let Some(model) = &r.model {
        args.extend(["--model".into(), model.clone()]);
    }
    if r.provider == "cursor" {
        args.extend(["--".into(), r.prompt.clone()]);
    } else if r.provider == "codex" {
        args.push("-".into());
    }
    ensure!(
        !args.iter().any(|a| forbidden_flag(a)),
        "Profile produced a forbidden flag."
    );
    Ok(args)
}

/// Sandbox- and permission-bypassing options. Never emitted, for any input.
pub fn forbidden_flag(arg: &str) -> bool {
    let lower = arg.to_ascii_lowercase();
    lower.starts_with("--dangerously")
        || lower.starts_with("--allow-dangerously")
        || lower == "--yolo"
        || lower == "--force"
        || lower == "-f"
        || lower == "--approve-mcps"
        || lower.contains("bypasspermissions")
        || lower.contains("danger-full-access")
        || lower == "--bare"
}

#[cfg(test)]
mod connection_tests {
    use super::*;
    fn profile(value: Value) -> Profile {
        serde_json::from_value(value).unwrap()
    }
    #[test]
    fn connection_policy_selects_per_run_kind_and_validates() {
        let p = profile(json!({"name":"coder","providers":{"codex":{}},
            "connections":["a","b","c"],
            "connection_policy":{"planner":[],"builder":["b"],"*":["a"]}}));
        p.validate(&|_| true).unwrap();
        let pick = |kind| connections_for(&p.connections, &p.connection_policy, kind).to_vec();
        assert_eq!(pick(None), vec!["a", "b", "c"]);
        assert!(pick(Some("planner")).is_empty());
        assert_eq!(pick(Some("builder")), vec!["b"]);
        assert_eq!(pick(Some("deploy")), vec!["a"]);
        let open = profile(
            json!({"name":"coder","providers":{"codex":{}},"connections":["a"],"connection_policy":{"planner":[]}}),
        );
        assert_eq!(
            connections_for(&open.connections, &open.connection_policy, Some("debug")),
            ["a".to_string()]
        );
        for bad in [
            json!({"name":"x","providers":{"codex":{}},"connections":["a","a"]}),
            json!({"name":"x","providers":{"codex":{}},"connections":["A"]}),
            json!({"name":"x","providers":{"codex":{}},"connections":["a"],"connection_policy":{"debug":["b"]}}),
            json!({"name":"x","providers":{"codex":{}},"connections":["a"],"connection_policy":{"Bad Kind":[]}}),
        ] {
            assert!(profile(bad).validate(&|_| true).is_err());
        }
        // Profiles without connections serialize exactly as before.
        let legacy = profile(json!({"name":"x","providers":{"codex":{}}}));
        let text = serde_json::to_string(&legacy).unwrap();
        assert!(!text.contains("connection"));
        assert!(valid_run_kind("builder") && !valid_run_kind("Builder") && !valid_run_kind(""));
    }
}
