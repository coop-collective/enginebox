#![cfg(unix)]
//! Exercise the actual guest supervisor with a fixture native listener. No Incus,
//! GitHub credentials, mutable process environment or live runner is involved.
use serde_json::Value;
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
    time::Instant,
};

fn supervise(
    idle: f64,
    job: f64,
    runner: &str,
    original_hook: Option<&str>,
) -> (Value, f64, tempfile::TempDir) {
    supervise_with_retirement(idle, job, runner, original_hook, false)
}
fn supervise_with_retirement(
    idle: f64,
    job: f64,
    runner: &str,
    original_hook: Option<&str>,
    retired: bool,
) -> (Value, f64, tempfile::TempDir) {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("retire.py"),
        include_str!("../src/runner_retire.py"),
    )
    .unwrap();
    let run = root.path().join("run.sh");
    fs::write(&run, format!("#!/bin/sh\nset -eu\n{runner}\n")).unwrap();
    fs::set_permissions(&run, fs::Permissions::from_mode(0o700)).unwrap();
    let mut command = Command::new("python3");
    command
        .args([
            "-c",
            include_str!("../src/runner_supervisor.py"),
            &idle.to_string(),
            &job.to_string(),
            if retired { "true" } else { "false" },
        ])
        .current_dir(root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("ACTIONS_RUNNER_HOOK_JOB_STARTED")
        .env_remove("ENGINEBOX_ORIGINAL_JOB_STARTED_HOOK");
    if let Some(hook) = original_hook {
        let path = root.path().join("original.sh");
        fs::write(&path, format!("#!/bin/sh\nset -eu\n{hook}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        command.env("ACTIONS_RUNNER_HOOK_JOB_STARTED", path);
    }
    let started = Instant::now();
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"opaque-fixture-jit\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result = serde_json::from_slice(
        &fs::read(root.path().join(".enginebox-lifetime/result.json")).unwrap(),
    )
    .unwrap();
    (result, started.elapsed().as_secs_f64(), root)
}

#[test]
fn late_assignment_receives_full_job_timeout_and_preserves_preparation_hook() {
    let (result, duration, root) = supervise(
        3.0,
        3.0,
        "sleep 1.5\n\"$ACTIONS_RUNNER_HOOK_JOB_STARTED\"\nsleep 2.0\nprintf done > job-completed",
        Some("printf verified > preparation-hook-ran"),
    );
    assert_eq!(result["outcome"], "job_finished");
    assert_eq!(result["exit_code"], 0);
    assert!(
        duration > 3.0,
        "The late job must outlive the idle deadline"
    );
    assert!(root.path().join("job-completed").exists());
    assert_eq!(
        fs::read_to_string(root.path().join("preparation-hook-ran")).unwrap(),
        "verified"
    );
}

#[test]
fn idle_expiry_does_not_kill_listener_and_retirement_waits_for_natural_exit() {
    let (result, duration, root) = supervise(
        0.2,
        5.0,
        "sleep 0.5\ntest -e .enginebox-lifetime/idle-expired\nprintf alive > survived-idle-deadline\npython3 retire.py .enginebox-lifetime > retirement.json\nsleep 0.2\nprintf natural > listener-exit",
        None,
    );
    assert_eq!(result["outcome"], "idle_timeout");
    assert!(result["job_started_at_ms"].is_null());
    assert!(duration < 5.0);
    assert!(root.path().join("survived-idle-deadline").exists());
    assert!(root.path().join("listener-exit").exists());
    assert!(
        !fs::read_to_string(root.path().join(".enginebox-lifetime/result.json"))
            .unwrap()
            .contains("opaque-fixture-jit")
    );
}

#[test]
fn assigned_job_has_its_own_bounded_deadline() {
    let (result, duration, _) = supervise(
        5.0,
        0.2,
        "\"$ACTIONS_RUNNER_HOOK_JOB_STARTED\"\nsleep 20",
        None,
    );
    assert_eq!(result["outcome"], "job_timeout");
    assert!(result["job_started_at_ms"].is_u64());
    assert!(duration < 5.0);
}

#[test]
fn retirement_before_hook_preserves_an_already_accepted_job() {
    let (result, _, root) = supervise(
        5.0,
        5.0,
        "python3 retire.py .enginebox-lifetime > retirement.json\n\"$ACTIONS_RUNNER_HOOK_JOB_STARTED\"\nprintf unsafe > job-completed",
        None,
    );
    assert_eq!(result["outcome"], "job_finished");
    assert!(result["job_started_at_ms"].is_u64());
    assert!(root.path().join("job-completed").exists());
}

#[test]
fn retirement_preserves_an_already_assigned_job() {
    let (result, _, root) = supervise(
        5.0,
        5.0,
        "\"$ACTIONS_RUNNER_HOOK_JOB_STARTED\"\npython3 retire.py .enginebox-lifetime > retirement.json\nsleep 0.2\nprintf done > job-completed",
        None,
    );
    assert_eq!(result["outcome"], "job_finished");
    assert!(root.path().join("job-completed").exists());
    let retirement: Value =
        serde_json::from_slice(&fs::read(root.path().join("retirement.json")).unwrap()).unwrap();
    assert_eq!(retirement["reserve_retire"], "busy");
}

#[test]
fn retirement_during_provisioning_never_launches_the_listener() {
    let (result, _, root) =
        supervise_with_retirement(5.0, 5.0, "printf unsafe > listener-started", None, true);
    assert_eq!(result["outcome"], "idle_timeout");
    assert!(!root.path().join("listener-started").exists());
    assert!(root.path().join("_diag").exists());
}
