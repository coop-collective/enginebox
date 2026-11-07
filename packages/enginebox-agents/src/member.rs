//! Member subscription runs (docs/MEMBER-SUBSCRIPTIONS.md).
//!
//! A member run carries the access material of exactly one member's own
//! credential to the guest on stdin (never argv, never the workspace's
//! HOME). The guest writes it only into that run's private directory (Codex:
//! `CODEX_HOME/auth.json`) or the run's process environment (Claude:
//! `CLAUDE_CODE_OAUTH_TOKEN`), redacts it from output, and removes the
//! directory when the run ends. Refresh tokens never leave the controller:
//! material that carries one is refused here.
//!
//! No fallback: a member run refuses to start in a workspace that holds any
//! delegated provider credential of its own (the controller's accounts, a
//! settings override, a setup token), so the native CLI can only ever
//! authenticate with the member's material.
use crate::{profile, security};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The guest mode for member runs.
pub const GUEST_MODE: &str = "run-member";
/// The modes a member envelope may wrap.
/// (Hosted repositories are not offered to member runs in this release.)
pub const MODES: &[&str] = &["run", "run-profile"];
/// A profile envelope plus the material.
pub const MAX_ENVELOPE_BYTES: usize = profile::MAX_ENVELOPE_BYTES + 65536;
/// Claude's state file (`.claude.json`) grows with use; read and rewrite it
/// within this bound rather than the 64 KiB private-file default.
pub const CLAUDE_STATE_LIMIT: usize = 16 * 1024 * 1024;

/// Short-lived access material for one run of one member's own credential.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Delegated {
    pub provider: String,
    /// `native` (a Codex `auth.json`) or `environment` (Claude's variable).
    pub kind: String,
    pub content: Value,
    /// Unix seconds when the access token expires, when known.
    #[serde(default)]
    pub expires_at: Option<f64>,
}
impl std::fmt::Debug for Delegated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the material.
        f.debug_struct("Delegated")
            .field("provider", &self.provider)
            .field("kind", &self.kind)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}
impl Delegated {
    /// From [`crate::delegation::material`] output for `provider`.
    pub fn from_material(provider: &str, material: &Value) -> Result<Self> {
        let delegated = Self {
            provider: provider.to_owned(),
            kind: material["kind"].as_str().unwrap_or("").to_owned(),
            content: material["content"].clone(),
            expires_at: material["expires_at"].as_f64(),
        };
        delegated.validate()?;
        Ok(delegated)
    }
    /// Only access material: Codex's access-token `auth.json` with an empty
    /// refresh token, or Claude's access token alone.
    pub fn validate(&self) -> Result<()> {
        match self.provider.as_str() {
            "codex" => {
                let t = &self.content["tokens"];
                ensure!(
                    self.kind == "native"
                        && self.content["auth_mode"] == "chatgptAuthTokens"
                        && self.content["OPENAI_API_KEY"].is_null()
                        && security::token(&t["access_token"])
                        && security::token(&t["id_token"])
                        && t["refresh_token"] == ""
                        && t["account_id"].as_str().is_some_and(|s| {
                            !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control)
                        }),
                    "Member material must be a ChatGPT access token without a refresh token."
                );
            }
            "claude" => {
                ensure!(
                    self.kind == "environment"
                        && self.content.as_object().is_some_and(|o| {
                            o.len() == 1
                                && o.get("CLAUDE_CODE_OAUTH_TOKEN")
                                    .is_some_and(security::token)
                        }),
                    "Member material must be a Claude access token."
                );
            }
            _ => anyhow::bail!("Member runs support claude and codex."),
        }
        if let Some(at) = self.expires_at {
            ensure!(at > 0. && at < 253402300799., "Invalid expiry.");
        }
        Ok(())
    }
    /// Every secret value, for output redaction.
    pub fn secrets(&self) -> Vec<String> {
        let values = match self.provider.as_str() {
            "codex" => vec![
                &self.content["tokens"]["access_token"],
                &self.content["tokens"]["id_token"],
            ],
            _ => vec![&self.content["CLAUDE_CODE_OAUTH_TOKEN"]],
        };
        values
            .into_iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    }
}

/// What the controller sends the guest for a member run: the ordinary run
/// input of `mode`, the member's key (a digest of their user ID, never the
/// ID itself) and the material.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub mode: String,
    pub input: Value,
    pub member_key: String,
    pub credential: Delegated,
}
impl std::fmt::Debug for Envelope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Envelope")
            .field("mode", &self.mode)
            .field("member_key", &self.member_key)
            .field("credential", &self.credential)
            .finish()
    }
}

/// The guest-side key of a member: 64 hex characters naming their private
/// session store, so one member's history never meets another's.
pub fn member_key(member: &str) -> String {
    security::digest(&Value::String(format!("enginebox-member:{member}")))
}
fn valid_key(key: &str) -> bool {
    key.len() == 64
        && key
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Files through which a native CLI would find credentials other than the
/// run's own. Relative to the guest user's home.
pub const DELEGATED_FILES: &[&str] = &[
    ".codex/auth.json",
    ".claude/.credentials.json",
    crate::delegation::TOKEN_FILE,
    ".config/enginebox/agents/codex.json",
    ".config/enginebox/agents/claude.json",
    ".config/enginebox/agents/cursor.env",
];
/// Claude settings files that apply whatever the config directory is
/// (managed policy), checked on every member run.
const MANAGED_CLAUDE_SETTINGS: &[&str] = &[
    "/etc/claude-code/managed-settings.json",
    "/Library/Application Support/ClaudeCode/managed-settings.json",
];

/// Claude settings keys that run a helper to obtain credentials.
const CLAUDE_AUTH_SETTINGS: &[&str] = &["apiKeyHelper", "awsAuthRefresh", "awsCredentialExport"];
/// Environment variable prefixes in Claude settings that select another
/// credential, endpoint, cloud or configuration directory.
const CLAUDE_AUTH_ENV_PREFIXES: &[&str] = &[
    "ANTHROPIC_",
    "CLAUDE_CODE_",
    "CLAUDE_CONFIG",
    "AWS_",
    "GOOGLE_",
    "VERTEX_",
];
fn claude_auth_env(key: &str) -> bool {
    CLAUDE_AUTH_ENV_PREFIXES.iter().any(|p| key.starts_with(p))
}
/// Claude settings that would authenticate as someone other than the member.
fn claude_settings_override(settings: &Value) -> bool {
    CLAUDE_AUTH_SETTINGS
        .iter()
        .any(|k| settings.get(k).is_some())
        || settings["env"]
            .as_object()
            .is_some_and(|e| e.keys().any(|k| claude_auth_env(k)))
}
/// Remove what [`claude_settings_override`] refuses; true when anything was.
fn strip_claude_overrides(settings: &mut Value) -> bool {
    if !claude_settings_override(settings) {
        return false;
    }
    if let Some(o) = settings.as_object_mut() {
        for key in CLAUDE_AUTH_SETTINGS {
            o.remove(*key);
        }
    }
    if let Some(env) = settings["env"].as_object_mut() {
        env.retain(|k, _| !claude_auth_env(k));
    }
    true
}
/// `.claude.json` keys holding an API key.
const CLAUDE_STATE_KEYS: &[&str] = &["primaryApiKey", "customApiKeyResponses"];
/// Claude's state file (`.claude.json`) may hold an API key.
fn claude_state_key(state: &Value) -> bool {
    CLAUDE_STATE_KEYS.iter().any(|k| state.get(k).is_some())
}
/// Read a `.claude.json` within [`CLAUDE_STATE_LIMIT`] (none when absent).
fn read_claude_state(path: &Path) -> Result<Option<Value>> {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Ok(None);
    };
    ensure!(
        meta.is_file() && meta.len() <= CLAUDE_STATE_LIMIT as u64,
        "Claude's state file is not a regular file within 16 MiB; member runs refuse it."
    );
    let raw = std::fs::read(path)?;
    Ok(Some(serde_json::from_slice(&raw).map_err(|_| {
        anyhow::anyhow!("Unreadable Claude state; member runs refuse it.")
    })?))
}
fn write_claude_state(root: &Path, state: &Value) -> Result<()> {
    security::write_bytes(
        root,
        ".claude.json",
        &serde_json::to_vec(state)?,
        CLAUDE_STATE_LIMIT,
    )
}
fn check_claude_state(path: &Path) -> Result<()> {
    let Some(state) = read_claude_state(path)? else {
        return Ok(());
    };
    ensure!(
        !claude_state_key(&state),
        "Claude's state in this workspace holds an API key; member runs refuse it."
    );
    Ok(())
}
fn check_claude_settings(path: &Path) -> Result<()> {
    let Ok(raw) = std::fs::read(path) else {
        return Ok(());
    };
    // Unreadable or malformed settings could still be read by the CLI: refuse.
    let settings: Value = serde_json::from_slice(&raw)
        .map_err(|_| anyhow::anyhow!("Unreadable Claude settings; member runs refuse them."))?;
    ensure!(
        !claude_settings_override(&settings),
        "Claude settings in this workspace override authentication; member runs refuse them."
    );
    Ok(())
}

/// Refuse a member run in a workspace whose home, managed policy or working
/// tree holds any other provider credential or authentication override: no
/// fallback to the controller's accounts, a key, a helper or a proxy.
pub fn require_credential_free(home: &Path, cwd: &Path, workspace: &Path) -> Result<()> {
    for relative in DELEGATED_FILES {
        let path = security::safe_path(home, relative, false)?;
        ensure!(
            std::fs::symlink_metadata(&path).is_err(),
            "This workspace holds delegated provider credentials; member runs need a credential-free workspace."
        );
    }
    for relative in [".claude/settings.json", ".claude/settings.local.json"] {
        check_claude_settings(&home.join(relative))?;
    }
    check_claude_state(&home.join(".claude.json"))?;
    for managed in MANAGED_CLAUDE_SETTINGS {
        check_claude_settings(Path::new(managed))?;
    }
    // Project and local settings from the run's directory up to the workspace.
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        for relative in [".claude/settings.json", ".claude/settings.local.json"] {
            check_claude_settings(&d.join(relative))?;
        }
        if d == workspace {
            break;
        }
        dir = d.parent().filter(|p| p.starts_with(workspace));
    }
    Ok(())
}

/// A member's own Claude settings or state file must not select a key,
/// helper or proxy either.
pub fn require_clean_claude_file(path: &Path) -> Result<()> {
    if path.file_name().is_some_and(|n| n == ".claude.json") {
        check_claude_state(path)
    } else {
        check_claude_settings(path)
    }
}

/// What a member run carries into the native run: the material and the
/// member's key.
#[derive(Clone, Copy)]
pub struct RunCredential<'a> {
    pub credential: &'a Delegated,
    pub member_key: &'a str,
}

/// Removes a member run's private directory when dropped, on every path.
pub struct RunDirCleanup {
    home: PathBuf,
    id: String,
}
impl RunDirCleanup {
    pub fn new(home: &Path, id: &str) -> Self {
        Self {
            home: home.to_path_buf(),
            id: id.to_owned(),
        }
    }
}
impl Drop for RunDirCleanup {
    fn drop(&mut self) {
        let _ = profile::remove_run_dir(&self.home, &self.id);
    }
}

/// System and managed Codex configuration, which applies to every run.
const MANAGED_CODEX_CONFIG: &[&str] = &["/etc/codex/config.toml", "/etc/codex/managed_config.toml"];

/// Codex configuration keys that select another provider, an API key, a
/// bearer token or an endpoint instead of the member's ChatGPT login. They
/// are refused at the top level and in `profiles.*`, where Codex reads its
/// model provider and authentication; MCP server tables (`mcp_servers.*`)
/// may use headers, keys and URLs of their own.
const CODEX_FORBIDDEN_KEYS: &[&str] = &[
    "model_providers",
    "model_provider",
    "preferred_auth_method",
    "forced_login_method",
    "forced_chatgpt_workspace_id",
    "experimental_bearer_token",
    "env_key",
    "http_headers",
    "env_http_headers",
    "chatgpt_base_url",
    "openai_base_url",
    "base_url",
    "api_key",
];
fn codex_table_forbidden(table: &toml::Table) -> bool {
    table
        .keys()
        .any(|k| CODEX_FORBIDDEN_KEYS.contains(&k.as_str()) || k.ends_with("_base_url"))
}
fn codex_forbidden(config: &toml::Table) -> bool {
    codex_table_forbidden(config)
        || config
            .get("profiles")
            .and_then(toml::Value::as_table)
            .is_some_and(|profiles| {
                profiles
                    .values()
                    .any(|p| p.as_table().is_none_or(codex_table_forbidden))
            })
}
fn check_codex_file(path: &Path) -> Result<()> {
    let Ok(raw) = std::fs::read(path) else {
        return Ok(());
    };
    let config: toml::Table = std::str::from_utf8(&raw)
        .ok()
        .and_then(|text| toml::from_str(text).ok())
        .ok_or_else(|| anyhow::anyhow!("Unreadable Codex configuration; member runs refuse it."))?;
    ensure!(
        !codex_forbidden(&config),
        "The Codex configuration selects another provider or key; member runs refuse it."
    );
    Ok(())
}
/// Refuse a Codex configuration that could authenticate as anyone but the
/// member: the run's own `CODEX_HOME/config.toml` and every project
/// `.codex/config.toml` from the working directory up to the workspace root.
pub fn check_codex_config(codex_home: &Path, cwd: &Path, workspace: &Path) -> Result<()> {
    check_codex_file(&codex_home.join("config.toml"))?;
    for managed in MANAGED_CODEX_CONFIG {
        check_codex_file(Path::new(managed))?;
    }
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        check_codex_file(&d.join(".codex/config.toml"))?;
        if d == workspace {
            break;
        }
        dir = d.parent().filter(|p| p.starts_with(workspace));
    }
    Ok(())
}

/// Copy the workspace template's native client setup (instructions, skills,
/// prompts, agents, plugins, commands, rules) from `from` into a member's
/// private directory `to`, never credentials, sessions or history. Regular
/// files and directories only (links are skipped), bounded in size.
pub fn copy_template(from: &Path, to: &Path, entries: &[&str]) -> Result<()> {
    let mut budget: u64 = 256 * 1024 * 1024;
    for name in entries {
        let source = from.join(name);
        let Ok(meta) = std::fs::symlink_metadata(&source) else {
            continue;
        };
        let target = to.join(name);
        if std::fs::symlink_metadata(&target)
            .is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
        {
            std::fs::remove_dir_all(&target)?;
        } else if std::fs::symlink_metadata(&target).is_ok() {
            std::fs::remove_file(&target)?;
        }
        copy_tree(&source, &target, &meta, &mut budget)?;
    }
    Ok(())
}
/// A digest of the template entries' paths, sizes and modification times.
fn template_digest(from: &Path, entries: &[&str]) -> String {
    fn walk(path: &Path, rel: &str, out: &mut Vec<Value>) {
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return;
        };
        if meta.is_dir() {
            let mut names: Vec<_> = std::fs::read_dir(path)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.file_name())
                .collect();
            names.sort();
            for name in names {
                walk(
                    &path.join(&name),
                    &format!("{rel}/{}", name.to_string_lossy()),
                    out,
                );
            }
        } else if meta.is_file() {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as u64);
            out.push(serde_json::json!([rel, meta.len(), mtime]));
        }
    }
    let mut out = vec![];
    for name in entries {
        walk(&from.join(name), name, &mut out);
    }
    security::digest(&Value::Array(out))
}
/// [`copy_template`] only when the template changed since the last copy
/// into `to` (a digest of paths, sizes and times kept in `to`).
pub fn sync_template(from: &Path, to: &Path, entries: &[&str]) -> Result<()> {
    const MARKER: &str = ".enginebox-template";
    let digest = template_digest(from, entries);
    if security::read_bytes(to, MARKER, 128, false)?.is_some_and(|d| d == digest.as_bytes()) {
        return Ok(());
    }
    copy_template(from, to, entries)?;
    security::write_bytes(to, MARKER, digest.as_bytes(), 128)
}
fn copy_tree(
    source: &Path,
    target: &Path,
    meta: &std::fs::Metadata,
    budget: &mut u64,
) -> Result<()> {
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    if meta.is_dir() {
        security::private_dir(target)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            let meta = std::fs::symlink_metadata(entry.path())?;
            copy_tree(
                &entry.path(),
                &target.join(entry.file_name()),
                &meta,
                budget,
            )?;
        }
    } else if meta.is_file() {
        ensure!(
            meta.len() <= *budget,
            "The template's client setup is too large to copy."
        );
        *budget -= meta.len();
        std::fs::copy(source, target)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Owner-only, keeping execute bits (hooks, scripts).
            let mode = (meta.permissions().mode() & 0o700) | 0o600;
            std::fs::set_permissions(target, std::fs::Permissions::from_mode(mode))?;
        }
    }
    Ok(())
}
/// What of `~/.codex` a member's `CODEX_HOME` gets (besides `config.toml`).
pub const CODEX_TEMPLATE: &[&str] = &[
    "AGENTS.md",
    "AGENTS.override.md",
    "prompts",
    "skills",
    "rules",
    "plugins",
];
/// What of `~/.claude` a member's `CLAUDE_CONFIG_DIR` gets.
pub const CLAUDE_TEMPLATE: &[&str] = &[
    "CLAUDE.md",
    "settings.json",
    "skills",
    "agents",
    "commands",
    "plugins",
    "output-styles",
];
/// `.claude.json` keys a member's configuration takes from the template:
/// user MCP servers and onboarding, never keys or project history.
const CLAUDE_STATE_TEMPLATE: &[&str] = &[
    "mcpServers",
    "hasCompletedOnboarding",
    "lastOnboardingVersion",
    "theme",
    "bypassPermissionsModeAccepted",
];

/// A member's Claude configuration directory with the template's setup:
/// copied from `~/.claude` (settings stripped of any authentication
/// override) and the template's user MCP servers merged into the member's
/// own `.claude.json`. The member's sessions in it are kept.
pub fn prepare_claude_config(home: &Path, key: &str) -> Result<PathBuf> {
    let config = member_dir(home, key, "claude")?;
    sync_template(&home.join(".claude"), &config, CLAUDE_TEMPLATE)?;
    if let Some(mut settings) = security::read_json(&config, "settings.json")?
        && strip_claude_overrides(&mut settings)
    {
        security::write_json(&config, "settings.json", &settings)?;
    }
    let mut state =
        read_claude_state(&config.join(".claude.json"))?.unwrap_or(serde_json::json!({}));
    ensure!(state.is_object(), "Invalid member Claude state.");
    if let Some(template) = read_claude_state(&home.join(".claude.json"))? {
        for key in CLAUDE_STATE_TEMPLATE {
            if let Some(v) = template.get(*key) {
                state[*key] = v.clone();
            }
        }
    }
    if let Some(o) = state.as_object_mut() {
        for key in CLAUDE_STATE_KEYS {
            o.remove(*key);
        }
    }
    write_claude_state(&config, &state)?;
    Ok(config)
}

/// The member's own persistent store under the guest home (sessions,
/// Claude's configuration directory), created private on first use.
pub fn member_dir(home: &Path, key: &str, name: &str) -> Result<PathBuf> {
    ensure!(valid_key(key), "Invalid member key.");
    let relative = format!(".local/state/enginebox/members/{key}/{name}/placeholder");
    security::safe_path(home, &relative, true)?;
    let dir = home.join(format!(".local/state/enginebox/members/{key}/{name}"));
    security::private_dir(&dir)?;
    Ok(dir)
}

/// A run-private `CODEX_HOME` for a member run without a profile: the
/// workspace's `config.toml` (checked later) and links to the member's own
/// copy of the template's setup (instructions, prompts, skills, rules,
/// plugins), refreshed only when the template changes. Never auth, sessions
/// or history.
pub fn prepare_codex_home(home: &Path, run_id: &str, key: &str) -> Result<PathBuf> {
    let root = profile::run_dir(home, run_id)?;
    security::safe_path(
        home,
        &format!("{}/{run_id}/placeholder", profile::RUNS_DIR),
        true,
    )?;
    profile::remove_run_dir(home, run_id)?;
    security::private_dir(&root)?;
    let codex = root.join("codex-home");
    security::private_dir(&codex)?;
    if let Some(config) = security::read_bytes(home, ".codex/config.toml", security::LIMIT, false)?
    {
        security::write_bytes(&root, "codex-home/config.toml", &config, security::LIMIT)?;
    }
    let cache = member_dir(home, key, "codex-template")?;
    sync_template(&home.join(".codex"), &cache, CODEX_TEMPLATE)?;
    #[cfg(unix)]
    for name in CODEX_TEMPLATE {
        if std::fs::symlink_metadata(cache.join(name)).is_ok() {
            std::os::unix::fs::symlink(cache.join(name), codex.join(name))?;
        }
    }
    Ok(codex)
}

/// Point a member run's `CODEX_HOME` at the member's own session store
/// (replacing a link to the workspace's shared one).
pub fn link_member_sessions(codex_home: &Path, home: &Path, key: &str) -> Result<()> {
    let sessions = member_dir(home, key, "codex-sessions")?;
    let link = codex_home.join("sessions");
    if std::fs::symlink_metadata(&link).is_ok() {
        let meta = std::fs::symlink_metadata(&link)?;
        ensure!(
            meta.file_type().is_symlink(),
            "Unexpected sessions directory."
        );
        std::fs::remove_file(&link)?;
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&sessions, &link)?;
    Ok(())
}

/// Write the run's own Codex access file into its private `CODEX_HOME`.
pub fn write_codex_auth(codex_home: &Path, credential: &Delegated) -> Result<()> {
    security::write_bytes(
        codex_home,
        "auth.json",
        &serde_json::to_vec(&credential.content)?,
        security::LIMIT,
    )
    .context("Member material could not be written.")
}

/// Remove every delegated provider credential and authentication override
/// from a guest home (`enginebox-guest transfer` `{"operation":"clear"}`),
/// so the workspace can serve member runs.
pub fn clear_delegated(home: &Path) -> Result<Value> {
    security::absolute_existing(home)?;
    let mut removed = 0;
    for relative in DELEGATED_FILES {
        let path = security::safe_path(home, relative, false)?;
        if std::fs::symlink_metadata(&path).is_ok() {
            security::remove(home, relative)?;
            removed += 1;
        }
    }
    if let Some(mut state) = read_claude_state(&home.join(".claude.json"))?
        && claude_state_key(&state)
    {
        if let Some(o) = state.as_object_mut() {
            for key in CLAUDE_STATE_KEYS {
                o.remove(*key);
            }
        }
        write_claude_state(home, &state)?;
        removed += 1;
    }
    for relative in [".claude/settings.json", ".claude/settings.local.json"] {
        if let Some(mut settings) = security::read_json(home, relative)?
            && strip_claude_overrides(&mut settings)
        {
            security::write_json(home, relative, &settings)?;
            removed += 1;
        }
    }
    Ok(serde_json::json!({"ok":true,"cleared":removed}))
}

/// Parse an envelope the guest received (the material is validated once,
/// where it is used).
pub fn parse(input: Value) -> Result<Envelope> {
    let envelope: Envelope = serde_json::from_value(input)?;
    ensure!(
        MODES.contains(&envelope.mode.as_str()),
        "Unsupported member run mode."
    );
    ensure!(valid_key(&envelope.member_key), "Invalid member key.");
    Ok(envelope)
}
