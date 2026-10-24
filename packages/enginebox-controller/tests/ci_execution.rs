use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::{
    net::TcpListener,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
#[path = "../../enginebox-ci/tests/support/mod.rs"]
mod template_fixture;

struct Server {
    child: Child,
    url: String,
    client: Client,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Server {
    fn start(root: &Path) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let mut command = Command::new(env!("CARGO_BIN_EXE_enginebox-controller"));
        command.env_clear();
        for key in ["PATH", "SystemRoot", "WINDIR", "COMSPEC", "PATHEXT"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        let child = command
            .env("ENGINEBOX_API_TOKEN", "x".repeat(32))
            .env("ENGINEBOX_CI_ROOT", root.join("worker"))
            .arg("--config")
            .arg(root.join("fleet.toml"))
            .arg("--state-dir")
            .arg(root.join("state"))
            .arg("--listen")
            .arg(address.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let server = Self {
            child,
            url: format!("http://{address}"),
            client: Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !server
            .client
            .get(format!("{}/healthz", server.url))
            .send()
            .is_ok_and(|r| r.status().is_success())
        {
            assert!(
                Instant::now() < deadline,
                "fixture controller did not start"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        server
    }
    fn request(&self, path: &str, body: Option<&Value>) -> (u16, Value) {
        let url = format!("{}{path}", self.url);
        let request = if let Some(body) = body {
            self.client.post(url).json(body)
        } else {
            self.client.get(url)
        };
        let response = request.bearer_auth("x".repeat(32)).send().unwrap();
        (response.status().as_u16(), response.json().unwrap())
    }
    fn template_state(&self, expected: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            let catalog = self.request("/v1/ci/templates", None).1;
            let state = &catalog["templates"][0]["nodes"][0];
            if state["status"] == expected {
                return catalog;
            }
            assert!(
                state["status"] != "failed" || expected == "failed",
                "Installation unexpectedly failed: {catalog}"
            );
            assert!(Instant::now() < deadline, "Expected {expected}: {catalog}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[test]
fn enabled_templates_install_on_later_nodes_gate_builds_and_reverify_after_restart() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("fleet.toml"),
        "schema_version=1\n[[hosts]]\nid='agents'\ntarget='agent@agent-host'\n",
    )
    .unwrap();
    let server = Server::start(tmp.path());
    let recipe = template_fixture::template();
    let node = serde_json::to_value(template_fixture::node()).unwrap();
    let recipe_value = serde_json::to_value(&recipe).unwrap();
    let path = "/v1/ci/templates/fixture-toolchain";
    let registered = server.request("/v1/ci/templates", Some(&json!({"manifest":recipe_value})));
    assert_eq!(registered.0, 200, "{registered:?}");
    assert_eq!(registered.1["templates"][0]["enabled"], false);
    assert_eq!(
        server
            .request(&format!("{path}/enable"), Some(&json!({})))
            .0,
        200
    );
    let mut trigger = json!({"schema_version":1,"request_id":"managed-build-1","event":"merge","sha":"a".repeat(40),"job":{"runner":"automation","template":"fixture-toolchain","runs-on":["enginebox","fixture-tool"],"shell":if cfg!(windows){"pwsh"}else{"bash"},"run":"ebox-fixture","timeout-minutes":1}});
    assert_eq!(server.request("/v1/ci/plan", Some(&trigger)).0, 400);
    // Enabling before any worker exists still drives installation on registration.
    assert_eq!(server.request("/v1/ci/nodes", Some(&node)).0, 200);
    let ready = server.template_state("ready");
    assert_eq!(
        ready["templates"][0]["nodes"][0]["capabilities_verified"],
        true
    );
    assert!(!ready.to_string().contains("agent@"));
    let plan = server.request("/v1/ci/plan", Some(&trigger));
    assert_eq!(plan.0, 200, "{plan:?}");
    assert_eq!(plan.1["template"]["revision"], 1);
    let (status, run) = server.request("/v1/ci/runs", Some(&trigger));
    assert_eq!(status, 202, "{run}");
    let id = run["job"]["id"].as_str().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let result = server.request(&format!("/v1/jobs/{id}"), None).1;
        if result["job"]["status"] == "succeeded" {
            assert_eq!(
                result["job"]["result"]["stdout"].as_str().unwrap().trim(),
                "fixture-1"
            );
            break;
        }
        assert_ne!(result["job"]["status"], "failed", "{result}");
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    let receipt_path = tmp
        .path()
        .join("worker/templates/fixture-toolchain")
        .join(recipe.digest())
        .join("receipt.json");
    let receipt: Value = serde_json::from_slice(&std::fs::read(&receipt_path).unwrap()).unwrap();
    assert_eq!(
        server
            .request(&format!("{path}/disable"), Some(&json!({})))
            .0,
        200
    );
    assert_eq!(server.request("/v1/ci/plan", Some(&trigger)).0, 400);
    assert!(receipt_path.is_file());
    assert_eq!(
        server
            .request(&format!("{path}/enable"), Some(&json!({})))
            .0,
        200
    );
    server.template_state("ready");
    drop(server);
    let server = Server::start(tmp.path());
    server.template_state("ready");
    let observed: Value = serde_json::from_slice(&std::fs::read(&receipt_path).unwrap()).unwrap();
    assert_eq!(
        observed["installation"], receipt["installation"],
        "Restart must probe without replaying installer"
    );
    let mut disabled_node = node.clone();
    disabled_node["disabled"] = json!(true);
    let before_disable =
        server.template_state("ready")["templates"][0]["nodes"][0]["job_id"].clone();
    assert_eq!(server.request("/v1/ci/nodes", Some(&disabled_node)).0, 200);
    assert_eq!(server.request("/v1/ci/plan", Some(&trigger)).0, 400);
    assert_eq!(server.request("/v1/ci/nodes", Some(&node)).0, 200);
    let reenabled = server.template_state("ready");
    assert_ne!(
        reenabled["templates"][0]["nodes"][0]["job_id"], before_disable,
        "Returning to an earlier node registration must reverify with a new attempt identity"
    );
    // A new revision invalidates old readiness; installer failure is durable and explicit.
    let mut broken = recipe_value.clone();
    broken["revision"] = json!(2);
    broken["install"] = json!("exit 1");
    assert_eq!(
        server
            .request("/v1/ci/templates", Some(&json!({"manifest":broken})))
            .0,
        200
    );
    let failed = server.template_state("failed");
    assert_eq!(
        failed["templates"][0]["nodes"][0]["error"],
        "installation-failed"
    );
    assert_eq!(server.request("/v1/ci/plan", Some(&trigger)).0, 400);
    let old_job = failed["templates"][0]["nodes"][0]["job_id"].clone();
    assert_eq!(
        server
            .request(&format!("{path}/retry"), Some(&json!({"node":"fixture"})))
            .0,
        200
    );
    let retried = server.template_state("failed");
    assert_ne!(retried["templates"][0]["nodes"][0]["job_id"], old_job);
    let mut corrected = recipe_value;
    corrected["revision"] = json!(3);
    assert_eq!(
        server
            .request("/v1/ci/templates", Some(&json!({"manifest":corrected})))
            .0,
        200
    );
    server.template_state("ready");
    // Desired enabled state survives revision updates, and the prior version remains on disk.
    assert!(receipt_path.is_file());
    trigger["request_id"] = json!("managed-build-2");
    assert_eq!(server.request("/v1/ci/plan", Some(&trigger)).0, 200);
}

#[test]
fn real_controller_runs_a_native_build_once_and_preserves_artifacts_across_restart() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("fleet.toml"),
        "schema_version=1\n[[hosts]]\nid='agents'\ntarget='agent@agent-host'\n",
    )
    .unwrap();
    let server = Server::start(tmp.path());
    let mut node = json!({"schema_version":1,"id":"build","target":"builder@agent-host","os":enginebox_ci::model::Os::current().unwrap(),"architecture":enginebox_ci::model::architecture().unwrap(),"image":"fixture-v1","capabilities":["pwsh"]});
    assert_eq!(server.request("/v1/ci/nodes", Some(&node)).0, 400);
    node["target"] = json!("local");
    assert_eq!(server.request("/v1/ci/nodes", Some(&node)).0, 200);
    let script = if cfg!(windows) {
        "New-Item -ItemType Directory -Path dist | Out-Null\nSet-Content -NoNewline -Path dist/build.txt -Value built"
    } else {
        "mkdir dist\nprintf built > dist/build.txt"
    };
    let mut trigger = json!({"schema_version":1,"request_id":"merge-build-1","event":"merge","sha":"a".repeat(40),"job":{"runner":"automation","runs-on":["enginebox","image:fixture-v1"],"shell":if cfg!(windows){"pwsh"}else{"bash"},"run":script,"artifacts":["dist"],"timeout-minutes":1}});
    let (code, first) = server.request("/v1/ci/runs", Some(&trigger));
    assert_eq!(code, 202);
    let id = first["job"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        server.request("/v1/ci/runs", Some(&trigger)).1["job"]["id"],
        id
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let result = server.request(&format!("/v1/jobs/{id}"), None).1;
        match result["job"]["status"].as_str().unwrap() {
            "succeeded" => {
                assert_eq!(
                    result["job"]["result"]["artifact_root"],
                    "artifacts/merge-build-1"
                );
                break;
            }
            "failed" => panic!("CI fixture build failed: {result}"),
            _ => {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
    let artifact = tmp
        .path()
        .join("worker")
        .join("artifacts")
        .join("merge-build-1")
        .join("dist")
        .join("build.txt");
    assert_eq!(
        std::fs::read_to_string(&artifact).unwrap_or_else(|error| {
            let inventory = std::fs::read_dir(artifact.parent().unwrap()).map(|entries| {
                entries
                    .map(|entry| entry.map(|e| e.file_name()))
                    .collect::<Vec<_>>()
            });
            panic!("Missing fixture artifact {artifact:?}: {error}; directory: {inventory:?}")
        }),
        "built"
    );
    assert!(
        tmp.path()
            .join(format!("state/ci-jobs/{id}.json"))
            .is_file()
    );
    assert!(
        !tmp.path().join(format!("state/jobs/{id}.json")).exists(),
        "Older-release job pruning must not reach the CI deduplication journal"
    );
    drop(server);
    let server = Server::start(tmp.path());
    let result = server.request("/v1/ci/runs", Some(&trigger));
    assert_eq!(result.0, 202);
    assert_eq!(result.1["job"]["id"], id);
    assert_eq!(result.1["job"]["status"], "succeeded");
    trigger["job"]["run"] = json!("echo changed");
    assert_eq!(server.request("/v1/ci/runs", Some(&trigger)).0, 400);
}
