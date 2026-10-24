use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use enginebox_controller::{Controller, router};
use serde_json::{Value, json};
use tower::ServiceExt;
async fn call(app: &axum::Router, token: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let r = app
        .clone()
        .oneshot(
            Request::post(path)
                .header("Authorization", format!("Bearer {token}"))
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = r.status();
    (
        status,
        serde_json::from_slice(&to_bytes(r.into_body(), 500000).await.unwrap()).unwrap(),
    )
}
#[tokio::test]
async fn scoped_node_http_credentials_and_drain_admission() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = tmp.path().join("fleet.toml");
    std::fs::write(
        &fleet,
        "schema_version=1\n[[hosts]]\nid='metal'\ntarget='unused-fixture'\n",
    )
    .unwrap();
    let owner = "x".repeat(32);
    let c = Controller::new(&fleet, &tmp.path().join("state"), owner.clone()).unwrap();
    c.activate().unwrap();
    let app = router(c.clone());
    let (status,invite)=call(&app,&owner,"/v1/contributors/enrollments",json!({"label":"Windows PC","budget":{"cpu":2,"memory_gib":4,"disk_gib":20},"workloads":{"ci":true,"agents":true}})).await;
    assert_eq!(status, StatusCode::OK);
    let token = format!("{}.{}", invite["id"].as_str().unwrap(), "a".repeat(64));
    assert_eq!(
        call(&app, &token, "/v1/node/hello", json!({})).await.0,
        StatusCode::UNAUTHORIZED
    );
    let enrollment = json!({"secret_hash":enginebox_core::state::digest(&json!(token)),"budget":{"cpu":2,"memory_gib":4,"disk_gib":20},"os":"windows","architecture":"x86_64"});
    assert_eq!(
        call(
            &app,
            invite["code"].as_str().unwrap(),
            "/v1/node/enroll",
            enrollment
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, &token, "/v1/contributors/enrollments", json!({}))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&app, &owner, "/v1/node/hello", json!({})).await.0,
        StatusCode::UNAUTHORIZED
    );
    let session = uuid::Uuid::new_v4().to_string();
    let (status, hello) = call(&app, &token, "/v1/node/hello", json!({"session":session})).await;
    assert_eq!(status, StatusCode::OK);
    let body = json!({"session":session,"epoch":hello["epoch"],"tools":{"worker_running":false}});
    assert_eq!(
        call(&app, &token, "/v1/node/heartbeat", body.clone())
            .await
            .0,
        StatusCode::OK
    );
    // Draining admits completion traffic only, so the existing owner can finish.
    c.drain();
    assert_eq!(
        call(&app, &token, "/v1/node/poll", body.clone()).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let status = call(&app, &token, "/v1/node/heartbeat", body.clone())
        .await
        .0;
    assert!(status == StatusCode::OK || status == StatusCode::SERVICE_UNAVAILABLE); // Already drained has no work to finish.
    c.activate().unwrap();
    assert_eq!(
        call(
            &app,
            &owner,
            &format!("/v1/contributors/{}/revoke", invite["id"].as_str().unwrap()),
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, &token, "/v1/node/heartbeat", body).await.0,
        StatusCode::UNAUTHORIZED
    );
}
