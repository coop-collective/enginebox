//! Launch telemetry: admission and placement failures and pool member
//! creations are logged as events with a stage and reason code, kept in a
//! bounded history, shown on the run as `failure`, and reported as codes only
//! in the terminal callback. Pools belong to projects.
use enginebox_agents::{
    AgentService,
    launch_events::{Filter, LaunchFailure, Stage},
    profile::ProviderProfile,
    queue::{HostLoad, QueueLimits},
    security,
    service::{AdmittedRun, Callback, FinishedRun, LaunchOptions, ResolvedProfile},
};
use enginebox_core::{Fleet, Host};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const GIB: u64 = 1024 * 1024 * 1024;
const SECRET_ENV: &str = "env-secret-value-0451";
const PROMPT: &str = "private prompt text";

fn root() -> (tempfile::TempDir, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().canonicalize().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let home = path.join("agents/homes/claude");
    security::private_dir(&home).unwrap();
    security::write_json(
        &home,
        ".config/enginebox/controller-token.json",
        &json!({"kind":"setup_token","value":"controller-token-private","expires_at":null}),
    )
    .unwrap();
    (t, path)
}
fn service(root: &Path) -> AgentService {
    let host = |id: &str| Host {
        id: id.into(),
        target: format!("{id}.invalid"),
        max_agent_runs: Some(4),
        ..Default::default()
    };
    let fleet = Fleet::from_hosts(vec![host("h1"), host("h2")], root).unwrap();
    AgentService::with_limits(root, fleet, BTreeMap::new(), QueueLimits::default()).unwrap()
}
fn measure(service: &AgentService, online: bool) {
    service.set_measure_hook(Arc::new(move |_| {
        if online {
            Ok(HostLoad {
                cpu_total: 64,
                reserve_cpu: 2,
                memory_available_bytes: 256 * GIB,
                reserve_memory_bytes: 4 * GIB,
            })
        } else {
            Err(anyhow::anyhow!("offline"))
        }
    }));
}
fn wait_for(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn body() -> Value {
    json!({"provider":"claude","prompt":PROMPT,"env":{"API_TOKEN":SECRET_ENV}})
}
fn callback_sink(service: &AgentService) -> Arc<Mutex<Vec<FinishedRun>>> {
    let seen: Arc<Mutex<Vec<FinishedRun>>> = Arc::default();
    let sink = seen.clone();
    service.set_finish_hook(Arc::new(move |f| sink.lock().unwrap().push(f)));
    seen
}
fn with_callback() -> LaunchOptions {
    LaunchOptions {
        callback: Some(Callback {
            url: "https://hooks.example.com/runs?token=abc".into(),
            secret: "k".repeat(40),
        }),
        ..Default::default()
    }
}
fn events(service: &AgentService) -> Vec<Value> {
    service.launch_events(&Filter::default())["events"]
        .as_array()
        .unwrap()
        .clone()
}
fn run_view(service: &AgentService, id: &str) -> Value {
    service.get_run("h1", "ebox-a", id).unwrap()
}

#[test]
fn a_placement_failure_fails_the_run_logs_an_event_and_reports_codes_only() {
    let (_t, root) = root();
    let service = service(&root);
    measure(&service, true);
    let seen = callback_sink(&service);
    // Stands in for workspace preparation finding the wrong template, with a
    // message that (wrongly) echoes a secret and a token.
    service.set_run_executor(Arc::new(|_run: &AdmittedRun| {
        Err(LaunchFailure::new(
            Stage::Place,
            "template_mismatch",
            format!("template differs; env {SECRET_ENV} Bearer abcdefghijklmnop0123456789xyz"),
        )
        .into())
    }));
    let (run, _) = service
        .launch_with("h1", "ebox-a", body(), "owner", with_callback())
        .unwrap();
    let id = run["id"].as_str().unwrap().to_owned();
    wait_for("failure", || run_view(&service, &id)["status"] == "failed");
    let view = run_view(&service, &id);
    assert_eq!(
        view["failure"],
        json!({"stage":"place","reason":"template_mismatch"})
    );
    assert!(view["execution_uncertain"].is_null() || view["execution_uncertain"] == false);
    assert!(
        view["error"]
            .as_str()
            .unwrap()
            .starts_with("Run could not start")
    );
    // The listing shows it too.
    let listed = service.list_runs();
    assert_eq!(
        listed["runs"][0]["failure"],
        json!({"stage":"place","reason":"template_mismatch"})
    );

    let events = events(&service);
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event["kind"], "run_admission_failed");
    assert_eq!(event["outcome"], "failed");
    assert_eq!(event["run_id"], id.as_str());
    assert_eq!(event["host_id"], "h1");
    assert_eq!(event["workspace"], "ebox-a");
    assert_eq!(event["provider"], "claude");
    assert_eq!(event["stage"], "place");
    assert_eq!(event["reason"], "template_mismatch");
    assert!(event["queue_wait_ms"].is_u64());
    assert!(event["duration_ms"].is_u64());
    let text = event.to_string();
    for forbidden in [
        SECRET_ENV,
        PROMPT,
        "abcdefghijklmnop0123456789xyz",
        "token=abc",
    ] {
        assert!(!text.contains(forbidden), "event leaks {forbidden}");
    }
    assert!(
        event["message"]
            .as_str()
            .unwrap()
            .contains("template differs")
    );

    wait_for("callback", || !seen.lock().unwrap().is_empty());
    let payload = seen.lock().unwrap()[0].payload.clone();
    assert_eq!(
        payload["failure"],
        json!({"stage":"place","reason":"template_mismatch"})
    );
    let text = payload.to_string();
    for forbidden in [
        SECRET_ENV,
        PROMPT,
        "template differs",
        "Run could not start",
    ] {
        assert!(!text.contains(forbidden), "callback leaks {forbidden}");
    }
}

/// A simulated failure stage and reason; None for a plain guest error.
type Step = Option<(Stage, &'static str)>;

#[test]
fn each_stage_is_recorded_and_a_guest_launch_failure_stays_uncertain() {
    let (_t, root) = root();
    let service = service(&root);
    measure(&service, true);
    let plan: Arc<Mutex<Vec<Step>>> = Arc::new(Mutex::new(vec![
        Some((Stage::Delegate, "provider_not_delegable")),
        Some((Stage::Bootstrap, "workspace_locked")),
        None, // a plain error: the guest helper failed to launch the run
    ]));
    let next = plan.clone();
    service.set_run_executor(Arc::new(move |_run: &AdmittedRun| {
        match next.lock().unwrap().remove(0) {
            Some((stage, reason)) => Err(LaunchFailure::new(stage, reason, "simulated").into()),
            None => Err(anyhow::anyhow!("guest helper exited")),
        }
    }));
    let mut ids = vec![];
    for name in ["ebox-a", "ebox-b", "ebox-c"] {
        let (run, _) = service
            .launch_with("h1", name, body(), "owner", LaunchOptions::default())
            .unwrap();
        let id = run["id"].as_str().unwrap().to_owned();
        wait_for("terminal", || {
            service.get_run("h1", name, &id).unwrap()["status"] == "failed"
        });
        ids.push((name, id));
    }
    let failure = |i: usize| {
        let (name, id) = &ids[i];
        service.get_run("h1", name, id).unwrap()
    };
    assert_eq!(failure(0)["failure"]["stage"], "delegate");
    assert_eq!(failure(1)["failure"]["stage"], "bootstrap");
    let start = failure(2);
    assert_eq!(
        start["failure"],
        json!({"stage":"start","reason":"guest_launch_failed"})
    );
    assert_eq!(start["execution_uncertain"], true);
    let stages: Vec<String> = events(&service)
        .iter()
        .map(|e| e["stage"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(stages, ["start", "bootstrap", "delegate"], "newest first");
}

#[test]
fn a_profile_gone_after_a_restart_fails_at_resolve() {
    let (_t, root) = root();
    let first = service(&root);
    measure(&first, false); // nothing is admitted before the restart
    let profile = ResolvedProfile {
        name: "coder".into(),
        revision: 3,
        template: None,
        block: ProviderProfile::default(),
        files: BTreeMap::new(),
    };
    let (run, _) = first
        .launch_with(
            "h1",
            "ebox-a",
            body(),
            "owner",
            LaunchOptions {
                profile: Some(profile),
                ..Default::default()
            },
        )
        .unwrap();
    let id = run["id"].as_str().unwrap().to_owned();
    first.close();
    drop(first);
    let second = service(&root);
    second.set_profile_resolver(Arc::new(|_, _| Err(anyhow::anyhow!("Unknown profile"))));
    measure(&second, true);
    second.start_scheduler();
    wait_for("resolve failure", || {
        run_view(&second, &id)["status"] == "failed"
    });
    assert_eq!(
        run_view(&second, &id)["failure"],
        json!({"stage":"resolve","reason":"profile_unavailable"})
    );
    let event = &events(&second)[0];
    assert_eq!(event["profile"], "coder");
    assert_eq!(event["stage"], "resolve");
}

#[test]
fn pool_member_creations_are_logged_with_their_project_and_survive_a_restart() {
    let (_t, root) = root();
    let service = service(&root);
    measure(&service, true);
    let attempts = Arc::new(Mutex::new(0));
    let counter = attempts.clone();
    service.set_create_hook(Arc::new(move |_host, _name, _template, _size| {
        let mut n = counter.lock().unwrap();
        *n += 1;
        if *n == 1 {
            anyhow::bail!("incus: image missing; token ghp_0123456789abcdefghijklmnop")
        }
        Ok(())
    }));
    service
        .pools()
        .put(
            json!({"name":"grow","project":"alpha","template":"native-agents",
                "autoscale":{"hosts":["h2"],"max_workspaces":2}}),
            &|h| h == "h1" || h == "h2",
        )
        .unwrap();
    service.set_run_executor(Arc::new(|_run: &AdmittedRun| {
        Ok(json!({"exit_code":0,"output":"","timed_out":false,"output_truncated":false}))
    }));
    service
        .launch_pool("grow", body(), "owner", LaunchOptions::default())
        .unwrap();
    wait_for("failed creation", || {
        events(&service)
            .iter()
            .any(|e| e["kind"] == "pool_member_create_failed")
    });
    let failed = events(&service)
        .into_iter()
        .find(|e| e["kind"] == "pool_member_create_failed")
        .unwrap();
    assert_eq!(failed["pool"], "grow");
    assert_eq!(failed["project"], "alpha");
    assert_eq!(failed["host_id"], "h2");
    assert_eq!(failed["template"], "native-agents");
    assert_eq!(failed["workspace"], "ebox-grow-1");
    assert_eq!(failed["stage"], "create");
    assert_eq!(failed["reason"], "create_failed");
    assert!(!failed.to_string().contains("ghp_0123456789"));
    let status = service.pool_status("grow").unwrap();
    assert!(
        !status["pool"]["last_create_error"]
            .to_string()
            .contains("ghp_0123456789")
    );
    // The project filter on the history.
    let alpha = service.launch_events(&Filter {
        project: Some("alpha".into()),
        ..Default::default()
    });
    assert_eq!(alpha["events"].as_array().unwrap().len(), 1);
    drop(service);
    let reopened = self::service(&root);
    let kept = events(&reopened);
    assert_eq!(kept.len(), 1, "history is persisted");
    assert_eq!(kept[0]["kind"], "pool_member_create_failed");
    let raw = std::fs::read_to_string(root.join("launch-events.json")).unwrap();
    assert!(!raw.contains(SECRET_ENV) && !raw.contains(PROMPT));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(root.join("launch-events.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "launch history is private");
    }
}

#[test]
fn a_successful_member_creation_is_logged() {
    let (_t, root) = root();
    let service = service(&root);
    measure(&service, true);
    service.set_create_hook(Arc::new(|_, _, _, _| Ok(())));
    service
        .pools()
        .put(
            json!({"name":"grow","template":"native-agents",
                "autoscale":{"hosts":["h2"],"max_workspaces":1}}),
            &|h| h == "h2",
        )
        .unwrap();
    service.set_run_executor(Arc::new(|_run: &AdmittedRun| {
        Ok(json!({"exit_code":0,"output":"","timed_out":false,"output_truncated":false}))
    }));
    service
        .launch_pool("grow", body(), "owner", LaunchOptions::default())
        .unwrap();
    wait_for("created", || {
        events(&service)
            .iter()
            .any(|e| e["kind"] == "pool_member_created")
    });
    let created = &events(&service)[0];
    assert_eq!(created["outcome"], "succeeded");
    assert_eq!(created["reason"], "ok");
    assert_eq!(
        created["project"], "default",
        "pools without a project use the default"
    );
    assert_eq!(created["workspace"], "ebox-grow-1");
}

#[test]
fn pools_belong_to_projects_and_list_by_project() {
    let (_t, root) = root();
    let service = service(&root);
    let known = |h: &str| h == "h1";
    let put = |v: Value| service.pools().put(v, &known);
    // No project: the default project (migration path for older pools too).
    let legacy =
        put(json!({"name":"legacy","workspaces":[{"host_id":"h1","name":"ebox-l1"}]})).unwrap();
    assert_eq!(legacy["pool"]["project"], "default");
    put(json!({"name":"a-one","project":"alpha","workspaces":[{"host_id":"h1","name":"ebox-a1"}]}))
        .unwrap();
    put(json!({"name":"a-two","project":"alpha","workspaces":[{"host_id":"h1","name":"ebox-a2"}]}))
        .unwrap();
    assert!(
        put(json!({"name":"bad","project":"Not_Valid","workspaces":[{"host_id":"h1","name":"ebox-b"}]}))
            .is_err()
    );
    // Replacing without a project keeps the pool's project.
    let replaced = put(
        json!({"name":"a-two","description":"x","workspaces":[{"host_id":"h1","name":"ebox-a2"}]}),
    )
    .unwrap();
    assert_eq!(replaced["pool"]["project"], "alpha");
    let all = service.list_pools();
    assert_eq!(
        all["projects"],
        json!([{"project":"alpha","pools":["a-one","a-two"]},{"project":"default","pools":["legacy"]}])
    );
    let alpha = service.list_project_pools("alpha").unwrap();
    let names: Vec<&str> = alpha["pools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["a-one", "a-two"]);
    assert!(service.list_project_pools("../x").is_err());
    // A record written before projects existed loads into the default project.
    let dir = root.join("agent-pools");
    security::write_json(
        &dir,
        "old.json",
        &json!({"name":"old","workspaces":[{"host_id":"h1","name":"ebox-o1"}],"revision":1}),
    )
    .unwrap();
    drop(service);
    let reopened = self::service(&root);
    assert_eq!(reopened.pools().get("old").unwrap().project, "default");
}
