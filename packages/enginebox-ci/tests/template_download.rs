//! Opt-in network qualification of the shipped recipe on this native platform.
use enginebox_ci::{
    Node, executor,
    model::{Os, architecture},
    provision::{self, InstallRequest, Operation},
    templates::{Binding, Template},
};
use serde_json::json;

#[test]
#[ignore = "downloads the pinned official Node archive into a disposable worker; CI opts in"]
fn installed_node_recipe_builds_with_the_downloaded_executable() {
    let os = match Os::current().unwrap() {
        Os::Linux => "linux",
        Os::Macos => "macos",
        Os::Windows => "windows",
    };
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
        "../../examples/ci/templates/node24-{os}-{}.json",
        architecture().unwrap()
    ));
    let manifest: Template = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("worker");
    let node = Node {
        schema_version: 1,
        id: "qualification".into(),
        target: "local".into(),
        os: Os::current().unwrap(),
        architecture: architecture().unwrap().into(),
        image: "unprepared".into(),
        capabilities: vec![],
        disabled: false,
    };
    let request = InstallRequest {
        operation: Operation::InstallTemplate,
        node,
        manifest,
        attempt: "f".repeat(64),
        reinstall: false,
    };
    let outcome = provision::install_at(&request, &root).unwrap();
    if !outcome.ok {
        let directory = root
            .join("templates")
            .join(&request.manifest.id)
            .join(request.manifest.digest());
        let diagnostic = std::fs::read_to_string(
            directory.join(&request.attempt).join("error.json"),
        )
        .or_else(|_| {
            std::fs::read_to_string(directory.join(format!("{}.error.json", request.attempt)))
        });
        // This opt-in test uses only the public pinned recipe and disposable state.
        panic!("Pinned recipe failed: {outcome:?}; fixture diagnostic: {diagnostic:?}");
    }
    let receipt = outcome.receipt.unwrap();
    let reverified = provision::install_at(&request, &root).unwrap();
    assert!(reverified.ok, "{reverified:?}");
    assert_eq!(
        reverified.receipt.unwrap().installation,
        receipt.installation
    );
    let binding = Binding {
        manifest: request.manifest,
        receipt,
    };
    let job = serde_json::from_value(json!({"schema_version":1,"request_id":"pinned-node-build","event":"workflow_dispatch","sha":"a".repeat(40),"job":{"runner":"automation","template":binding.manifest.id,"runs-on":["enginebox","node-24"],"shell":binding.manifest.shell,"run":"node -e 'const fs = require(\"node:fs\"); fs.mkdirSync(\"dist\"); fs.writeFileSync(\"dist/version.txt\", process.version); console.log(process.execPath)'","artifacts":["dist"],"timeout-minutes":1}})).unwrap();
    let result = executor::execute_managed_at(&request.node, &job, &root, Some(&binding)).unwrap();
    let executable = std::fs::canonicalize(result["stdout"].as_str().unwrap().trim()).unwrap();
    assert!(
        executable.starts_with(root.canonicalize().unwrap()),
        "Build must use the privately installed executable"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("artifacts/pinned-node-build/dist/version.txt")).unwrap(),
        "v24.22.1"
    );
}
