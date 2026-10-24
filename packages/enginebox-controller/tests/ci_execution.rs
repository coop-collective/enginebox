use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::{
    net::TcpListener,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

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
        "New-Item -ItemType Directory dist | Out-Null\nSet-Content -NoNewline -Path dist/build.txt -Value built"
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
    assert_eq!(
        std::fs::read_to_string(
            tmp.path()
                .join("worker/artifacts/merge-build-1/dist/build.txt")
        )
        .unwrap(),
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
