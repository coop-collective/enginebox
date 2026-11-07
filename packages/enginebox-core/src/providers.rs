//! First-class native AI subscription providers.
//!
//! One registry: IDs, labels, usage links and installability. Callers must not
//! re-list `codex`/`claude`/`cursor`/`glm`.
use anyhow::{Result, ensure};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Provider {
    pub id: &'static str,
    pub label: &'static str,
    /// Human-facing usage/billing page. Allowance APIs may still be unavailable.
    pub usage_url: &'static str,
    pub usage_unavailable: &'static str,
    /// Preferred PATH binary. Empty when the provider is an app bundle (GLM/ZCode).
    pub binary: &'static str,
    /// Additional PATH names accepted when locating an already-installed client.
    pub binary_aliases: &'static [&'static str],
    /// Official Linux/macOS installer exists for guest/controller client setup.
    pub installable: bool,
    /// Guest MCP migration/config applies to this client.
    pub mcp_config: bool,
    /// Profile-attached MCP connections can be applied to this provider's CLI.
    pub profile_mcp: bool,
}

pub const CODEX: Provider = Provider {
    id: "codex",
    label: "Codex",
    usage_url: "https://chatgpt.com/codex/settings/usage",
    usage_unavailable: "Connect a ChatGPT account to read its reported allowance.",
    binary: "codex",
    binary_aliases: &[],
    installable: true,
    mcp_config: true,
    profile_mcp: true,
};
pub const CLAUDE: Provider = Provider {
    id: "claude",
    label: "Claude Code",
    usage_url: "https://claude.ai/settings/usage",
    usage_unavailable: "Claude does not provide an on-demand allowance through this integration. Check provider usage.",
    binary: "claude",
    binary_aliases: &[],
    installable: true,
    mcp_config: true,
    profile_mcp: true,
};
pub const CURSOR: Provider = Provider {
    id: "cursor",
    label: "Cursor Agent",
    usage_url: "https://cursor.com/dashboard",
    usage_unavailable: "Cursor does not expose personal allowance through the supported CLI. Check provider usage.",
    binary: "cursor-agent",
    // Historical Cursor installers and Engine Box guests expose `agent`.
    binary_aliases: &["agent"],
    installable: true,
    mcp_config: true,
    profile_mcp: false,
};
pub const GLM: Provider = Provider {
    id: "glm",
    label: "GLM (Z.ai Coding Plan)",
    usage_url: "https://z.ai/manage-apikey/billing",
    usage_unavailable: "GLM does not expose personal allowance through this integration. Check the Z.ai usage dashboard.",
    binary: "",
    binary_aliases: &[],
    installable: false,
    mcp_config: false,
    profile_mcp: false,
};

pub const ALL: &[Provider] = &[CODEX, CLAUDE, CURSOR, GLM];
/// Stable ID list for clap parsers, JSON Schema enums and iteration.
pub const IDS: &[&str] = &[CODEX.id, CLAUDE.id, CURSOR.id, GLM.id];

pub fn get(id: &str) -> Option<&'static Provider> {
    ALL.iter().find(|p| p.id == id)
}
pub fn known(id: &str) -> bool {
    get(id).is_some()
}
pub fn require(id: &str) -> Result<&'static Provider> {
    get(id).ok_or_else(|| anyhow::anyhow!("Unknown native agent provider."))
}
/// Clap `value_parser` for `--provider`.
pub fn parse_id(value: &str) -> Result<String, String> {
    require(value)
        .map(|p| p.id.to_string())
        .map_err(|e| e.to_string())
}
pub fn json_ids() -> Value {
    Value::Array(ALL.iter().map(|p| json!(p.id)).collect())
}
pub fn installable() -> impl Iterator<Item = &'static Provider> {
    ALL.iter().filter(|p| p.installable)
}
pub fn binary_names(id: &str) -> Vec<&'static str> {
    match get(id) {
        Some(p) if !p.binary.is_empty() => {
            let mut names = vec![p.binary];
            names.extend_from_slice(p.binary_aliases);
            names
        }
        _ => Vec::new(),
    }
}

/// Codex/Claude/Cursor native session IDs are UUIDs. GLM/ZCode uses `sess_…`.
pub fn glm_session_id(id: &str) -> bool {
    id.strip_prefix("sess_").is_some_and(|rest| {
        (1..=128).contains(&rest.len())
            && rest
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    })
}
pub fn uuid_session_id(id: &str) -> bool {
    regex::Regex::new(
        r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$",
    )
    .ok()
    .is_some_and(|re| re.is_match(id))
}
pub fn valid_resume_id(provider: &str, id: &str) -> bool {
    if provider == GLM.id {
        glm_session_id(id)
    } else {
        uuid_session_id(id)
    }
}
/// A stored run may carry either a UUID or a GLM session id.
pub fn recorded_session_id(id: &str) -> bool {
    uuid_session_id(id) || glm_session_id(id)
}

/// JSON Schema fragment for MCP/tool provider fields.
pub fn schema() -> Value {
    json!({"type":"string","enum":json_ids()})
}

pub fn validate_native_client(id: &str) -> Result<()> {
    ensure!(known(id), "Unsupported native client");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_is_the_only_id_list() {
        assert_eq!(IDS, &["codex", "claude", "cursor", "glm"]);
        assert!(known("glm") && known("cursor"));
        assert!(require("nope").is_err());
        assert_eq!(parse_id("glm").unwrap(), "glm");
        assert!(parse_id("gemini").is_err());
        assert_eq!(binary_names("cursor"), ["cursor-agent", "agent"]);
        assert!(binary_names("glm").is_empty());
        assert!(!GLM.installable && !GLM.mcp_config && !GLM.profile_mcp);
        assert!(CURSOR.mcp_config && !CURSOR.profile_mcp);
        assert!(installable().all(|p| p.installable));
    }

    #[test]
    fn session_ids_are_provider_specific() {
        let uuid = "adee2503-cabd-443e-9cef-0f0a5beb2f1a";
        assert!(valid_resume_id("cursor", uuid));
        assert!(!valid_resume_id("cursor", "sess_abc"));
        assert!(valid_resume_id("glm", "sess_abc"));
        assert!(valid_resume_id("glm", "sess_ABC-123_def"));
        assert!(!valid_resume_id("glm", uuid));
        assert!(!valid_resume_id("glm", "sess_"));
        assert!(!valid_resume_id("glm", "session_abc"));
        assert!(recorded_session_id(uuid) && recorded_session_id("sess_1"));
    }
}
