use enginebox_agents::run_details;
#[cfg(unix)]
use enginebox_agents::{native, security};
use serde_json::json;

#[test]
fn usage_is_reported_only_from_terminal_provider_counters() {
    let codex = concat!(
        "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":100,\"output_tokens\":4,\"cached_input_tokens\":60}}\n",
        "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":200,\"output_tokens\":6,\"cached_input_tokens\":80}}\n"
    );
    assert_eq!(
        run_details::token_usage("codex", codex),
        json!({"input_tokens":200,"output_tokens":6,"cached_input_tokens":80,"scope":"session_total"})
    );
    let claude = concat!(
        "{\"type\":\"assistant\",\"message\":{\"usage\":{\"input_tokens\":900}}}\n",
        "{\"type\":\"result\",\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":100,\"cache_creation_input_tokens\":20,\"output_tokens\":7}}\n"
    );
    assert_eq!(
        run_details::token_usage("claude", claude),
        json!({"input_tokens":130,"output_tokens":7,"cached_input_tokens":100,"scope":"run"})
    );
    assert!(run_details::token_usage("claude",r#"{"type":"result","modelUsage":{"one":{"inputTokens":10,"cacheReadInputTokens":20,"cacheCreationInputTokens":30,"outputTokens":5}}}"#)["input_tokens"].is_null());
    for (provider, output) in [
        (
            "cursor",
            r#"{"type":"result","duration_ms":34,"usage":{"input_tokens":999}}"#,
        ),
        ("codex", "not JSON"),
        (
            "claude",
            r#"{"type":"result","usage":{"input_tokens":2,"output_tokens":7}}"#,
        ),
    ] {
        let usage = run_details::token_usage(provider, output);
        assert!(usage["input_tokens"].is_null());
        assert!(usage["cached_input_tokens"].is_null());
    }
    assert_eq!(
        run_details::token_usage(
            "codex",
            r#"{"type":"turn.completed","usage":{"input_tokens":0,"output_tokens":0,"cached_input_tokens":0}}"#
        )["input_tokens"],
        0
    );
    let resumed = concat!(
        "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":28572,\"output_tokens\":46,\"cached_input_tokens\":0}}\n",
        "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":60824,\"output_tokens\":92,\"cached_input_tokens\":20000}}\n"
    );
    assert_eq!(
        run_details::token_usage("codex", resumed),
        json!({"input_tokens":60824,"output_tokens":92,"cached_input_tokens":20000,"scope":"session_total"})
    );
    let claude_resume = r#"{"type":"result","usage":{"input_tokens":1,"cache_read_input_tokens":100,"cache_creation_input_tokens":10,"output_tokens":111},"modelUsage":{"opus":{"outputTokens":290}}}"#;
    assert_eq!(
        run_details::token_usage("claude", claude_resume)["output_tokens"],
        111
    );
    assert_eq!(
        run_details::token_usage("claude", claude_resume)["scope"],
        "run"
    );
    assert_eq!(run_details::token_usage("cursor", "")["scope"], "unknown");
    let overflow=json!({"type":"result","usage":{"input_tokens":u64::MAX,"cache_read_input_tokens":1,"cache_creation_input_tokens":0,"output_tokens":5}}).to_string();
    assert!(run_details::token_usage("claude", &overflow)["input_tokens"].is_null());
}

#[test]
fn summaries_are_bounded_private_views_with_truthful_elapsed() {
    let task = run_details::task_preview(&format!("do\nthis\t\u{1b}{}", "x".repeat(500)));
    assert_eq!(task.chars().count(), 240);
    assert!(!task.chars().any(char::is_control));
    let record = json!({"id":"a","provider":"claude","status":"failed","created_at":"2026-10-01T00:00:00Z","finished_at":"2026-10-01T00:00:05Z","result":{"output":"PRIVATE_RAW_TRANSCRIPT"}});
    let detail = json!({"schema":1,"task":task,"started_at":"2026-10-01T00:00:02Z","cancel_requested":true,"cancelled":true});
    run_details::validate(&detail).unwrap();
    let value = run_details::summary(&record, Some(&detail));
    assert_eq!(value["status"], "cancelled");
    assert_eq!(value["elapsed_seconds"], 3.0);
    assert!(!value.to_string().contains("PRIVATE_RAW_TRANSCRIPT"));
    let legacy = run_details::summary(&record, None);
    assert!(legacy["task"].is_null());
    assert!(legacy["elapsed_seconds"].is_null());
    assert_eq!(legacy["status"], "failed");
}

#[cfg(unix)]
fn home() -> (tempfile::TempDir, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().canonicalize().unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    security::private_dir(&home.join(".local/bin")).unwrap();
    let executable = home.join(".local/bin/codex");
    std::fs::write(&executable,b"#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'codex-cli 0.163.0'; exit 0; fi\nprintf '%s' $$ > \"$HOME/native-started\"\nsleep 60 &\nwait\n").unwrap();
    std::fs::set_permissions(executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    (temporary, home)
}

#[cfg(unix)]
#[test]
fn cancellation_targets_one_run_and_holds_lease_until_native_process_stops() {
    use std::time::{Duration, Instant};
    let (_temp, home) = home();
    let id = "11111111111111111111111111111111";
    let other = "22222222222222222222222222222222";
    let request = native::RunRequest::parse(json!({"provider":"codex","prompt":"test"})).unwrap();
    let worker_home = home.clone();
    let worker = std::thread::spawn(move || {
        native::execute_scoped(&request, &worker_home, &worker_home, Some(id)).unwrap()
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !home.join("native-started").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(native::lease_active(&home, "codex").unwrap());
    native::request_cancel(&home, "codex", other).unwrap();
    native::request_cancel(&home, "claude", id).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        !worker.is_finished(),
        "another run/provider must remain alive"
    );
    native::request_cancel(&home, "codex", id).unwrap();
    let result = worker.join().unwrap();
    assert_eq!(
        result.exit_code,
        enginebox_agents::process::CANCELLED_EXIT_CODE
    );
    assert!(!result.timed_out);
    assert!(!native::lease_active(&home, "codex").unwrap());
    assert!(!native::cancellation_requested(&home, "codex", id).unwrap());
    let pid: i32 = std::fs::read_to_string(home.join("native-started"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "native process must be reaped before lease is released"
    );
}

#[cfg(unix)]
#[test]
fn cancellation_before_launch_prevents_native_inference() {
    let (_temp, home) = home();
    let id = "33333333333333333333333333333333";
    native::request_cancel(&home, "codex", id).unwrap();
    let request = native::RunRequest::parse(json!({"provider":"codex","prompt":"test"})).unwrap();
    let result = native::execute_scoped(&request, &home, &home, Some(id)).unwrap();
    assert_eq!(
        result.exit_code,
        enginebox_agents::process::CANCELLED_EXIT_CODE
    );
    assert!(!home.join("native-started").exists());
    assert!(!native::lease_active(&home, "codex").unwrap());
    assert!(native::request_cancel(&home, "codex", "../anything").is_err());
    assert!(native::request_cancel(&home, "unknown", id).is_err());
}

#[cfg(unix)]
#[test]
fn cancellation_does_not_claim_success_while_detached_descendant_holds_lease() {
    use std::time::{Duration, Instant};
    let (_temp, home) = home();
    let binary = std::env::current_exe()
        .unwrap()
        .to_string_lossy()
        .replace('\'', "'\"'\"'");
    std::fs::write(home.join(".local/bin/codex"),format!("#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'codex-cli 0.163.0'; exit 0; fi\nexec '{binary}' --ignored --exact __detached_cancellation_fixture --nocapture\n")).unwrap();
    let id = "44444444444444444444444444444444";
    let request = native::RunRequest::parse(json!({"provider":"codex","prompt":"test"})).unwrap();
    let worker_home = home.clone();
    let worker = std::thread::spawn(move || {
        native::execute_scoped(&request, &worker_home, &worker_home, Some(id))
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !home.join("detached.pid").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let pid: i32 = std::fs::read_to_string(home.join("detached.pid"))
        .unwrap()
        .parse()
        .unwrap();
    native::request_cancel(&home, "codex", id).unwrap();
    let result = worker.join().unwrap();
    let still_active = native::lease_active(&home, "codex").unwrap();
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while native::lease_active(&home, "codex").unwrap() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        result.is_err(),
        "detached writer means cancellation cannot be confirmed"
    );
    assert!(still_active);
    assert!(!native::lease_active(&home, "codex").unwrap());
}

#[cfg(unix)]
#[test]
#[ignore = "subprocess fixture invoked by detached cancellation regression"]
fn __detached_cancellation_fixture() {
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
    if !home.join("fixture-parent").exists() {
        std::fs::write(home.join("fixture-parent"), b"1").unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "__detached_cancellation_fixture",
                "--nocapture",
            ])
            .spawn()
            .unwrap();
        let _ = child.wait();
    } else {
        assert!(unsafe { libc::setsid() } >= 0);
        std::fs::write(home.join("detached.pid"), std::process::id().to_string()).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}
