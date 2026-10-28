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
async fn catalog(app: &axum::Router, token: &str) -> Value {
    let response = app
        .clone()
        .oneshot(
            Request::get("/v1/contributors")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(&to_bytes(response.into_body(), 500000).await.unwrap()).unwrap()
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
    assert_eq!(catalog(&app, &owner).await["compute_nodes"], json!([]));
    let token = format!("{}.{}", invite["id"].as_str().unwrap(), "a".repeat(64));
    let renewal_path = format!(
        "/v1/contributors/{}/renew-enrollment",
        invite["id"].as_str().unwrap()
    );
    assert_eq!(
        call(
            &app,
            invite["code"].as_str().unwrap(),
            &renewal_path,
            json!({})
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&app, &token, &renewal_path, json!({})).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&app, &owner, &renewal_path, json!({"budget":{}}))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let (status, renewed) = call(&app, &owner, &renewal_path, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(renewed["id"], invite["id"]);
    assert_ne!(renewed["code"], invite["code"]);
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
            enrollment.clone()
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        call(
            &app,
            renewed["code"].as_str().unwrap(),
            "/v1/node/enroll",
            enrollment
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, &owner, &renewal_path, json!({})).await.0,
        StatusCode::BAD_REQUEST
    );
    let registered = catalog(&app, &owner).await;
    let machine_id = format!("worker:{}", invite["id"].as_str().unwrap());
    assert_eq!(registered["compute_nodes"].as_array().unwrap().len(), 1);
    assert_eq!(registered["compute_nodes"][0]["id"], machine_id);
    assert_eq!(registered["compute_nodes"][0]["registered"], true);
    assert_eq!(registered["compute_nodes"][0]["online"], false);
    assert_eq!(
        registered["compute_nodes"][0]["resources"]["budget"]["memory_bytes"],
        4 * enginebox_core::GIB
    );
    assert!(!registered.to_string().contains(&token));
    assert!(
        !registered
            .to_string()
            .contains(renewed["code"].as_str().unwrap())
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
    let body = json!({"session":session,"epoch":hello["epoch"],"tools":{"worker_running":false,"backend":"wsl2-layerfs","workspace":true,"templates":["native-dev"]}});
    assert_eq!(
        call(&app, &token, "/v1/node/heartbeat", body.clone())
            .await
            .0,
        StatusCode::OK
    );
    let connected = catalog(&app, &owner).await;
    assert_eq!(connected["compute_nodes"][0]["id"], machine_id);
    assert_eq!(connected["compute_nodes"][0]["runtime"]["state"], "ready");
    assert_eq!(connected["compute_nodes"][0]["online"], true);
    assert_eq!(
        connected["compute_nodes"][0]["resources"]["available"],
        Value::Null
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
    // Reloading the durable registration preserves the canonical identity even
    // while liveness waits for the next authenticated heartbeat.
    assert_eq!(
        catalog(&app, &owner).await["compute_nodes"][0]["id"],
        machine_id
    );
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
    assert_eq!(catalog(&app, &owner).await["compute_nodes"], json!([]));
    assert_eq!(
        call(&app, &token, "/v1/node/heartbeat", body).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn wsl_enrollment_uses_local_resources_and_requires_template_readiness() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = tmp.path().join("fleet.toml");
    std::fs::write(
        &fleet,
        "schema_version=1\n[[hosts]]\nid='metal'\ntarget='unused-fixture'\n",
    )
    .unwrap();
    let owner = "w".repeat(32);
    let controller = Controller::new(&fleet, &tmp.path().join("state"), owner.clone()).unwrap();
    controller.activate().unwrap();
    let app = router(controller);
    // An invitation authorizes registration; resources are selected on the PC.
    let (status, invitation) = call(
        &app,
        &owner,
        "/v1/contributors/enrollments",
        json!({"label":"WSL machine","workloads":{"ci":true,"agents":true}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let token = format!("{}.{}", invitation["id"].as_str().unwrap(), "c".repeat(64));
    let budget = json!({"cpu":3,"memory_gib":6,"disk_gib":30});
    let enrollment = json!({"secret_hash":enginebox_core::state::digest(&json!(token)),"budget":budget,"os":"linux","architecture":"x86_64"});
    let (status, result) = call(
        &app,
        invitation["code"].as_str().unwrap(),
        "/v1/node/enroll",
        enrollment,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["budget"], budget);
    let session = uuid::Uuid::new_v4().to_string();
    let (_, hello) = call(&app, &token, "/v1/node/hello", json!({"session":session})).await;
    let mut heartbeat = json!({"session":session,"epoch":hello["epoch"],"tools":{"workspace":false,"worker_running":false}});
    assert_eq!(
        call(&app, &token, "/v1/node/heartbeat", heartbeat.clone())
            .await
            .0,
        StatusCode::OK
    );
    let path = format!(
        "/v1/contributors/{}/runs",
        invitation["id"].as_str().unwrap()
    );
    let job = json!({"request_id":"workspace-proof","kind":"workspace","request":{"operation":"fork","workspace":"proof","project":"welcome"}});
    assert!(!call(&app, &owner, &path, job.clone()).await.0.is_success());
    heartbeat["tools"]["workspace"] = json!(true);
    assert_eq!(
        call(&app, &token, "/v1/node/heartbeat", heartbeat.clone())
            .await
            .0,
        StatusCode::OK
    );
    let mut invalid = job.clone();
    invalid["request"]["workspace"] = json!("../../private");
    assert!(!call(&app, &owner, &path, invalid).await.0.is_success());
    let (status, submitted) = call(&app, &owner, &path, job).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let mut assigned = None;
    for _ in 0..100 {
        let (_, poll) = call(&app, &token, "/v1/node/poll", heartbeat.clone()).await;
        if poll["job"].is_object() {
            assigned = Some(poll["job"].clone());
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let assigned = assigned.expect("template work was never assigned");
    assert_eq!(assigned["run"]["kind"], "workspace");
    assert!(submitted["job"]["id"].is_string());
    assert_eq!(assigned["run"]["request_id"], "workspace-proof");
    let result = json!({"session":session,"epoch":hello["epoch"],"job":assigned["id"],"result":{"ok":true,"backend":"wsl2-layerfs"}});
    assert_eq!(
        call(&app, &token, "/v1/node/result", result).await.0,
        StatusCode::OK
    );
}
