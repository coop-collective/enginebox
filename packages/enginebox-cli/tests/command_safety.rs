use std::process::Command;

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
