use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use enginebox_controller::contributors::Contributors;
use enginebox_install::contributor::{self, Config};
use enginebox_node_protocol::{Budget, Enrollment, Invitation, Workloads};
use serde_json::{Value, json};

// Run the real registration client against controller enrollment storage. The
// fixture models WSL's OS/architecture even when this test runs on a Mac.
async fn enroll(
    State(service): State<Contributors>,
    headers: HeaderMap,
    Json(mut e): Json<Enrollment>,
) -> (StatusCode, Json<Value>) {
    e.os = "linux".into();
    e.architecture = "x86_64".into();
    let code = headers["Authorization"]
        .to_str()
        .unwrap()
        .strip_prefix("Bearer ")
        .unwrap();
    match service.enroll(code, e) {
        Ok(v) => (StatusCode::OK, Json(v)),
        Err(_) => (
            StatusCode::CONFLICT,
            Json(json!({"error":"Registration rejected"})),
        ),
    }
}

#[tokio::test]
async fn renewed_invitation_recovers_real_pending_file_without_replacing_key_or_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let service = Contributors::open(&root.join("controller")).unwrap();
    let budget = Budget {
        cpu: 2,
        memory_gib: 4,
        disk_gib: 20,
    };
    let invitation = Invitation {
        label: "Pending fixture".into(),
        budget: budget.clone(),
        workloads: Workloads {
            ci: true,
            agents: true,
            engine: false,
        },
    };
    let issued = service.issue(invitation.clone()).unwrap();
    let id = issued["id"].as_str().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/node/enroll", post(enroll))
        .with_state(service.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let path = root.join("private/worker.json");
    let bad_code = format!("{id}.{}", "0".repeat(64));
    let error = contributor::register(&path, &origin, &bad_code, budget.clone(), true)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("HTTP 409"), "{error:#}");
    let before = std::fs::read(&path).unwrap();
    let pending: Config = serde_json::from_slice(&before).unwrap();
    assert!(!pending.enrolled);
    let unrelated = service.issue(invitation).unwrap();
    assert!(
        contributor::register(
            &path,
            &origin,
            unrelated["code"].as_str().unwrap(),
            budget.clone(),
            true
        )
        .await
        .is_err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let renewed = service.renew_enrollment(id).unwrap();
    let result = contributor::register(
        &path,
        &origin,
        renewed["code"].as_str().unwrap(),
        budget.clone(),
        true,
    )
    .await
    .unwrap();
    let recovered: Config = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(result["id"], id);
    assert!(recovered.enrolled);
    assert_eq!(recovered.id, pending.id);
    assert_eq!(recovered.token, pending.token);
    assert_eq!(recovered.controller, pending.controller);
    assert_eq!(recovered.budget, pending.budget);
    assert!(recovered.workloads.ci && recovered.workloads.agents);
    assert!(service.authenticated(&pending.token));
    assert!(service.renew_enrollment(id).is_err());
    server.abort();
}
