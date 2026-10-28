//! Real native managed-host transport with disposable controller state. No host
//! helper, project, pool or VM is installed or changed by these tests.
use serde_json::json;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use std::process::Stdio;
use tokio::process::Command;

#[tokio::test]
async fn malformed_private_registration_cannot_run_or_disclose_its_credential() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let path = root.join("host.json");
    enginebox_install::release::atomic_json(&path, &json!({"schema":1,"controller":"https://controller.invalid","id":"fixture","token":"private-fixture-token-never-print","enrolled":true,"budget":{"cpu":2,"memory_gib":4,"disk_gib":20},"binding":{"host_id":"host","binding_digest":"b".repeat(64),"machine_id_hash":"c".repeat(64),"project":"enginebox","pool":"enginebox"}})).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_enginebox-node"))
        .args(["run-host", "--config"])
        .arg(&path)
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("Invalid managed node credential"));
    assert!(!text.contains("private-fixture-token-never-print"));
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod linux {
    use super::*;
    use axum::{extract::Request, middleware::Next, response::IntoResponse};
    use enginebox_controller::{Controller, contributors::Contributors, router};
    use enginebox_node_protocol::{Budget, machine_id_hash};
    use serde_json::Value;
    use std::{
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };
    use tokio::io::AsyncWriteExt;
    const OWNER: &str = "fixture-owner-token-for-managed-host-controller";
    fn host_value() -> Value {
        json!({"id":"fixture-host","label":"Fixture","target":"unused-fixture.invalid","project":"managed-fixture-no-project","pool":"managed-fixture-no-pool","managed_registration":true})
    }
    async fn post(
        client: &reqwest::Client,
        origin: &str,
        route: &str,
        token: &str,
        body: Value,
    ) -> reqwest::Response {
        client
            .post(format!("{origin}{route}"))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .unwrap()
    }
    async fn launch_register(origin: &str, path: &Path, code: &str) -> std::process::Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_enginebox-node"))
            .args(["register-host", "--controller", origin, "--config"])
            .arg(path)
            .arg("--test-loopback-http")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        input
            .write_all(format!("{code}\n").as_bytes())
            .await
            .unwrap();
        drop(input);
        child.wait_with_output().await.unwrap()
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn real_node_rejects_other_machine_retries_transport_and_observes_revocation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let state = root.join("state");
        let config = root.join("fleet.toml");
        std::fs::write(&config,"schema_version=1\n[[hosts]]\nid='fixture-host'\nlabel='Fixture'\ntarget='unused-fixture.invalid'\nproject='managed-fixture-no-project'\npool='managed-fixture-no-pool'\nmanaged_registration=true\n").unwrap();
        let registry = Contributors::open(&state).unwrap();
        let host = serde_json::from_value(host_value()).unwrap();
        let wrong = registry
            .issue_host(
                &host,
                "0".repeat(64),
                Budget {
                    cpu: 2,
                    memory_gib: 4,
                    disk_gib: 20,
                },
            )
            .unwrap();
        let controller = Controller::new(&config, &state, OWNER.into()).unwrap();
        controller.activate().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let attempts = Arc::new(AtomicUsize::new(0));
        let observed = attempts.clone();
        let app = router(controller.clone()).layer(axum::middleware::from_fn(
            move |request: Request, next: Next| {
                let attempts = attempts.clone();
                async move {
                    if request.uri().path() == "/v1/node/heartbeat"
                        && attempts.fetch_add(1, Ordering::SeqCst) == 0
                    {
                        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                    }
                    next.run(request).await
                }
            },
        ));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let rejected_path = root.join("rejected/host.json");
        let rejected =
            launch_register(&origin, &rejected_path, wrong["code"].as_str().unwrap()).await;
        assert!(!rejected.status.success());
        assert!(!rejected_path.exists());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("another machine"));
        // Stop the disposable owner before editing the fixture's registrations.
        assert_eq!(controller.drain()["drained"], true);
        let registry = Contributors::open(&state).unwrap();
        registry.revoke(wrong["id"].as_str().unwrap()).unwrap();
        let local = machine_id_hash(&std::fs::read_to_string("/etc/machine-id").unwrap()).unwrap();
        let issued = registry
            .issue_host(
                &host,
                local,
                Budget {
                    cpu: 2,
                    memory_gib: 4,
                    disk_gib: 20,
                },
            )
            .unwrap();
        let id = issued["id"].as_str().unwrap();
        let code = issued["code"].as_str().unwrap();
        let token = format!("{id}.{}", "a".repeat(64));
        controller.activate().unwrap();
        let client = reqwest::Client::new();
        let info: Value = post(
            &client,
            &origin,
            "/v1/node/enroll",
            code,
            json!({"kind":"incus-info"}),
        )
        .await
        .json()
        .await
        .unwrap();
        let encoded = serde_json::to_vec(&token).unwrap();
        use sha2::{Digest, Sha256};
        let secret = format!("{:x}", Sha256::digest(encoded));
        let enrolled=post(&client,&origin,"/v1/node/enroll",code,json!({"kind":"incus","secret_hash":secret,"machine_id_hash":info["binding"]["machine_id_hash"]})).await;
        assert!(enrolled.status().is_success());
        let path = root.join("registered/host.json");
        enginebox_install::release::private_dir(path.parent().unwrap()).unwrap();
        enginebox_install::release::atomic_json(&path,&json!({"schema":1,"controller":origin,"id":id,"token":token,"enrolled":true,"budget":info["budget"],"binding":info["binding"]})).unwrap();
        let mut pending: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        pending["enrolled"] = json!(false);
        enginebox_install::release::atomic_json(&path, &pending).unwrap();
        // Recover a lost enrollment acknowledgment using the saved credential,
        // even when the one-time code presented again is no longer valid.
        let expired_code = format!("{id}.{}", "0".repeat(64));
        let recovery = launch_register(&origin, &path, &expired_code).await;
        assert!(
            recovery.status.success(),
            "{}",
            String::from_utf8_lossy(&recovery.stderr)
        );
        let recovered: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(recovered["enrolled"], true);
        assert_eq!(recovered["token"], token);
        let before = std::fs::read(&path).unwrap();
        // Idempotent installation uses the saved identity without probing or consuming again.
        assert!(launch_register(&origin, &path, code).await.status.success());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let mut child = Command::new(env!("CARGO_BIN_EXE_enginebox-node"))
            .args(["run-host", "--config"])
            .arg(&path)
            .arg("--test-loopback-http")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let status_path = path.parent().unwrap().join("status.json");
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            let status = std::fs::read(&status_path)
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
            if status.as_ref().is_some_and(|s| s["online"] == true) {
                let status = status.unwrap();
                assert_eq!(status["id"], id);
                assert_eq!(status["ready"], false);
                assert!(status["last_seen"].as_u64().is_some());
                break;
            }
            assert!(
                child.try_wait().unwrap().is_none(),
                "Managed node exited unexpectedly"
            );
            assert!(Instant::now() < deadline, "Managed node did not reconnect");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(observed.load(Ordering::SeqCst) >= 2);
        let denied = client
            .get(format!("{origin}/v1/fleet"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), reqwest::StatusCode::UNAUTHORIZED);
        assert!(
            post(
                &client,
                &origin,
                &format!("/v1/contributors/{id}/revoke"),
                OWNER,
                json!({})
            )
            .await
            .status()
            .is_success()
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let status: Value =
                serde_json::from_slice(&std::fs::read(&status_path).unwrap()).unwrap();
            if status["online"] == false {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Revoked node still reports online"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(std::fs::read(&path).unwrap(), before);
        child.kill().await.unwrap();
        let _ = child.wait().await;
        server.abort();
    }
}
