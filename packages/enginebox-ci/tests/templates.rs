mod support;
use enginebox_ci::{
    executor,
    provision::{self, InstallRequest, Operation},
    templates::{Binding, Template},
};
use serde_json::json;
use support::{node, template};

fn install(manifest: Template, attempt: char) -> InstallRequest {
    InstallRequest {
        operation: Operation::InstallTemplate,
        node: node(),
        manifest,
        attempt: attempt.to_string().repeat(64),
        reinstall: false,
    }
}

#[test]
fn installed_toolchain_is_reused_verified_and_available_to_a_real_build() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("worker");
    let request = install(template(), 'a');
    let outcome = provision::install_at(&request, &root).unwrap();
    assert!(outcome.ok, "{outcome:?}");
    let receipt = outcome.receipt.unwrap();
    receipt.validate(&request.node, &request.manifest).unwrap();
    let directory = root
        .join("templates/fixture-toolchain")
        .join(request.manifest.digest());
    let prefix = directory.join(&receipt.installation);
    assert!(prefix.join("bin").is_dir());
    let mut repeat = install(template(), 'b');
    let observed = provision::install_at(&repeat, &root)
        .unwrap()
        .receipt
        .unwrap();
    assert_eq!(
        observed.installation, receipt.installation,
        "Installer mkdir would fail if replayed"
    );
    let binding = Binding {
        manifest: request.manifest.clone(),
        receipt: observed,
    };
    let job = serde_json::from_value(json!({"schema_version":1,"request_id":"template-build","event":"merge","sha":"a".repeat(40),"job":{"runner":"automation","template":"fixture-toolchain","runs-on":["enginebox","fixture-tool","image:fixture-r1"],"shell":if cfg!(windows) { "pwsh" } else { "bash" },"run":"ebox-fixture","timeout-minutes":1}})).unwrap();
    let result = executor::execute_managed_at(&node(), &job, &root, Some(&binding)).unwrap();
    assert_eq!(result["stdout"].as_str().unwrap().trim(), "fixture-1");
    assert!(
        executor::execute_at(&node(), &job, &root).is_err(),
        "Managed job cannot fall back to declared capabilities"
    );
    let executable = prefix.join(if cfg!(windows) {
        "bin/ebox-fixture.ps1"
    } else {
        "bin/ebox-fixture"
    });
    std::fs::write(
        &executable,
        if cfg!(windows) {
            "Write-Output 'wrong-version'"
        } else {
            "#!/bin/sh\nprintf wrong-version"
        },
    )
    .unwrap();
    assert!(
        executor::execute_managed_at(&node(), &job, &root, Some(&binding)).is_err(),
        "Every build must recheck the actual tool version"
    );
    let failed = provision::install_at(&repeat, &root).unwrap();
    assert!(!failed.ok);
    assert_eq!(failed.phase, "verification");
    repeat.reinstall = true;
    let retried = provision::install_at(&repeat, &root).unwrap();
    assert!(retried.ok, "{retried:?}");
    assert_eq!(retried.receipt.unwrap().installation, "b".repeat(64));
    assert!(prefix.exists(), "Retry retains old files for investigation");
    assert!(
        executor::execute_managed_at(&node(), &job, &root, Some(&binding)).is_err(),
        "Superseded binding is fenced"
    );
}

#[test]
fn failures_are_private_and_uncertain_installers_never_replay_automatically() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("worker");
    let mut recipe = template();
    recipe.install = if cfg!(windows) {
        "Set-Content marker touched\nthrow 'private-diagnostic'"
    } else {
        "touch marker\nprintf private-diagnostic >&2\nexit 1"
    }
    .into();
    let request = install(recipe, 'c');
    let failed = provision::install_at(&request, &root).unwrap();
    assert!(!failed.ok);
    assert_eq!(failed.phase, "installation");
    assert!(
        !serde_json::to_string(&failed)
            .unwrap()
            .contains("private-diagnostic")
    );
    let directory = root
        .join("templates/fixture-toolchain")
        .join(request.manifest.digest());
    assert!(!directory.join("receipt.json").exists());
    let marker = directory.join(&request.attempt).join("marker");
    std::fs::remove_file(&marker).unwrap();
    let repeated = provision::install_at(&request, &root).unwrap();
    assert_eq!(repeated.phase, "preparation");
    assert!(!marker.exists());
    let guard = enginebox_core::state::lock(&root.join("worker.lock"), false).unwrap();
    assert!(
        provision::install_at(&install(template(), 'd'), &root).is_err(),
        "A build/Actions runner excludes installation"
    );
    drop(guard);
    let mut oversized = template();
    oversized.minimum.disk_mib = 4_194_304;
    let failed = provision::install_at(&install(oversized, 'e'), &root).unwrap();
    assert_eq!(failed.phase, "capacity");
    assert!(!failed.ok);
}

#[test]
fn recipes_require_pinned_assets_complete_probes_and_private_paths() {
    let value = serde_json::to_value(template()).unwrap();
    for change in [
        json!({"paths":["../outside"]}),
        json!({"paths":["C:/outside"]}),
        json!({"environment":{"PATH":"bin"}}),
        json!({"verify":[]}),
        json!({"capabilities":["linux"]}),
        json!({"assets":[{"name":"archive","url":"https://example.com/tool","sha256":"latest","max_bytes":100}]}),
        json!({"assets":[{"name":"archive","url":"https://user:password@example.com/tool","sha256":"a".repeat(64),"max_bytes":100}]}),
        json!({"assets":[{"name":"archive","url":"http://example.com/tool","sha256":"a".repeat(64),"max_bytes":100}]}),
    ] {
        let mut changed = value.clone();
        changed
            .as_object_mut()
            .unwrap()
            .extend(change.as_object().unwrap().clone());
        assert!(
            serde_json::from_value::<Template>(changed)
                .unwrap()
                .validate()
                .is_err()
        );
    }
    let mut request = install(template(), 'a');
    request.node.architecture = if request.node.architecture == "x64" {
        "arm64"
    } else {
        "x64"
    }
    .into();
    let tmp = tempfile::tempdir().unwrap();
    assert!(provision::install_at(&request, &tmp.path().join("worker")).is_err());
}

#[test]
fn shipped_toolchain_recipes_are_pinned_and_validate_on_all_native_platforms() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/ci/templates");
    for os in ["linux", "macos", "windows"] {
        for arch in ["x64", "arm64"] {
            let manifest: Template = serde_json::from_slice(
                &std::fs::read(root.join(format!("node24-{os}-{arch}.json"))).unwrap(),
            )
            .unwrap();
            manifest.validate().unwrap();
            assert!(
                manifest.assets[0]
                    .url
                    .starts_with("https://nodejs.org/dist/v24.22.1/")
            );
            assert_eq!(manifest.verify[0].expected_stdout, "v24.22.1");
        }
    }
}

#[cfg(unix)]
#[test]
fn toolchain_environment_rejects_symlink_escapes() {
    let tmp = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink("/tmp", tmp.path().join("bin")).unwrap();
    assert!(
        provision::environment(
            &mut std::process::Command::new("true"),
            &template(),
            tmp.path()
        )
        .is_err()
    );
}
