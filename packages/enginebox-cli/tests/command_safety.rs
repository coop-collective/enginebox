use std::process::Command;

#[test]
fn node_local_storage_cannot_ignore_a_remote_host_selection() {
    for selection in [
        vec!["--host", "unused.invalid"],
        vec!["--fleet-host", "registered"],
    ] {
        let result = Command::new(env!("CARGO_BIN_EXE_ebox"))
            .env_clear()
            .args(selection)
            .args(["storage", "layerfs", "doctor", "--config", "absent.json"])
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("node-local"));
    }
}

#[test]
fn lifecycle_commands_cannot_silently_bypass_the_controller() {
    for action in ["pause", "resume", "hibernate", "stop"] {
        let result = Command::new(env!("CARGO_BIN_EXE_ebox"))
            .env_clear()
            .args(["--host", "unused.invalid", action, "ebox-private"])
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("--fleet-host"));
    }
}

#[test]
fn private_snapshot_operations_require_explicit_administrator_mode() {
    for args in [
        vec!["snapshot", "ebox-private", "checkpoint"],
        vec!["restore", "ebox-private", "checkpoint"],
        vec!["fork", "ebox-private", "checkpoint", "ebox-copy"],
    ] {
        let result = Command::new(env!("CARGO_BIN_EXE_ebox"))
            .env_clear()
            .args(["--host", "unused.invalid"])
            .args(args)
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("--direct"));
    }
}
