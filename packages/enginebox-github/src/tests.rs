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
    let root = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    state::write_json(&root.path().join("github.json"), &config()).unwrap();
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
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                }
            }
            let mut input = vec![0; length];
            reader.read_exact(&mut input).unwrap();
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
    data.accounts.insert("repos".into(), value);
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
    let (_root, mut g) = service();
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
    g.config.as_mut().unwrap().repos.private_key = String::from_utf8(output.stdout).unwrap();
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
fn signed(g: &GitHub, payload: &Value) -> Value {
    let bytes = serde_json::to_vec(payload).unwrap();
    let key = ring::hmac::Key::new(
        ring::hmac::HMAC_SHA256,
        g.app("runners").unwrap().webhook_secret.as_bytes(),
    );
    let sig = ring::hmac::sign(&key, &bytes);
    let value = format!(
        "sha256={}",
        sig.as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    g.webhook("runners", "workflow_job", &value, &bytes)
        .unwrap()
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
