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
use crate::{native, profile, repository, security};
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
/// input of `mode` plus the material.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub mode: String,
    pub input: Value,
    pub credential: Delegated,
}
impl std::fmt::Debug for Envelope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Envelope")
            .field("mode", &self.mode)
            .field("credential", &self.credential)
            .finish()
    }
}

/// Files and settings through which a native CLI would find credentials
/// other than the run's own. Relative to the guest user's home.
const DELEGATED_FILES: &[&str] = &[
    ".codex/auth.json",
    ".claude/.credentials.json",
    crate::delegation::TOKEN_FILE,
    ".config/enginebox/agents/codex.json",
    ".config/enginebox/agents/claude.json",
    ".config/enginebox/agents/cursor.env",
];

/// Refuse a member run in a workspace whose home holds any other provider
/// credential: no fallback to the controller's accounts or a settings override.
pub fn require_credential_free(home: &Path) -> Result<()> {
    for relative in DELEGATED_FILES {
        let path = security::safe_path(home, relative, false)?;
        ensure!(
            std::fs::symlink_metadata(&path).is_err(),
            "This workspace holds delegated provider credentials; member runs need a credential-free workspace."
        );
    }
    if let Some(settings) = security::read_json(home, ".claude/settings.json")? {
        let env = settings["env"].as_object();
        ensure!(
            env.is_none_or(|e| !e.keys().any(|k| {
                k.starts_with("ANTHROPIC_")
                    || k.starts_with("CLAUDE_CODE_")
                    || k == "CLAUDE_CONFIG_DIR"
            })) && settings.get("apiKeyHelper").is_none(),
            "This workspace's Claude settings override authentication; member runs need a credential-free workspace."
        );
    }
    Ok(())
}

/// A run-private `CODEX_HOME` for a member run without a profile: the
/// workspace's `config.toml` (if any) and a link to its session store, so
/// settings and `resume` behave as in an ordinary run.
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
    #[cfg(unix)]
    {
        let sessions = home.join(".codex/sessions");
        if std::fs::symlink_metadata(&sessions).is_ok_and(|m| m.is_dir()) {
            std::os::unix::fs::symlink(&sessions, codex.join("sessions"))?;
        }
    }
    Ok(codex)
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

/// Parse and check an envelope the guest received.
pub fn parse(input: Value) -> Result<Envelope> {
    let envelope: Envelope = serde_json::from_value(input)?;
    ensure!(
        MODES.contains(&envelope.mode.as_str()),
        "Unsupported member run mode."
    );
    envelope.credential.validate()?;
    Ok(envelope)
}

/// The run request inside an envelope, for the provider check.
pub fn request_of(envelope: &Envelope) -> Result<native::RunRequest> {
    let request = match envelope.mode.as_str() {
        "run" => native::RunRequest::parse(envelope.input.clone())?,
        _ => native::RunRequest::parse(envelope.input["request"].clone())?,
    };
    ensure!(
        request.provider == envelope.credential.provider,
        "Member material is for another provider."
    );
    Ok(request)
}
