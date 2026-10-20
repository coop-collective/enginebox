//! Versioned behavioral contracts, independent of marketing/package versions.
//! This report proves binary support, not Docker, kernel or guest readiness.
use anyhow::{Result, ensure};
use serde_json::{Value, json};

pub const PROTOCOL: u32 = 1;
pub fn report() -> Value {
    json!({"protocol":PROTOCOL,"component":"enginebox-host","version":env!("CARGO_PKG_VERSION"),"os":std::env::consts::OS,"capabilities":{"reserved_workspace_guard":1,"external_checkout_lifecycle_guard":1},"read_only":true})
}
pub fn require_host(report: &Value) -> Result<()> {
    ensure!(
        serde_json::to_vec(report)?.len() <= 16 * 1024,
        "Node protocol report exceeds 16 KiB"
    );
    ensure!(
        report["protocol"] == PROTOCOL
            && report["component"] == "enginebox-host"
            && report["os"] == "linux"
            && report["capabilities"]["reserved_workspace_guard"] == 1
            && report["capabilities"]["external_checkout_lifecycle_guard"] == 1,
        "Node helper is incompatible with stopped workspace preparation; install a compatible complete release"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn protocol_requires_explicit_behavior_not_just_a_version() {
        let mut valid = report();
        valid["os"] = json!("linux");
        require_host(&valid).unwrap();
        for (key, value) in [
            ("protocol", json!(2)),
            ("os", json!("windows")),
            ("component", json!("ebox")),
            ("capabilities", json!({})),
        ] {
            let mut invalid = valid.clone();
            invalid[key] = value;
            assert!(require_host(&invalid).is_err());
        }
        assert!(require_host(&json!({"version":env!("CARGO_PKG_VERSION")})).is_err());
    }
}
