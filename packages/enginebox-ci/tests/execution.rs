use enginebox_ci::{
    Node, Request, executor,
    model::{Os, architecture},
};
use serde_json::json;

fn node() -> Node {
    serde_json::from_value(json!({"schema_version":1,"id":"fixture","target":"local","os":Os::current().unwrap(),"architecture":architecture().unwrap(),"image":"fixture-v1","capabilities":["cmake"]})).unwrap()
}
fn request(script: &str) -> Request {
    serde_json::from_value(json!({"schema_version":1,"request_id":"merge-test","event":"merge","sha":"a".repeat(40),"job":{"runner":"automation","runs-on":["enginebox","image:fixture-v1"],"shell":"bash","run":script,"timeout-minutes":1}})).unwrap()
}

#[test]
fn capabilities_are_conjunctive_case_insensitive_and_cannot_spoof_platform() {
    let mut n = node();
    assert!(n.matches(&["ENGINEBOX".into(), "cmake".into()]));
    assert!(!n.matches(&["enginebox".into(), "xcode".into()]));
    n.disabled = true;
    assert!(!n.matches(&["cmake".into()]));
    n.capabilities.push("Windows".into());
    assert!(n.validate().is_err());
    n.capabilities = vec!["image:fake".into()];
    assert!(n.validate().is_err());
}

#[test]
fn examples_are_valid_native_jobs_with_exact_platform_constraints() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/ci");
    for name in ["ios", "android", "windows", "linux"] {
        let n: Node = serde_json::from_slice(
            &std::fs::read(root.join(format!("nodes/{name}.json"))).unwrap(),
        )
        .unwrap();
        let r: Request = serde_json::from_slice(
            &std::fs::read(root.join(format!("automations/{name}.json"))).unwrap(),
        )
        .unwrap();
        n.validate().unwrap();
        r.validate().unwrap();
        let enginebox_ci::Job::Automation {
            runs_on, artifacts, ..
        } = &r.job
        else {
            panic!()
        };
        assert!(n.matches(runs_on));
        assert!(!artifacts.is_empty());
        assert!(runs_on.contains(&format!("image:{}", n.image)));
    }
    let r: Request =
        serde_json::from_slice(&std::fs::read(root.join("github-trigger.json")).unwrap()).unwrap();
    r.validate().unwrap();
}

#[test]
fn rejects_credentials_traversal_and_wrong_worker_before_execution() {
    let mut r = serde_json::to_value(request("true")).unwrap();
    r["job"]["checkout"] = json!({"url":"https://token@github.com/o/r.git"});
    assert!(
        serde_json::from_value::<Request>(r.clone())
            .unwrap()
            .validate()
            .is_err()
    );
    r["job"].as_object_mut().unwrap().remove("checkout");
    for path in [
        "../secret",
        "/etc/passwd",
        "build/../../secret",
        "build/.git",
        "C:\\secret",
        "build/*",
    ] {
        r["job"]["artifacts"] = json!([path]);
        assert!(
            serde_json::from_value::<Request>(r.clone())
                .unwrap()
                .validate()
                .is_err()
        );
    }
    let mut n = node();
    n.os = if n.os == Os::Linux {
        Os::Windows
    } else {
        Os::Linux
    };
    let tmp = tempfile::tempdir().unwrap();
    assert!(executor::execute_at(&n, &request("true"), tmp.path()).is_err());
}

#[cfg(unix)]
#[test]
fn native_script_failure_is_red_and_success_retains_only_declared_artifacts() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("worker");
    let mut r = serde_json::to_value(request("test -z \"${ENGINEBOX_API_TOKEN:-}\"\ntest -z \"${ENGINEBOX_CI_GITHUB_TOKEN:-}\"\nmkdir -p dist\nprintf artifact > dist/build.txt\nprintf '%s' \"$ENGINEBOX_SHA\"\ntouch scratch")).unwrap();
    r["job"]["artifacts"] = json!(["dist"]);
    let r: Request = serde_json::from_value(r).unwrap();
    let result = executor::execute_at(&node(), &r, &root).unwrap();
    assert_eq!(result["stdout"], "a".repeat(40));
    assert_eq!(
        std::fs::read_to_string(root.join("artifacts/merge-test/dist/build.txt")).unwrap(),
        "artifact"
    );
    assert!(!root.join("scratch").exists());
    assert!(
        executor::execute_at(&node(), &r, &root).is_err(),
        "existing artifacts cannot be overwritten"
    );
    assert!(executor::execute_at(&node(), &request("false\nprintf false-green"), &root).is_err());
    let guard = enginebox_core::state::lock(&root.join("worker.lock"), false).unwrap();
    assert!(executor::execute_at(&node(), &request("true"), &root).is_err());
    drop(guard);
}

#[cfg(unix)]
#[test]
fn artifact_export_refuses_symlink_escapes() {
    let tmp = tempfile::tempdir().unwrap();
    let mut r = serde_json::to_value(request("ln -s /tmp dist")).unwrap();
    r["job"]["artifacts"] = json!(["dist"]);
    assert!(
        executor::execute_at(
            &node(),
            &serde_json::from_value(r).unwrap(),
            &tmp.path().join("worker")
        )
        .is_err()
    );
}

#[cfg(unix)]
#[test]
fn successful_compiler_stderr_above_transport_limit_is_supported() {
    let tmp = tempfile::tempdir().unwrap();
    let result = executor::execute_at(
        &node(),
        &request("head -c 131072 /dev/zero >&2\nprintf built"),
        &tmp.path().join("worker"),
    )
    .unwrap();
    assert_eq!(result["stdout"], "built");
}

#[cfg(windows)]
#[test]
fn powershell_build_uses_a_canonical_worker_path_and_retains_artifacts() {
    let tmp = tempfile::tempdir().unwrap();
    // The CLI canonicalizes its private root before invoking the executor.
    let root = tmp.path().canonicalize().unwrap();
    let mut n = node();
    n.capabilities.push("pwsh".into());
    let mut r = serde_json::to_value(request("New-Item -ItemType Directory dist | Out-Null\nSet-Content -NoNewline -Path dist/build.txt -Value built")).unwrap();
    r["job"]["shell"] = json!("pwsh");
    r["job"]["artifacts"] = json!(["dist"]);
    let result = executor::execute_at(&n, &serde_json::from_value(r).unwrap(), &root);
    assert!(result.is_ok(), "PowerShell execution failed: {result:?}");
    assert_eq!(
        std::fs::read_to_string(root.join("artifacts/merge-test/dist/build.txt")).unwrap(),
        "built"
    );
}
