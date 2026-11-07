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
use crate::{profile, repository, security};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The guest mode for member runs.
pub const GUEST_MODE: &str = "run-member";
/// The modes a member envelope may wrap.
pub const MODES: &[&str] = &["run", "run-profile", "run-repository"];
/// A repository envelope plus the material.
pub const MAX_ENVELOPE_BYTES: usize = repository::MAX_ENVELOPE_BYTES + 65536;

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
                            o.len() == 1 && security::token(&o["CLAUDE_CODE_OAUTH_TOKEN"])
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

/// Claude settings that would authenticate as someone other than the member.
fn claude_settings_override(settings: &Value) -> bool {
    settings.get("apiKeyHelper").is_some()
        || settings.get("awsAuthRefresh").is_some()
        || settings.get("awsCredentialExport").is_some()
        || settings["env"].as_object().is_some_and(|e| {
            e.keys().any(|k| {
                k.starts_with("ANTHROPIC_")
                    || k.starts_with("CLAUDE_CODE_")
                    || k.starts_with("CLAUDE_CONFIG")
                    || k.starts_with("AWS_")
                    || k.starts_with("GOOGLE_")
                    || k.starts_with("VERTEX_")
            })
        })
}
/// Claude's state file (`.claude.json`) may hold an API key.
fn claude_state_key(state: &Value) -> bool {
    state.get("primaryApiKey").is_some() || state.get("customApiKeyResponses").is_some()
}
fn check_claude_state(path: &Path) -> Result<()> {
    let Ok(raw) = std::fs::read(path) else {
        return Ok(());
    };
    let state: Value = serde_json::from_slice(&raw)
        .map_err(|_| anyhow::anyhow!("Unreadable Claude state; member runs refuse it."))?;
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

/// Codex configuration keys that select another provider, an API key or a
/// bearer token instead of the member's ChatGPT login.
const CODEX_FORBIDDEN_KEYS: &[&str] = &[
    "model_providers",
    "model_provider",
    "preferred_auth_method",
    "forced_login_method",
    "experimental_bearer_token",
    "env_key",
    "http_headers",
    "env_http_headers",
    "chatgpt_base_url",
    "openai_base_url",
    "base_url",
];
fn codex_forbidden(value: &toml::Value) -> bool {
    match value {
        toml::Value::Table(t) => t
            .iter()
            .any(|(k, v)| CODEX_FORBIDDEN_KEYS.contains(&k.as_str()) || codex_forbidden(v)),
        toml::Value::Array(a) => a.iter().any(codex_forbidden),
        _ => false,
    }
}
/// Refuse a Codex configuration that could authenticate as anyone but the
/// member (another provider, an API key, a bearer token or base URL).
pub fn check_codex_config(codex_home: &Path) -> Result<()> {
    let Some(raw) = security::read_bytes(codex_home, "config.toml", security::LIMIT, false)? else {
        return Ok(());
    };
    let config: toml::Value = toml::from_str(std::str::from_utf8(&raw)?)
        .map_err(|_| anyhow::anyhow!("Unreadable Codex configuration; member runs refuse it."))?;
    ensure!(
        !codex_forbidden(&config),
        "The Codex configuration selects another provider or key; member runs refuse it."
    );
    Ok(())
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
/// workspace's `config.toml` (checked later), and the member's own session
/// store, so `resume` works without touching anyone else's history.
pub fn prepare_codex_home(home: &Path, run_id: &str) -> Result<PathBuf> {
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
    if let Some(mut state) = security::read_json(home, ".claude.json")?
        && claude_state_key(&state)
    {
        if let Some(o) = state.as_object_mut() {
            o.remove("primaryApiKey");
            o.remove("customApiKeyResponses");
        }
        security::write_json(home, ".claude.json", &state)?;
        removed += 1;
    }
    for relative in [".claude/settings.json", ".claude/settings.local.json"] {
        if let Some(mut settings) = security::read_json(home, relative)?
            && claude_settings_override(&settings)
        {
            if let Some(o) = settings.as_object_mut() {
                for key in ["apiKeyHelper", "awsAuthRefresh", "awsCredentialExport"] {
                    o.remove(key);
                }
            }
            if let Some(env) = settings["env"].as_object_mut() {
                env.retain(|k, _| {
                    !(k.starts_with("ANTHROPIC_")
                        || k.starts_with("CLAUDE_CODE_")
                        || k.starts_with("CLAUDE_CONFIG")
                        || k.starts_with("AWS_")
                        || k.starts_with("GOOGLE_")
                        || k.starts_with("VERTEX_"))
                });
            }
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
