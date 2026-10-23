//! Native versions qualified for installation and credential delegation.
use anyhow::{Result, bail, ensure};

pub const CODEX_VERSION: &str = "0.163.0";
pub const CLAUDE_VERSION: &str = "2.1.302";
pub const CURSOR_VERSION: &str = "2026.10.22-27dcf5f";

pub fn qualified_version(provider: &str) -> Result<&'static str> {
    match provider {
        "codex" => Ok(CODEX_VERSION),
        "claude" => Ok(CLAUDE_VERSION),
        "cursor" => Ok(CURSOR_VERSION),
        _ => bail!("Unknown native provider."),
    }
}

pub fn verify_qualified_version(provider: &str, output: &str) -> Result<()> {
    let expected = qualified_version(provider)?;
    ensure!(
        output.split_whitespace().any(|part| part == expected),
        "Installed {provider} client is not the qualified version {expected}; prepare a new template with a qualified client before assigning workspaces."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_cursor_drift_and_partial_version_matches() {
        for version in [
            "2026.10.16-e3cb207",
            "2026.10.18-ea085cb",
            "2026.10.22-27dcf5f-extra",
        ] {
            assert!(verify_qualified_version("cursor", version).is_err());
        }
        assert!(verify_qualified_version("cursor", "2026.10.22-27dcf5f\n").is_ok());
        assert!(verify_qualified_version("codex", "codex-cli 0.163.0").is_ok());
        assert!(verify_qualified_version("claude", "2.1.302 (Claude Code)").is_ok());
        assert!(verify_qualified_version("unknown", CURSOR_VERSION).is_err());
    }
}
