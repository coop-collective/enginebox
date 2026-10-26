//! Per-run environment injection: validation, delivery to the native process,
//! and that values never enter records, sidecars or returned output.
use enginebox_agents::{native, run_details, service};
use serde_json::json;
use std::collections::BTreeMap;

fn parse(env: serde_json::Value) -> anyhow::Result<native::RunRequest> {
    native::RunRequest::parse(json!({"provider":"codex","prompt":"hello","env":env}))
}

#[test]
fn env_names_must_be_well_formed_and_not_controlled_by_engine_box() {
    for key in [
        "lower",
        "1LEADING_DIGIT",
        "HAS-DASH",
        "HAS SPACE",
        "",
        &"A".repeat(65),
        "PATH",
        "HOME",
        "USER",
        "SHELL",
        "DISPLAY",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_BASE_URL",
        "OPENAI_API_KEY",
        "OPENAI_BASE_URL",
        "CODEX_HOME",
        "CURSOR_API_KEY",
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "DYLD_INSERT_LIBRARIES",
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "NODE_OPTIONS",
        "BASH_ENV",
        "XDG_CONFIG_HOME",
        "ENGINEBOX_ANYTHING",
        "RUST_LOG",
    ] {
        assert!(parse(json!({key:"value"})).is_err(), "{key} accepted");
        assert!(!native::env_key_allowed(key), "{key} allowed");
    }
    let ok =
        parse(json!({"API_BASE_URL":"https://example.test","A":"","GH_TOKEN":"x","A1_B2":"y"}))
            .unwrap();
    assert_eq!(ok.env_keys(), ["A", "A1_B2", "API_BASE_URL", "GH_TOKEN"]);
    assert!(native::env_key_allowed(&format!("A{}", "B".repeat(63))));
}

#[test]
fn env_count_size_and_values_are_bounded() {
    let too_many: serde_json::Map<_, _> = (0..=native::MAX_ENV_VARS)
        .map(|i| (format!("VAR_{i}"), json!("v")))
        .collect();
    assert!(parse(serde_json::Value::Object(too_many)).is_err());
    let at_cap: serde_json::Map<_, _> = (0..native::MAX_ENV_VARS)
        .map(|i| (format!("VAR_{i}"), json!("v")))
        .collect();
    assert!(parse(serde_json::Value::Object(at_cap)).is_ok());
    assert!(parse(json!({"BIG":"x".repeat(native::MAX_ENV_BYTES)})).is_err());
    assert!(parse(json!({"NUL":"a\u{0000}b"})).is_err());
    assert!(parse(json!({"NOT_STRING":1})).is_err());
    assert!(parse(json!(["A"])).is_err());
    // Prompt and env together must still fit the 16 KiB launch bound.
    assert!(
        native::RunRequest::parse(
            json!({"provider":"codex","prompt":"p".repeat(12000),"env":{"BIG":"x".repeat(8000)}})
        )
        .is_err()
    );
}

#[test]
fn env_values_never_appear_in_debug_or_empty_env_serialization() {
    let request = parse(json!({"SERVICE_SECRET":"s3cr3t-value-123"})).unwrap();
    let debug = format!("{request:?}");
    assert!(debug.contains("SERVICE_SECRET"));
    assert!(!debug.contains("s3cr3t-value-123"));
    // Older guest helpers reject unknown fields; an empty env is not sent.
    let plain = native::RunRequest::parse(json!({"provider":"codex","prompt":"hello"})).unwrap();
    assert!(serde_json::to_value(&plain).unwrap().get("env").is_none());
}

#[test]
fn run_records_carry_principal_and_env_names_but_never_values() {
    let request =
        parse(json!({"SERVICE_SECRET":"s3cr3t-value-123","API_BASE_URL":"https://x"})).unwrap();
    let id = "22222222222222222222222222222222";
    let principal = format!("service-token:{}", "a".repeat(32));
    let (record, detail) = service::new_run_entries(id, "worker", "ebox-a", &request, &principal);
    let stored = format!("{record}{detail}");
    assert!(!stored.contains("s3cr3t-value-123"));
    assert!(!stored.contains("https://x"));
    assert_eq!(detail["started_by"], principal);
    assert_eq!(
        detail["env_keys"],
        json!(["API_BASE_URL", "SERVICE_SECRET"])
    );
    // The record keeps its legacy shape so retained releases can read it.
    service::validate_record(&record, id).unwrap();
    run_details::validate(&detail).unwrap();
    let view = run_details::enrich(&record, Some(&detail), chrono::Utc::now());
    assert_eq!(view["started_by"], principal);
    assert_eq!(view["env_keys"], json!(["API_BASE_URL", "SERVICE_SECRET"]));
    let summary = run_details::summary(&record, Some(&detail));
    assert_eq!(summary["started_by"], principal);
    // Legacy sidecars predate principals and read as owner runs.
    let legacy =
        json!({"schema":1,"task":"t","started_at":null,"cancel_requested":false,"cancelled":false});
    run_details::validate(&legacy).unwrap();
    assert_eq!(
        run_details::enrich(&record, Some(&legacy), chrono::Utc::now())["started_by"],
        "owner"
    );
    for bad in ["service-token:XYZ", "someone", ""] {
        assert!(run_details::validate_principal(bad).is_err());
        let mut tampered = detail.clone();
        tampered["started_by"] = json!(bad);
        assert!(run_details::validate(&tampered).is_err());
    }
    let mut tampered = detail.clone();
    tampered["env_keys"] = json!(["PATH"]);
    assert!(run_details::validate(&tampered).is_err());
}

#[test]
fn redaction_removes_raw_and_json_escaped_values_without_growing_output() {
    let env = BTreeMap::from([
        ("LONG".to_string(), "abc\"def/ghi".to_string()),
        ("SHORT".to_string(), "abc".to_string()),
    ]);
    let raw = "short abc plain abc\"def/ghi json {\"v\":\"abc\\\"def/ghi\"}";
    let out = native::redact(raw, &env);
    assert!(!out.contains("abc\"def/ghi"));
    assert!(!out.contains("abc\\\"def/ghi"));
    assert!(out.contains(native::REDACTED));
    assert!(out.len() <= raw.len());
    // Values shorter than the minimum are left alone to avoid shredding output.
    assert!(out.contains("abc"));
}

#[cfg(unix)]
#[test]
fn env_reaches_only_the_native_run_and_is_scrubbed_from_output() {
    use enginebox_agents::security;
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let home = root.join("home");
    let workspace = root.join("workspace");
    security::private_dir(&home).unwrap();
    security::private_dir(&workspace).unwrap();
    security::safe_path(&home, ".local/bin/codex", true).unwrap();
    let exe = home.join(".local/bin/codex");
    // The version preflight records whether it saw the caller variable.
    std::fs::write(
        &exe,
        b"#!/bin/sh\nif [ \"$1\" = --version ]; then echo \"preflight=${SERVICE_SECRET:-unset}\" >>\"$HOME/.seen\"; echo 'codex-cli 0.163.0'; exit 0; fi\ncat >/dev/null\necho \"run=$SERVICE_SECRET home=$HOME\" >>\"$HOME/.seen\"\nprintf '{\"type\":\"item\",\"text\":\"%s\"}\\n' \"$SERVICE_SECRET\"\necho \"stderr $SERVICE_SECRET\" >&2\n",
    )
    .unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
    let secret = "tenant-secret-value-42";
    let request = parse(json!({"SERVICE_SECRET":secret})).unwrap();
    let out = native::execute(&request, &home, &workspace).unwrap();
    assert_eq!(out.exit_code, 0);
    let seen = std::fs::read_to_string(home.join(".seen")).unwrap();
    assert!(seen.contains("preflight=unset"), "{seen}");
    assert!(
        seen.contains(&format!("run={secret} home={}", home.display())),
        "{seen}"
    );
    assert!(!out.output.contains(secret), "{}", out.output);
    assert!(out.output.contains(native::REDACTED));
    // The variable is per run: a later run without it does not inherit it.
    std::fs::remove_file(home.join(".seen")).unwrap();
    let plain = native::RunRequest::parse(json!({"provider":"codex","prompt":"hello"})).unwrap();
    native::execute(&plain, &home, &workspace).unwrap();
    let seen = std::fs::read_to_string(home.join(".seen")).unwrap();
    assert!(seen.contains("run= home="), "{seen}");
}
