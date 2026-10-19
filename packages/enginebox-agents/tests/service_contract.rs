use enginebox_agents::{AgentService, security};
use enginebox_core::{Fleet, Host};
use serde_json::{Value, json};
#[cfg(unix)]
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
};
fn temp() -> (tempfile::TempDir, std::path::PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().canonicalize().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    security::private_dir(&path).unwrap();
    (t, path)
}
fn fleet(root: &std::path::Path) -> Fleet {
    Fleet::from_hosts(
        vec![Host {
            id: "test-node".into(),
            target: "offline.invalid".into(),
            ..Default::default()
        }],
        root,
    )
    .unwrap()
}
#[test]
fn restart_reuses_legacy_native_homes_and_marks_unfinished_runs_uncertain() {
    let (_t, root) = temp();
    let home = root.join("agents/homes/claude");
    security::private_dir(&home).unwrap();
    security::write_json(
        &home,
        ".config/enginebox/controller-token.json",
        &json!({"kind":"setup_token","value":"controller-token-private","expires_at":null}),
    )
    .unwrap();
    security::write_bytes(&home, ".claude/CLAUDE.md", b"tenant memory", 65536).unwrap();
    let runs = root.join("agent-runs");
    security::private_dir(&runs).unwrap();
    let id = "12345678901234567890123456789012";
    security::write_json(&runs,&format!("{id}.json"),&json!({"id":id,"host_id":"test-node","name":"ebox-private","provider":"claude","scope":"controller-owner","state_scope":"private-workspace","status":"running","workdir":".","resume_id":null,"created_at":"2026-10-01T00:00:00+00:00"})).unwrap();
    let service = AgentService::new(&root, fleet(&root)).unwrap();
    let record = service.get_run("test-node", "ebox-private", id).unwrap();
    assert_eq!(record["status"], "failed");
    assert_eq!(record["execution_uncertain"], true);
    assert!(service.get_run("test-node", "ebox-other", id).is_err());
    assert_eq!(
        std::fs::read(home.join(".claude/CLAUDE.md")).unwrap(),
        b"tenant memory"
    );
    assert_eq!(
        security::read_json(&home, ".config/enginebox/controller-token.json")
            .unwrap()
            .unwrap()["value"],
        "controller-token-private"
    );
    assert!(!root.join("homes").exists());
    assert!(!root.join("runs").exists());
    service.close();
}
#[cfg(unix)]
#[test]
fn controller_native_login_uses_filtered_protocol_and_persistent_native_home() {
    use std::os::unix::fs::PermissionsExt;
    let (_t, root) = temp();
    let exe = root.join("fake-codex");
    std::fs::write(&exe,b"#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'codex-cli 0.163.0'; exit 0; fi\nif [ \"$4\" = status ]; then echo 'Logged in using ChatGPT'; exit 0; fi\nprintf 'https://auth.openai.com/codex/device\\nABCD-EFGH\\nPRIVATE_EMAIL_AND_TOKEN\\n'\nprintf '{\"tokens\":{\"access_token\":\"access-token\",\"id_token\":\"identity-token\",\"refresh_token\":\"SECRET_REFRESH\",\"account_id\":\"account\"}}' >\"$HOME/.codex/auth.json\"\n").unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
    let service = AgentService::with_binaries(
        &root,
        fleet(&root),
        BTreeMap::from([("codex".into(), exe.to_string_lossy().into_owned())]),
    )
    .unwrap();
    let session = service.start("codex").unwrap();
    let id = session["id"].as_str().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let completed = loop {
        let current = service.session("codex", id).unwrap();
        assert!(!current.to_string().contains("PRIVATE_EMAIL_AND_TOKEN"));
        assert!(!current.to_string().contains("SECRET_REFRESH"));
        if ["succeeded", "failed"].contains(&current["status"].as_str().unwrap_or("")) {
            break current;
        }
        assert!(Instant::now() < deadline, "login timed out: {current}");
        std::thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(completed["status"], "succeeded");
    assert_eq!(
        completed["authorization_url"],
        "https://auth.openai.com/codex/device"
    );
    assert_eq!(completed["user_code"], "ABCD-EFGH");
    assert!(root.join("agents/homes/codex/.codex/auth.json").exists());
    let deadline = Instant::now() + Duration::from_secs(2);
    while service.pending() > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(service.pending(), 0);
    service.close();
}
#[test]
fn desktop_mcp_guest_mode_has_no_host_mutation_tools() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_enginebox-mcp"))
        .env("ENGINEBOX_GUEST", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    for request in [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"environment_state","arguments":{"name":"ebox-private","action":"stop"}}}),
    ] {
        writeln!(stdin, "{request}").unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        out.read_line(&mut line).unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        match request["id"].as_u64().unwrap() {
            1 => assert_eq!(
                response["result"]["serverInfo"]["name"],
                "Engine Box desktop"
            ),
            2 => {
                let tools = response["result"]["tools"].as_array().unwrap();
                assert_eq!(tools.len(), 4);
                assert!(
                    tools
                        .iter()
                        .all(|t| t["name"].as_str().unwrap().starts_with("desktop_"))
                );
            }
            3 => assert_eq!(response["result"]["isError"], true),
            _ => {}
        }
    }
    drop(stdin);
    assert!(child.wait().unwrap().success());
}

#[cfg(unix)]
#[test]
fn cancelling_login_releases_pending_and_preserves_other_native_home_files() {
    use std::os::unix::fs::PermissionsExt;
    let (_t, root) = temp();
    let exe = root.join("fake-codex");
    std::fs::write(&exe, b"#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'codex-cli 0.163.0'; exit 0; fi\nprintf 'https://auth.openai.com/codex/device\\nABCD-EFGH\\n'\nsleep 60\n").unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
    let service = AgentService::with_binaries(
        &root,
        fleet(&root),
        BTreeMap::from([("codex".into(), exe.to_string_lossy().into_owned())]),
    )
    .unwrap();
    let session = service.start("codex").unwrap();
    let id = session["id"].as_str().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while service.session("codex", id).unwrap()["status"] == "starting" && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(service.pending() > 0);
    assert_eq!(service.cancel("codex", id).unwrap()["status"], "cancelled");
    while service.pending() > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(service.pending(), 0);
    let home = root.join("agents/homes/codex");
    assert!(!security::Lease::active(&home, ".local/state/enginebox/auth-codex.lock").unwrap());
    service.close();
    assert!(service.start("codex").is_err());
}

#[cfg(unix)]
#[test]
fn claude_setup_token_is_private_and_verification_can_report_token_auth() {
    use std::os::unix::fs::PermissionsExt;
    let (_t, root) = temp();
    let exe = root.join("fake-claude");
    std::fs::write(&exe,b"#!/bin/sh\nif [ \"$1\" = --version ]; then echo '2.1.302 (Claude Code)'; exit 0; fi\nif [ \"$1\" = auth ]; then printf '{\"loggedIn\":true,\"authMethod\":\"oauth_token\"}\\n'; exit 0; fi\nprintf 'https://claude.com/cai/oauth/authorize?code=true\\n'\nprintf 'sk-ant-oat01-abcdefghijklmnopqrstuvwxyz0123456789\\n'\n").unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
    let service = AgentService::with_binaries(
        &root,
        fleet(&root),
        BTreeMap::from([("claude".into(), exe.to_string_lossy().into_owned())]),
    )
    .unwrap();
    let session = service.start("claude").unwrap();
    let id = session["id"].as_str().unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    let completed = loop {
        let value = service.session("claude", id).unwrap();
        assert!(!value.to_string().contains("sk-ant-oat01"));
        if ["succeeded", "failed"].contains(&value["status"].as_str().unwrap_or("")) {
            break value;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(completed["status"], "succeeded");
    let home = root.join("agents/homes/claude");
    let saved = security::read_json(&home, ".config/enginebox/controller-token.json")
        .unwrap()
        .unwrap();
    assert_eq!(saved["kind"], "setup_token");
    assert!(saved["value"].as_str().unwrap().starts_with("sk-ant-oat01"));
    service.close();
}

#[test]
fn native_client_install_plan_is_read_only_and_names_exact_qualified_versions() {
    let (_t, root) = temp();
    let state = root.join("new-state");
    let plan = enginebox_agents::clients::plan(
        &state,
        &["codex".into(), "claude".into(), "cursor".into()],
    )
    .unwrap();
    assert_eq!(plan["actions"][0]["qualified_version"], "0.163.0");
    assert_eq!(plan["actions"][1]["qualified_version"], "2.1.302");
    assert_eq!(
        plan["actions"][2]["qualified_version"],
        "2026.10.16-e3cb207"
    );
    assert!(!state.exists());
    assert!(enginebox_agents::clients::plan(&state, &["invalid".into()]).is_err());
}
