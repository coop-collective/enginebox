use super::*;
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
};

fn config() -> Value {
    let app = json!({"client_id":"fixture-repos","client_secret":"fixture-secret","slug":"fixture-repos","private_key":"fixture-not-a-key","webhook_secret":"fixture-webhook-secret-32-characters"});
    let mut runners = app.clone();
    runners["client_id"] = json!("fixture-runners");
    runners["slug"] = json!("fixture-runners");
    json!({"schema":1,"callback_url":"https://example.invalid/api/github/callback","repos":app,"runners":runners,"pools":[{"id":"linux","installation_id":2,"organization":"fixture","runner_group_id":1,"labels":["self-hosted","Linux","X64","enginebox-linux"],"max_runners":1,"timeout_seconds":300,"worker":[std::env::current_exe().unwrap()]}]})
}
pub(crate) fn service() -> (tempfile::TempDir, GitHub) {
    service_config(config())
}
pub(crate) fn shared_service() -> (tempfile::TempDir, GitHub) {
    let mut value = config();
    let app = value.as_object_mut().unwrap().remove("runners").unwrap();
    value.as_object_mut().unwrap().remove("repos");
    value["app"] = app;
    service_config(value)
}
fn service_config(value: Value) -> (tempfile::TempDir, GitHub) {
    let root = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    state::write_json(&root.path().join("github.json"), &value).unwrap();
    let service = GitHub::open(root.path()).unwrap();
    (root, service)
}
pub(crate) fn api_fixture(
    responses: Vec<(&'static str, Value)>,
) -> (api::Api, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let thread = std::thread::spawn(move || {
        let mut runner_name = Value::Null;
        for (expected, mut response) in responses {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            assert!(first.contains(expected), "Unexpected fixture request path");
            let mut length = 0;
            let mut authorization = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                }
                if let Some(v) = line.strip_prefix("authorization: ") {
                    authorization = v.trim().to_owned();
                }
            }
            let mut input = vec![0; length];
            reader.read_exact(&mut input).unwrap();
            if let Some(expected) = response.as_object_mut().unwrap().remove("__request") {
                if let Some(token) = expected["authorization"].as_str() {
                    assert_eq!(authorization, token);
                }
                if let Some(fields) = expected["body"].as_object() {
                    let body: Value = serde_json::from_slice(&input).unwrap();
                    for (key, value) in fields {
                        assert_eq!(&body[key], value, "Unexpected fixture request field {key}");
                    }
                }
            }
            if expected == "/oauth" {
                let value: Value = serde_json::from_slice(&input).unwrap();
                assert!(
                    value["code_verifier"]
                        .as_str()
                        .is_some_and(|s| s.len() == 43)
                        || value["grant_type"] == "refresh_token"
                );
            }
            if expected == "/app/installations/4/access_tokens" {
                let value: Value = serde_json::from_slice(&input).unwrap();
                assert_eq!(value["repository_ids"], json!([101]));
                assert_eq!(value["permissions"], json!({"contents":"read"}));
            }
            if expected.contains("generate-jitconfig") {
                let value: Value = serde_json::from_slice(&input).unwrap();
                runner_name = value["name"].clone();
                assert_eq!(value["runner_group_id"], 1);
                assert_eq!(value["work_folder"], response["expected_work_folder"]);
            }
            if response["token"] == "fixture-verification-token" {
                let value: Value = serde_json::from_slice(&input).unwrap();
                assert_eq!(value["repository_ids"], json!([101]));
                assert_eq!(value["permissions"], json!({"actions":"read"}));
            }
            if matches!(
                response["token"].as_str(),
                Some("fixture-installation-token" | "fixture-cleanup-token")
            ) {
                let value: Value = serde_json::from_slice(&input).unwrap();
                assert_eq!(
                    value["permissions"],
                    json!({"organization_self_hosted_runners":"write","actions":"read"})
                );
            }
            if response["name"] == "fixture-created-runner" {
                response["name"] = runner_name.clone();
            }
            let bytes = response.to_string();
            write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",bytes.len(),bytes).unwrap();
        }
    });
    (api::Api::fixture(format!("http://{address}")), thread)
}
fn token() -> Value {
    json!({"access_token":"fixture-user-access","refresh_token":"fixture-user-refresh","expires_in":28800})
}
pub(crate) fn account(g: &GitHub) {
    let mut data = g.data.lock().unwrap();
    let mut value = normalize_token(token()).unwrap();
    value["id"] = json!(1);
    value["login"] = json!("fixture");
    data.accounts
        .insert(g.account_kind("repos").unwrap().into(), value);
    g.save(&data).unwrap();
}
#[test]
fn oauth_uses_pkce_consumes_state_and_never_exposes_credentials() {
    let (root, mut g) = service();
    let (api, server) = api_fixture(vec![
        ("/oauth", token()),
        ("/user", json!({"id":1,"login":"fixture"})),
    ]);
    g.api = api;
    let flow = g.connect("repos").unwrap();
    let url = url::Url::parse(flow["url"].as_str().unwrap()).unwrap();
    assert_eq!(
        url.query_pairs()
            .find(|(k, _)| k == "code_challenge_method")
            .unwrap()
            .1,
        "S256"
    );
    assert!(g.callback("wrong-state", "code").is_err());
    g.callback(text(&flow, "state").unwrap(), "code").unwrap();
    assert!(g.callback(text(&flow, "state").unwrap(), "code").is_err());
    server.join().unwrap();
    let status = g.status().to_string();
    assert!(!status.contains("fixture-user-access") && !status.contains("fixture-user-refresh"));
    let reopened = GitHub::open(root.path()).unwrap();
    assert_eq!(reopened.status()["accounts"][0]["login"], "fixture");
}
#[test]
fn pagination_refresh_and_repository_selection_are_authoritative() {
    let (_root, mut g) = service();
    account(&g);
    g.data.lock().unwrap().accounts.get_mut("repos").unwrap()["expires_at"] = json!(0);
    let repo = json!({"id":101,"full_name":"fixture/private","private":true,"permissions":{"push":true},"default_branch":"main"});
    let (api, server) = api_fixture(vec![
        ("/oauth", token()),
        (
            "page=1",
            json!({"repositories":(1..=100).map(|id|json!({"id":id})).collect::<Vec<_>>()}),
        ),
        ("page=2", json!({"repositories":[repo]})),
    ]);
    g.api = api;
    g.select(4, 101, "main").unwrap();
    server.join().unwrap();
    assert_eq!(g.status()["repositories"][0]["private"], true);
    let (api, server) = api_fixture(vec![("page=1", json!({"repositories":[]}))]);
    g.api = api;
    assert!(
        g.transfer_credential("fixture/private", Some("main"), false)
            .is_err()
    );
    server.join().unwrap();
    assert!(!g.root.join("checkouts/101").exists());
}
#[cfg(unix)] // Generates an ephemeral RSA fixture with the Unix OpenSSL tool.
#[test]
fn transfer_credentials_are_scoped_to_the_selected_repository() {
    for (_root, mut g) in [service(), shared_service()] {
        account(&g);
        g.data.lock().unwrap().repositories.insert(
            101,
            Repository {
                id: 101,
                installation_id: 4,
                full_name: "fixture/private".into(),
                private: true,
                branch: "main".into(),
            },
        );
        assert!(
            g.transfer_credential("fixture/private", Some("other"), false)
                .is_err()
        );
        assert!(
            g.transfer_credential("fixture/other", Some("main"), false)
                .is_err()
        );
        let output = std::process::Command::new("openssl")
            .args(["genrsa", "2048"])
            .stderr(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success());
        let config = g.config.as_mut().unwrap();
        config
            .app
            .as_mut()
            .or(config.repos.as_mut())
            .unwrap()
            .private_key = String::from_utf8(output.stdout).unwrap();
        let (api, server) = api_fixture(vec![
            (
                "page=1",
                json!({"repositories":[{"id":101,"full_name":"fixture/private","permissions":{"push":true}}]}),
            ),
            (
                "/app/installations/4/access_tokens",
                json!({"token":"ghs_APP.jwt-fixture"}),
            ),
        ]);
        g.api = api;
        let credential = g
            .transfer_credential("fixture/private", Some("main"), false)
            .unwrap();
        assert_eq!(credential["token"], "ghs_APP.jwt-fixture");
        server.join().unwrap();
        assert!(!g.status().to_string().contains("ghs_APP"));
        assert!(
            !std::fs::read_to_string(g.root.join("state.json"))
                .unwrap()
                .contains("ghs_APP")
        );
    }
}

fn signed(g: &GitHub, payload: &Value) -> Value {
    signed_at(g, "runners", payload)
}
fn signed_at(g: &GitHub, kind: &str, payload: &Value) -> Value {
    let bytes = serde_json::to_vec(payload).unwrap();
    let key = ring::hmac::Key::new(
        ring::hmac::HMAC_SHA256,
        g.app(kind).unwrap().webhook_secret.as_bytes(),
    );
    let sig = ring::hmac::sign(&key, &bytes);
    let value = format!(
        "sha256={}",
        sig.as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    g.webhook(kind, "workflow_job", &value, &bytes).unwrap()
}
#[test]
fn webhook_duplicates_out_of_order_and_restart_do_not_replay_jobs() {
    let (root, g) = service();
    account(&g);
    g.data.lock().unwrap().repositories.insert(
        5,
        Repository {
            id: 5,
            installation_id: 4,
            full_name: "fixture/private".into(),
            private: true,
            branch: "main".into(),
        },
    );
    let mut event = json!({"action":"queued","installation":{"id":2},"repository":{"id":5,"full_name":"fixture/private","owner":{"login":"fixture"}},"workflow_job":{"id":99,"run_id":44,"labels":["self-hosted","enginebox-linux"]}});
    signed(&g, &event);
    signed(&g, &event);
    assert_eq!(g.status()["jobs"].as_array().unwrap().len(), 1);
    event["action"] = json!("completed");
    signed(&g, &event);
    event["action"] = json!("queued");
    signed(&g, &event);
    assert_eq!(g.status()["jobs"][0]["status"], "cancelled");
    event["action"] = json!("in_progress");
    signed(&g, &event);
    assert_eq!(g.status()["jobs"][0]["github_status"], "completed");
    event["action"] = json!("queued");
    event["workflow_job"]["id"] = json!(100);
    event["workflow_job"]["labels"] = json!(["self-hosted"]);
    assert_eq!(signed(&g, &event)["ignored"], true);
    event["workflow_job"]["labels"] = json!(["enginebox-linux"]);
    signed(&g, &event);
    {
        let mut data = g.data.lock().unwrap();
        data.jobs.get_mut("2-100").unwrap()["status"] = json!("running");
        g.save(&data).unwrap();
    }
    let reopened = GitHub::open(root.path()).unwrap();
    assert_eq!(
        reopened.data.lock().unwrap().jobs["2-100"]["status"],
        "uncertain"
    );
    reopened.set_draining(true).unwrap();
    reopened.tick().unwrap();
    assert_eq!(reopened.pending(), 0);
    assert!(reopened.disconnect("repos").is_err());
}
#[test]
fn disabled_module_does_not_create_state_or_touch_credentials() {
    let root = tempfile::tempdir().unwrap();
    let g = GitHub::open(root.path()).unwrap();
    assert_eq!(g.status()["configured"], false);
    assert!(!root.path().join("github").exists());
    assert!(g.connect("repos").is_err());
}

#[cfg(unix)]
#[test]
fn app_jwt_has_short_lifetime_and_a_verifiable_rsa_signature() {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use std::process::{Command, Stdio};
    let temp = tempfile::tempdir().unwrap();
    let generated = Command::new("openssl")
        .args(["genrsa", "2048"])
        .stderr(Stdio::null())
        .output()
        .unwrap();
    assert!(generated.status.success());
    let key_path = temp.path().join("fixture.pem");
    std::fs::write(&key_path, &generated.stdout).unwrap();
    let public = Command::new("openssl")
        .args(["rsa", "-RSAPublicKey_out", "-outform", "DER", "-in"])
        .arg(&key_path)
        .stderr(Stdio::null())
        .output()
        .unwrap();
    assert!(public.status.success());
    let mut app: App = serde_json::from_value(config()["runners"].clone()).unwrap();
    app.private_key = String::from_utf8(generated.stdout).unwrap();
    let jwt = api::jwt(&app).unwrap();
    let parts = jwt.split('.').collect::<Vec<_>>();
    assert_eq!(parts.len(), 3);
    let payload: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
    assert_eq!(payload["iss"], app.client_id);
    assert!(payload["exp"].as_u64().unwrap() <= api::now() + 600);
    ring::signature::UnparsedPublicKey::new(
        &ring::signature::RSA_PKCS1_2048_8192_SHA256,
        public.stdout,
    )
    .verify(
        format!("{}.{}", parts[0], parts[1]).as_bytes(),
        &URL_SAFE_NO_PAD.decode(parts[2]).unwrap(),
    )
    .unwrap();
}

#[test]
fn shared_app_uses_one_oauth_session_and_serializes_refresh_for_both_capabilities() {
    let (root, mut g) = shared_service();
    let flow = g.connect("runners").unwrap();
    let (api, server) = api_fixture(vec![
        ("/oauth", token()),
        ("/user", json!({"id":1,"login":"fixture"})),
    ]);
    g.api = api;
    assert_eq!(
        g.callback(text(&flow, "state").unwrap(), "code").unwrap()["kind"],
        "github"
    );
    server.join().unwrap();
    assert_eq!(g.status()["shared_app"], true);
    assert_eq!(g.status()["accounts"].as_array().unwrap().len(), 1);
    g.data.lock().unwrap().accounts.get_mut("github").unwrap()["expires_at"] = json!(0);
    let mut refreshed = token();
    refreshed["access_token"] = json!("fixture-rotated-access");
    refreshed["refresh_token"] = json!("fixture-rotated-refresh");
    refreshed["__request"] = json!({"body":{"client_id":"fixture-runners","grant_type":"refresh_token","refresh_token":"fixture-user-refresh"}});
    let (api, server) = api_fixture(vec![
        ("/oauth", refreshed),
        (
            "/user/installations?",
            json!({"installations":[],"__request":{"authorization":"Bearer fixture-rotated-access"}}),
        ),
        (
            "/user/installations/2/repositories?",
            json!({"repositories":[],"__request":{"authorization":"Bearer fixture-rotated-access"}}),
        ),
    ]);
    g.api = api;
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let callers = ["repos", "runners"].map(|kind| {
        let g = g.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            g.user_token(kind, &mut g.data.lock().unwrap()).unwrap()
        })
    });
    for caller in callers {
        assert_eq!(caller.join().unwrap(), "fixture-rotated-access");
    }
    g.list_installations("runners").unwrap();
    g.repositories(2).unwrap();
    server.join().unwrap();
    let reopened = GitHub::open(root.path()).unwrap();
    let mut data = reopened.data.lock().unwrap();
    assert_eq!(data.accounts.len(), 1);
    assert_eq!(
        data.accounts["github"]["refresh_token"],
        "fixture-rotated-refresh"
    );
    // Both callers, including simultaneous runner/sync work, use this same locked record.
    assert_eq!(
        reopened.user_token("repos", &mut data).unwrap(),
        "fixture-rotated-access"
    );
    assert_eq!(
        reopened.user_token("runners", &mut data).unwrap(),
        "fixture-rotated-access"
    );
    drop(data);
    assert!(!reopened.status().to_string().contains("fixture-rotated"));
}

#[test]
fn shared_disconnect_invalidates_both_flows_and_repo_selection_only_after_jobs_resolve() {
    for kind in ["repos", "runners"] {
        let (_root, g) = shared_service();
        account(&g);
        g.connect("repos").unwrap();
        g.connect("runners").unwrap();
        {
            let mut data = g.data.lock().unwrap();
            data.repositories.insert(
                101,
                Repository {
                    id: 101,
                    installation_id: 2,
                    full_name: "fixture/private".into(),
                    private: true,
                    branch: "main".into(),
                },
            );
            data.jobs
                .insert("2-99".into(), json!({"status":"uncertain"}));
        }
        assert!(g.disconnect(kind).is_err());
        assert_eq!(g.status()["accounts"].as_array().unwrap().len(), 1);
        g.data.lock().unwrap().jobs.clear();
        g.disconnect(kind).unwrap();
        let mut data = g.data.lock().unwrap();
        assert!(data.flows.is_empty() && data.accounts.is_empty() && data.repositories.is_empty());
        assert!(g.user_token("repos", &mut data).is_err());
        assert!(g.user_token("runners", &mut data).is_err());
    }
}

#[test]
fn shared_reconnect_cannot_switch_principals_or_reuse_a_flow_for_another_app() {
    let (_root, mut g) = shared_service();
    account(&g);
    let flow = g.connect("repos").unwrap();
    let (api, server) = api_fixture(vec![
        ("/oauth", token()),
        ("/user", json!({"id":2,"login":"other"})),
    ]);
    g.api = api;
    assert!(
        g.callback(text(&flow, "state").unwrap(), "code")
            .unwrap_err()
            .to_string()
            .contains("Disconnect")
    );
    server.join().unwrap();
    assert_eq!(g.status()["accounts"][0]["login"], "fixture");
    let flow = g.connect("runners").unwrap();
    g.config.as_mut().unwrap().app.as_mut().unwrap().client_id = "changed-app".into();
    assert!(
        g.callback(text(&flow, "state").unwrap(), "code")
            .unwrap_err()
            .to_string()
            .contains("App changed")
    );
    assert!(g.callback(text(&flow, "state").unwrap(), "code").is_err());
}

#[test]
fn configuration_keeps_legacy_modes_and_rejects_ambiguous_credentials() {
    let (root, g) = service();
    assert_eq!(g.status()["shared_app"], false);
    assert_eq!(g.status()["runner_app_configured"], true);
    let mut value = config();
    value["runners"] = Value::Null;
    value["pools"] = json!([]);
    state::write_json(&root.path().join("github.json"), &value).unwrap();
    let repos_only = GitHub::open(root.path()).unwrap();
    assert_eq!(repos_only.status()["runner_app_configured"], false);
    assert!(repos_only.connect("runners").is_err());
    assert!(repos_only.connect("repos").is_ok());
    for bad in [
        json!({"schema":1,"callback_url":"https://example.invalid/callback"}),
        {
            let mut value = config();
            value["app"] = value["repos"].clone();
            value
        },
        {
            let mut value = config();
            value["runners"] = value["repos"].clone();
            value
        },
    ] {
        state::write_json(&root.path().join("github.json"), &bad).unwrap();
        assert!(GitHub::open(root.path()).is_err());
    }
    // Switching modes must not silently reuse an unrelated App's token or selections.
    let mut value = config();
    value["app"] = value["runners"].clone();
    value.as_object_mut().unwrap().remove("repos");
    value.as_object_mut().unwrap().remove("runners");
    account(&g);
    state::write_json(&root.path().join("github.json"), &value).unwrap();
    assert!(
        GitHub::open(root.path())
            .err()
            .unwrap()
            .to_string()
            .contains("Disconnect")
    );
}

#[test]
fn shared_app_accepts_existing_webhook_urls_and_deduplicates_across_them() {
    let (_root, g) = shared_service();
    g.data.lock().unwrap().repositories.insert(
        101,
        Repository {
            id: 101,
            installation_id: 2,
            full_name: "fixture/private".into(),
            private: true,
            branch: "main".into(),
        },
    );
    let mut payload = json!({"action":"queued","installation":{"id":2},"repository":{"id":101,"full_name":"fixture/private","owner":{"login":"fixture"}},"workflow_job":{"id":99,"run_id":44,"labels":["enginebox-linux"]}});
    for kind in ["repos", "runners"] {
        assert!(
            g.webhook(
                kind,
                "workflow_job",
                "sha256=invalid",
                &serde_json::to_vec(&payload).unwrap()
            )
            .is_err()
        );
        assert_eq!(signed_at(&g, kind, &payload)["accepted"], true);
    }
    assert_eq!(g.status()["jobs"].as_array().unwrap().len(), 1);
    payload["action"] = json!("completed");
    signed_at(&g, "repos", &payload);
    payload["action"] = json!("queued");
    signed_at(&g, "runners", &payload);
    assert_eq!(g.status()["jobs"][0]["status"], "cancelled");
    payload["installation"]["id"] = json!(3);
    payload["workflow_job"]["id"] = json!(100);
    assert_eq!(signed_at(&g, "runners", &payload)["ignored"], true);
    // A separate repository App must not acquire runner authority via its webhook.
    let (_root, legacy) = service();
    assert_eq!(signed_at(&legacy, "repos", &payload)["ignored"], true);
}
