use serde_json::{Value, json};
use std::process::Command;

fn cli() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ebox"));
    command.env_clear();
    for key in ["PATH", "SystemRoot", "WINDIR", "COMSPEC", "PATHEXT"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
}

#[test]
fn ci_inspection_is_local_and_unavailable_to_agent_guests() {
    let output = cli().args(["ci", "inspect"]).output().unwrap();
    assert!(output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["image_provisioning"], false);
    assert!(
        !cli()
            .args(["--host", "elsewhere", "ci", "inspect"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        !cli()
            .env("ENGINEBOX_GUEST", "1")
            .args(["ci", "inspect"])
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[cfg(unix)]
#[test]
fn execute_cli_uses_private_workspaces_without_forwarding_operator_credentials() {
    use std::{io::Write, process::Stdio};
    let tmp = tempfile::tempdir().unwrap();
    let execution = json!({"node":{"schema_version":1,"id":"ci","target":"local","os":enginebox_ci::model::Os::current().unwrap(),"architecture":enginebox_ci::model::architecture().unwrap(),"image":"fixture","capabilities":[]},"request":{"schema_version":1,"request_id":"merge-cli","event":"merge","sha":"a".repeat(40),"job":{"runner":"automation","runs-on":["enginebox"],"shell":"bash","run":"test -z \"${ENGINEBOX_API_TOKEN:-}\"\ntest -z \"${ENGINEBOX_CI_GITHUB_TOKEN:-}\"\necho built","timeout-minutes":1}}});
    let mut child = cli()
        .env("ENGINEBOX_CI_ROOT", tmp.path().join("ci"))
        .env("ENGINEBOX_API_TOKEN", "fixture-secret")
        .env("ENGINEBOX_CI_GITHUB_TOKEN", "fixture-secret")
        .args(["ci", "execute"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(execution.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["stdout"], "built\n");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("fixture-secret"));
}

#[cfg(unix)]
#[test]
fn runner_adapter_passes_token_only_to_configuration_and_refuses_existing_registration() {
    use std::{fs, os::unix::fs::PermissionsExt};
    let tmp = tempfile::tempdir().unwrap();
    let runner = tmp.path().join("runner");
    fs::create_dir(&runner).unwrap();
    fs::set_permissions(&runner, fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir(runner.join("bin")).unwrap();
    let capture = tmp.path().join("args");
    let binary = runner.join("bin/Runner.Listener");
    fs::write(&binary,format!("#!/bin/sh\nset -eu\nprintf '%s\\n' \"$@\" >> {}\nif [ \"$1\" = configure ]; then test \"$ACTIONS_RUNNER_INPUT_TOKEN\" = fixture-token; else test -z \"${{ACTIONS_RUNNER_INPUT_TOKEN:-}}\"; fi\ntest -z \"${{ENGINEBOX_CI_REGISTRATION_TOKEN:-}}\"\n", enginebox_core::transport::shell_quote(capture.to_str().unwrap()))).unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    let manifest = tmp.path().join("node.json");
    fs::write(&manifest,json!({"schema_version":1,"id":"ci","target":"builder@ci-host","os":enginebox_ci::model::Os::current().unwrap(),"architecture":enginebox_ci::model::architecture().unwrap(),"image":"fixture","capabilities":["cmake"]}).to_string()).unwrap();
    let run = || {
        cli()
            .env("ENGINEBOX_CI_ROOT", tmp.path().join("ci"))
            .env("ENGINEBOX_CI_REGISTRATION_TOKEN", "fixture-token")
            .args(["ci", "github-runner", "--manifest"])
            .arg(&manifest)
            .arg("--directory")
            .arg(&runner)
            .args(["--repository", "owner/repo"])
            .output()
            .unwrap()
    };
    let output = run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let args = fs::read_to_string(&capture).unwrap();
    assert!(
        args.contains("--ephemeral\n")
            && args.contains("--disableupdate\n")
            && args.ends_with("run\n")
    );
    assert!(!args.contains("fixture-token") && !args.contains("--replace"));
    fs::write(runner.join(".runner"), "existing").unwrap();
    assert!(!run().status.success());
    assert_eq!(fs::read_to_string(&capture).unwrap(), args);
}
