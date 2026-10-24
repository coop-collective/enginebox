//! Legacy Windows CI remains scoped; host agent execution must be refused.
#![cfg(windows)]
use enginebox_controller::{Controller, router};
use serde_json::{Value, json};
use std::{
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{io::AsyncWriteExt, process::Command};
const OWNER: &str = "fixture-controller-owner-credential-32";
async fn api(client: &reqwest::Client, url: &str, path: &str, body: Option<Value>) -> Value {
    let r = if let Some(b) = body {
        client.post(format!("{url}{path}")).json(&b)
    } else {
        client.get(format!("{url}{path}"))
    };
    let r = r.bearer_auth(OWNER).send().await.unwrap();
    let status = r.status();
    let body: Value = r.json().await.unwrap();
    assert!(status.is_success(), "{status}: {body}");
    body
}
async fn done(client: &reqwest::Client, url: &str, id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let b = api(client, url, &format!("/v1/jobs/{id}"), None).await;
        match b["job"]["status"].as_str().unwrap() {
            "succeeded" => return b["job"]["result"].clone(),
            "failed" => panic!("{b}"),
            _ => {}
        }
        assert!(Instant::now() < deadline, "job timed out: {b}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
fn fake_codex(root: &Path) {
    std::fs::write(root.join("fixture.rs"),r#"
use std::io::Read;
fn main(){
 let args=std::env::args().collect::<Vec<_>>();
 if args[1]=="--version" {println!("codex-cli 1.0.0");return}
 if args[1]=="login" {return}
 assert_eq!(args[1],"exec");
 assert!(std::env::var("ENGINEBOX_API_TOKEN").is_err());
 assert!(std::env::var("OWNER_SECRET_FIXTURE").is_err());
 let mut input=String::new();std::io::stdin().read_to_string(&mut input).unwrap();
 std::fs::write("agent-result.txt",input).unwrap();
 println!("{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":\"fixture completed\"}}}}");
}
"#).unwrap();
    assert!(
        std::process::Command::new("rustc")
            .arg(root.join("fixture.rs"))
            .arg("-o")
            .arg(root.join("codex.exe"))
            .status()
            .unwrap()
            .success()
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn windows_enrollment_runs_legacy_ci_refuses_host_agents_then_revokes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fake_codex(root);
    let fleet = root.join("fleet.toml");
    std::fs::write(
        &fleet,
        "schema_version=1\n[[hosts]]\nid='existing-metal'\ntarget='unused-fixture'\n",
    )
    .unwrap();
    let controller = Controller::new(&fleet, &root.join("state"), OWNER.into()).unwrap();
    controller.activate().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server =
        tokio::spawn(async move { axum::serve(listener, router(controller)).await.unwrap() });
    // Exercise the real supervisor front door: node bearer auth must survive it.
    let install = enginebox_install::release::Installation::inspect(
        root.canonicalize().unwrap().join("supervisor"),
    )
    .unwrap();
    install.initialize().unwrap();
    let version = semver::Version::new(0, 3, 0);
    let dir = install.release_dir(&version);
    std::fs::create_dir_all(&dir).unwrap();
    enginebox_install::release::atomic_json(
        &dir.join("manifest.json"),
        &enginebox_install::release::Manifest {
            version: version.clone(),
            target: enginebox_install::release::current_target().into(),
            archive_sha256: "0".repeat(64),
            state_schema: 1,
            read_schema_min: 1,
            read_schema_max: 1,
            dashboard: false,
        },
    )
    .unwrap();
    install.activate(&version).unwrap();
    let supervisor = enginebox_install::supervisor::Supervisor::new(
        enginebox_install::supervisor::SupervisorConfig {
            install,
            state_dir: None,
            fleet_config: fleet,
            api_listen: "127.0.0.1:8765".parse().unwrap(),
            dashboard_listen: None,
            token: OWNER.into(),
            drain_timeout: Duration::from_secs(10),
        },
        enginebox_install::supervisor::Candidate::external(version, 1, url, None),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let proxy = tokio::spawn(async move {
        axum::serve(listener, supervisor.api_router())
            .await
            .unwrap()
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let invite=api(&client,&url,"/v1/contributors/enrollments",Some(json!({"label":"Windows fixture","budget":{"cpu":1,"memory_gib":2,"disk_gib":10},"workloads":{"ci":true,"agents":true}}))).await;
    let config = root.join("installation/private/worker.json");
    let mut register = Command::new(env!("CARGO_BIN_EXE_enginebox-node"))
        .args(["register-worker", "--controller", &url, "--config"])
        .arg(&config)
        .args([
            "--cpu",
            "1",
            "--memory-gib",
            "2",
            "--disk-gib",
            "10",
            "--test-loopback-http",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    register
        .stdin
        .take()
        .unwrap()
        .write_all(invite["code"].as_str().unwrap().as_bytes())
        .await
        .unwrap();
    assert!(register.wait().await.unwrap().success());
    let identity: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
    let denied = client
        .get(format!("{url}/v1/contributors"))
        .bearer_auth(identity["token"].as_str().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);
    let mut paths = vec![root.to_owned()];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let mut node = Command::new(env!("CARGO_BIN_EXE_enginebox-node"))
        .args(["run-worker", "--config"])
        .arg(&config)
        .arg("--test-loopback-http")
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("OWNER_SECRET_FIXTURE", "must-not-reach-workload")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let catalog = api(&client, &url, "/v1/contributors", None).await;
        if catalog["nodes"][0]["tools"]["ci"] == true
            && catalog["nodes"][0]["tools"]["codex"] == false
        {
            break;
        }
        assert!(
            node.try_wait().unwrap().is_none(),
            "node exited before ready"
        );
        assert!(Instant::now() < deadline, "node not ready: {catalog}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let path = format!("/v1/contributors/{}/runs", invite["id"].as_str().unwrap());
    let run = json!({"request_id":"ci-proof","kind":"ci","request":{"schema_version":1,"request_id":"ci-proof","event":"workflow_dispatch","sha":"a".repeat(40),"job":{"runner":"automation","runs-on":["windows","pwsh"],"shell":"pwsh","run":"if ($env:OWNER_SECRET_FIXTURE) { throw 'Credential escaped' }; New-Item -ItemType Directory dist | Out-Null; Set-Content -NoNewline -Path dist/result.txt -Value built","artifacts":["dist"],"timeout-minutes":1}}});
    let submitted = api(&client, &url, &path, Some(run.clone())).await;
    let id = submitted["job"]["id"].as_str().unwrap();
    let result = done(&client, &url, id).await;
    assert_eq!(result["result"]["sha"], "a".repeat(40));
    assert_eq!(
        std::fs::read_to_string(
            root.join("installation/work/ci/artifacts/ci-proof/dist/result.txt")
        )
        .unwrap_or_else(|e| panic!(
            "{e}; result={result}; artifacts={:?}",
            std::fs::read_dir(root.join("installation/work/ci/artifacts/ci-proof/dist")).map(
                |entries| entries
                    .map(|e| e.map(|e| e.file_name()))
                    .collect::<Vec<_>>()
            )
        )),
        "built"
    );
    assert_eq!(api(&client, &url, &path, Some(run)).await["job"]["id"], id);
    let denied = client.post(format!("{url}{path}")).bearer_auth(OWNER).json(&json!({"request_id":"agent-proof","kind":"agent","request":{"provider":"codex","prompt":"fixture-native-prompt","timeout_seconds":30}})).send().await.unwrap();
    assert!(!denied.status().is_success());
    assert!(!root.join("installation/work/agents/agent-proof").exists());
    // Revocation must terminate a running native job and its detached descendant.
    let pid_file = root.join("descendant.pid");
    let script = format!(
        "$p=Start-Process -FilePath (Get-Command pwsh).Source -ArgumentList @('-NoProfile','-Command','Start-Sleep -Seconds 90') -PassThru; Set-Content -LiteralPath '{}' -Value $p.Id; Start-Sleep -Seconds 90",
        pid_file.to_string_lossy().replace("'", "''")
    );
    api(&client,&url,&path,Some(json!({"request_id":"revoke-proof","kind":"ci","request":{"schema_version":1,"request_id":"revoke-proof","event":"workflow_dispatch","sha":"a".repeat(40),"job":{"runner":"automation","runs-on":["windows","pwsh"],"shell":"pwsh","run":script,"artifacts":[],"timeout-minutes":1}}}))).await;
    let deadline = Instant::now() + Duration::from_secs(15);
    while !pid_file.is_file() {
        assert!(Instant::now() < deadline, "child process never started");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let pid: u32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    api(
        &client,
        &url,
        &format!("/v1/contributors/{}/revoke", invite["id"].as_str().unwrap()),
        Some(json!({})),
    )
    .await;
    let status = tokio::time::timeout(Duration::from_secs(15), node.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(!status.success());
    let check = Command::new("pwsh")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ exit 1 }}"),
        ])
        .status()
        .await
        .unwrap();
    assert!(
        check.success(),
        "revocation left the worker's descendant alive"
    );
    server.abort();
    proxy.abort();
}
