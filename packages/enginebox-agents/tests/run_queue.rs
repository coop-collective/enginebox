//! Durable run queue, capacity-aware admission and pools, exercised through
//! the service with a stand-in executor and measurement (no live workspace).
use enginebox_agents::{
    AgentService,
    queue::{HostLoad, QueueFull, QueueLimits},
    security,
    service::{AdmittedRun, Callback, FinishedRun, LaunchOptions},
};
use enginebox_core::{Fleet, Host};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

const GIB: u64 = 1024 * 1024 * 1024;
const TOKEN_A: &str = "service-token:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const TOKEN_B: &str = "service-token:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const SESSION: &str = "0b9a5a6e-3c1f-4c7e-9a43-2f3e4d5c6b7a";

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
fn fleet(root: &Path) -> Fleet {
    let host = |id: &str, max: Option<u64>| Host {
        id: id.into(),
        target: format!("{id}.invalid"),
        max_agent_runs: max,
        ..Default::default()
    };
    Fleet::from_hosts(vec![host("h1", Some(4)), host("h2", None)], root).unwrap()
}
fn service(root: &Path, limits: QueueLimits) -> AgentService {
    AgentService::with_limits(root, fleet(root), BTreeMap::new(), limits).unwrap()
}
fn roomy() -> HostLoad {
    HostLoad {
        cpu_total: 64,
        reserve_cpu: 2,
        memory_available_bytes: 256 * GIB,
        reserve_memory_bytes: 4 * GIB,
    }
}
fn measure(service: &AgentService, load: Option<HostLoad>) {
    service.set_measure_hook(Arc::new(move |_| {
        load.ok_or_else(|| anyhow::anyhow!("offline"))
    }));
}

/// A stand-in executor: each run blocks until released and reports a session.
#[derive(Default)]
struct Gate {
    state: Mutex<GateState>,
    changed: Condvar,
}
#[derive(Default)]
struct GateState {
    open: bool,
    active: usize,
    max_active: usize,
    started: Vec<(String, String)>,
}
impl Gate {
    fn install(self: &Arc<Self>, service: &AgentService) {
        let gate = self.clone();
        service.set_run_executor(Arc::new(move |run: &AdmittedRun| {
            let mut s = gate.state.lock().unwrap();
            s.active += 1;
            s.max_active = s.max_active.max(s.active);
            s.started.push((run.id.clone(), run.name.clone()));
            gate.changed.notify_all();
            while !s.open {
                s = gate.changed.wait(s).unwrap();
            }
            s.active -= 1;
            gate.changed.notify_all();
            let session = run.request.resume_id.clone().unwrap_or(SESSION.into());
            Ok(json!({"exit_code":0,"output":json!({"type":"result","session_id":session}).to_string(),
                "timed_out":false,"output_truncated":false}))
        }));
    }
    fn open(&self) {
        self.state.lock().unwrap().open = true;
        self.changed.notify_all();
    }
    fn wait_started(&self, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut s = self.state.lock().unwrap();
        while s.started.len() < n {
            assert!(
                Instant::now() < deadline,
                "only {} started",
                s.started.len()
            );
            s = self
                .changed
                .wait_timeout(s, Duration::from_millis(50))
                .unwrap()
                .0;
        }
    }
}
fn wait_for(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn body(prompt: &str) -> Value {
    json!({"provider":"claude","prompt":prompt,"env":{"API_TOKEN":"secret-value-123"}})
}
fn start(service: &AgentService, name: &str, principal: &str, prompt: &str) -> Value {
    service
        .launch_with(
            "h1",
            name,
            body(prompt),
            principal,
            LaunchOptions::default(),
        )
        .unwrap()
        .0
}
fn status(service: &AgentService, name: &str, id: &Value) -> String {
    service.get_run("h1", name, id.as_str().unwrap()).unwrap()["status"]
        .as_str()
        .unwrap()
        .to_owned()
}
fn pool(service: &AgentService, value: Value) {
    service
        .pools()
        .put(value, &|h| h == "h1" || h == "h2")
        .unwrap();
}

#[test]
fn a_start_is_queued_then_runs_then_succeeds() {
    let (_t, root) = root();
    let service = service(&root, QueueLimits::default());
    let gate = Arc::new(Gate::default());
    gate.install(&service);
    measure(&service, Some(roomy()));
    let run = start(&service, "ebox-a", "owner", "build");
    // The scheduler may admit the run before the start's view is built: it is
    // then already running, with no waiting reason (and no queue position).
    assert!(run["status"] == "queued" || run["status"] == "running");
    assert!(
        run["waiting_reason"].is_string()
            || (run["waiting_reason"].is_null() && run["queue_position"].is_null())
    );
    gate.wait_started(1);
    wait_for("running", || {
        status(&service, "ebox-a", &run["id"]) == "running"
    });
    let running = service
        .get_run("h1", "ebox-a", run["id"].as_str().unwrap())
        .unwrap();
    assert!(running["queue_position"].is_null() && running["waiting_reason"].is_null());
    gate.open();
    wait_for("success", || {
        status(&service, "ebox-a", &run["id"]) == "succeeded"
    });
}

#[test]
fn queued_runs_survive_a_restart_and_are_admitted_in_order() {
    let (_t, root) = root();
    let first = service(&root, QueueLimits::default());
    measure(&first, None);
    pool(
        &first,
        json!({"name":"workers","workspaces":[{"host_id":"h1","name":"ebox-p1"}]}),
    );
    let a = start(&first, "ebox-a", "owner", "first");
    let b = start(&first, "ebox-a", "owner", "second");
    let (p, _) = first
        .launch_pool("workers", body("pooled"), TOKEN_A, LaunchOptions::default())
        .unwrap();
    assert_eq!(p["status"], "queued");
    assert!(p["host_id"].is_null() && p["name"].is_null());
    wait_for("a waiting reason", || {
        first
            .get_run("h1", "ebox-a", a["id"].as_str().unwrap())
            .unwrap()["waiting_reason"]
            .as_str()
            .unwrap_or("")
            .contains("measurement unavailable")
    });
    let queued = std::fs::read_dir(root.join("agent-run-queue"))
        .unwrap()
        .count();
    assert_eq!(queued, 3);
    // Env values wait in the private queue only, never in records or sidecars.
    for dir in ["agent-runs", "agent-run-details"] {
        for f in std::fs::read_dir(root.join(dir)).unwrap() {
            let text = std::fs::read_to_string(f.unwrap().path()).unwrap();
            assert!(!text.contains("secret-value-123"), "{dir} leaked env");
        }
    }
    first.close();
    drop(first);

    let second = service(&root, QueueLimits::default());
    assert_eq!(status(&second, "ebox-a", &a["id"]), "queued");
    assert_eq!(status(&second, "ebox-a", &b["id"]), "queued");
    let pooled = second
        .get_pool_run("workers", p["id"].as_str().unwrap())
        .unwrap();
    assert_eq!(pooled["status"], "queued");
    let gate = Arc::new(Gate::default());
    gate.install(&second);
    measure(&second, Some(roomy()));
    second.start_scheduler();
    gate.wait_started(2);
    {
        let s = gate.state.lock().unwrap();
        // One writer on ebox-a: the earlier run goes first; the pool run runs alongside.
        assert!(
            s.started
                .iter()
                .any(|(id, _)| id == a["id"].as_str().unwrap())
        );
        assert!(
            !s.started
                .iter()
                .any(|(id, _)| id == b["id"].as_str().unwrap())
        );
    }
    let landed = second
        .get_pool_run("workers", p["id"].as_str().unwrap())
        .unwrap();
    assert_eq!(landed["host_id"], "h1");
    assert_eq!(landed["name"], "ebox-p1");
    gate.open();
    wait_for("all finished", || {
        status(&second, "ebox-a", &b["id"]) == "succeeded"
    });
    // The queue file is gone and env never reached a record.
    wait_for("queue drained", || {
        std::fs::read_dir(root.join("agent-run-queue"))
            .unwrap()
            .count()
            == 0
    });
    let text = std::fs::read_to_string(
        root.join("agent-runs")
            .join(format!("{}.json", b["id"].as_str().unwrap())),
    )
    .unwrap();
    assert!(!text.contains("secret-value-123"));
}

#[test]
fn principals_share_capacity_round_robin() {
    let (_t, root) = root();
    let service = service(&root, QueueLimits::default());
    measure(&service, None);
    let mut heavy = vec![];
    for i in 0..6 {
        heavy.push(start(&service, &format!("ebox-a{i}"), TOKEN_A, "heavy"));
    }
    let light = start(&service, "ebox-b0", TOKEN_B, "light");
    let gate = Arc::new(Gate::default());
    gate.install(&service);
    // h1 admits 4 at a time; the light client's run must be among them.
    measure(&service, Some(roomy()));
    gate.wait_started(4);
    std::thread::sleep(Duration::from_millis(200));
    let started: Vec<String> = gate
        .state
        .lock()
        .unwrap()
        .started
        .iter()
        .map(|s| s.0.clone())
        .collect();
    assert_eq!(started.len(), 4);
    assert!(started.contains(&light["id"].as_str().unwrap().to_owned()));
    // FIFO within a principal: the heavy client's first three ran first.
    for run in &heavy[..3] {
        assert!(started.contains(&run["id"].as_str().unwrap().to_owned()));
    }
    let waiting = service
        .get_run("h1", "ebox-a5", heavy[5]["id"].as_str().unwrap())
        .unwrap();
    assert_eq!(waiting["status"], "queued");
    assert!(waiting["queue_position"].as_u64().unwrap() >= 1);
    assert!(
        waiting["waiting_reason"]
            .as_str()
            .unwrap()
            .contains("waiting for capacity on host h1")
    );
    let listed = service.list_runs();
    assert!(listed["runs"].as_array().unwrap().iter().any(|r| {
        r["waiting_reason"]
            .as_str()
            .is_some_and(|s| s.contains("host h1"))
    }));
    gate.open();
}

#[test]
fn hard_bounds_refuse_with_retry_after() {
    let (_t, root) = root();
    let limits = QueueLimits {
        max_queued_total: 3,
        max_queued_per_principal: 2,
        retry_after_seconds: 7,
        ..Default::default()
    };
    let service = service(&root, limits);
    measure(&service, None);
    start(&service, "ebox-a", TOKEN_A, "1");
    start(&service, "ebox-b", TOKEN_A, "2");
    let refused = service
        .launch_with("h1", "ebox-c", body("3"), TOKEN_A, LaunchOptions::default())
        .unwrap_err();
    let full = refused.downcast_ref::<QueueFull>().expect("queue full");
    assert_eq!(full.retry_after_seconds, 7);
    // Another principal still gets in until the total bound.
    start(&service, "ebox-c", TOKEN_B, "3");
    let refused = service
        .launch_with("h1", "ebox-d", body("4"), "owner", LaunchOptions::default())
        .unwrap_err();
    assert!(refused.downcast_ref::<QueueFull>().is_some());
}

#[test]
fn admission_waits_for_measured_memory() {
    let (_t, root) = root();
    let service = service(&root, QueueLimits::default());
    // 4 GiB reserve + 1 GiB per run from 6 GiB free: two runs at a time.
    let tight = HostLoad {
        memory_available_bytes: 6 * GIB,
        ..roomy()
    };
    measure(&service, Some(tight));
    let gate = Arc::new(Gate::default());
    gate.install(&service);
    let runs: Vec<_> = (0..4)
        .map(|i| start(&service, &format!("ebox-m{i}"), "owner", "m"))
        .collect();
    gate.wait_started(2);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(gate.state.lock().unwrap().started.len(), 2);
    let last = service
        .get_run("h1", "ebox-m3", runs[3]["id"].as_str().unwrap())
        .unwrap();
    assert!(
        last["waiting_reason"]
            .as_str()
            .unwrap()
            .contains("(memory)")
    );
    gate.open();
    wait_for("all finished", || {
        (0..4).all(|i| status(&service, &format!("ebox-m{i}"), &runs[i]["id"]) == "succeeded")
    });
    assert!(gate.state.lock().unwrap().max_active <= 2);
}

#[test]
fn pool_runs_spread_resume_on_their_workspace_and_grow_the_pool() {
    let (_t, root) = root();
    let service = service(&root, QueueLimits::default());
    measure(&service, Some(roomy()));
    let created: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = created.clone();
    service.set_create_hook(Arc::new(move |host, name, template, _| {
        assert_eq!((host, template), ("h2", "native-agents"));
        sink.lock().unwrap().push(name.to_owned());
        Ok(())
    }));
    pool(
        &service,
        json!({"name":"workers","template":"native-agents",
            "workspaces":[{"host_id":"h1","name":"ebox-p1"},{"host_id":"h1","name":"ebox-p2"}],
            "autoscale":{"hosts":["h2"],"max_workspaces":3}}),
    );
    let gate = Arc::new(Gate::default());
    gate.install(&service);
    let ids: Vec<String> = (0..3)
        .map(|_| {
            service
                .launch_pool("workers", body("pooled"), TOKEN_A, LaunchOptions::default())
                .unwrap()
                .0["id"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    gate.wait_started(3);
    assert_eq!(created.lock().unwrap().as_slice(), ["ebox-workers-1"]);
    let mut names: Vec<String> = ids
        .iter()
        .map(|id| {
            service.get_pool_run("workers", id).unwrap()["name"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    names.sort();
    assert_eq!(names, ["ebox-p1", "ebox-p2", "ebox-workers-1"]);
    let created_run = ids
        .iter()
        .find(|id| service.get_pool_run("workers", id).unwrap()["name"] == "ebox-workers-1")
        .unwrap();
    assert_eq!(
        service.get_pool_run("workers", created_run).unwrap()["host_id"],
        "h2"
    );
    gate.open();
    wait_for("pool runs finished", || {
        ids.iter()
            .all(|id| service.get_pool_run("workers", id).unwrap()["status"] == "succeeded")
    });
    // Resume lands on the workspace that holds the session.
    let first = service.get_pool_run("workers", &ids[0]).unwrap();
    let session = first["native_session_id"].as_str().unwrap().to_owned();
    let (resumed, _) = service
        .launch_pool(
            "workers",
            json!({"provider":"claude","prompt":"continue","resume_id":session}),
            TOKEN_A,
            LaunchOptions::default(),
        )
        .unwrap();
    let id = resumed["id"].as_str().unwrap().to_owned();
    wait_for("resume finished", || {
        service.get_pool_run("workers", &id).unwrap()["status"] == "succeeded"
    });
    let all = service.list_runs();
    let landed = all["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == id.as_str())
        .unwrap()
        .clone();
    assert_eq!(landed["pool"], "workers");
    // All runs report the same session here, so the index holds its latest workspace.
    assert!(["ebox-p1", "ebox-p2", "ebox-workers-1"].contains(&landed["name"].as_str().unwrap()));
    // An unknown session is refused rather than run anywhere.
    let error = service
        .launch_pool(
            "workers",
            json!({"provider":"claude","prompt":"x","resume_id":"11111111-2222-3333-4444-555555555555"}),
            TOKEN_A,
            LaunchOptions::default(),
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("not a session started in this pool")
    );
}

#[test]
fn a_resume_waits_for_the_workspace_that_holds_its_session() {
    let (_t, root) = root();
    let service = service(&root, QueueLimits::default());
    measure(&service, Some(roomy()));
    pool(
        &service,
        json!({"name":"workers","workspaces":[{"host_id":"h1","name":"ebox-p1"},{"host_id":"h1","name":"ebox-p2"}]}),
    );
    let gate = Arc::new(Gate::default());
    gate.install(&service);
    gate.open();
    let (first, _) = service
        .launch_pool("workers", body("start"), "owner", LaunchOptions::default())
        .unwrap();
    let first_id = first["id"].as_str().unwrap().to_owned();
    wait_for("first finished", || {
        service.get_pool_run("workers", &first_id).unwrap()["status"] == "succeeded"
    });
    let home = service.get_pool_run("workers", &first_id).unwrap()["name"]
        .as_str()
        .unwrap()
        .to_owned();
    for _ in 0..3 {
        let (run, _) = service
            .launch_pool(
                "workers",
                json!({"provider":"claude","prompt":"again","resume_id":SESSION}),
                "owner",
                LaunchOptions::default(),
            )
            .unwrap();
        let id = run["id"].as_str().unwrap().to_owned();
        wait_for("resume finished", || {
            service.get_pool_run("workers", &id).unwrap()["status"] == "succeeded"
        });
        assert_eq!(service.get_pool_run("workers", &id).unwrap()["name"], home);
    }
}

#[test]
fn idempotency_holds_across_queueing_and_is_scoped_by_pool() {
    let (_t, root) = root();
    let service = service(&root, QueueLimits::default());
    measure(&service, None);
    for name in ["workers", "other"] {
        pool(
            &service,
            json!({"name":name,"workspaces":[{"host_id":"h1","name":format!("ebox-{name}")}]}),
        );
    }
    let keyed = || LaunchOptions {
        idempotency_key: Some("job-1".into()),
        ..Default::default()
    };
    let (run, replayed) = service
        .launch_pool("workers", body("x"), TOKEN_A, keyed())
        .unwrap();
    assert!(!replayed);
    let (again, replayed) = service
        .launch_pool("workers", body("x"), TOKEN_A, keyed())
        .unwrap();
    assert!(replayed);
    assert_eq!(again["id"], run["id"]);
    assert_eq!(again["status"], "queued");
    // The same key in another pool, or from another principal, is a new run.
    let (elsewhere, replayed) = service
        .launch_pool("other", body("x"), TOKEN_A, keyed())
        .unwrap();
    assert!(!replayed && elsewhere["id"] != run["id"]);
    let (other, replayed) = service
        .launch_pool("workers", body("x"), TOKEN_B, keyed())
        .unwrap();
    assert!(!replayed && other["id"] != run["id"]);
    // A different request under the same key is refused.
    assert!(
        service
            .launch_pool("workers", body("changed"), TOKEN_A, keyed())
            .is_err()
    );
    // Running, then finished: still the same run.
    let gate = Arc::new(Gate::default());
    gate.install(&service);
    gate.open();
    measure(&service, Some(roomy()));
    let id = run["id"].as_str().unwrap().to_owned();
    wait_for("finished", || {
        service.get_pool_run("workers", &id).unwrap()["status"] == "succeeded"
    });
    let (done, replayed) = service
        .launch_pool("workers", body("x"), TOKEN_A, keyed())
        .unwrap();
    assert!(replayed);
    assert_eq!(done["id"], run["id"]);
    assert_eq!(done["status"], "succeeded");
    assert_eq!(done["name"], "ebox-workers");
}

fn counting_hook(service: &AgentService) -> Arc<Mutex<Vec<FinishedRun>>> {
    let seen: Arc<Mutex<Vec<FinishedRun>>> = Arc::default();
    let sink = seen.clone();
    service.set_finish_hook(Arc::new(move |f| sink.lock().unwrap().push(f)));
    seen
}
fn callback() -> LaunchOptions {
    LaunchOptions {
        callback: Some(Callback {
            url: "https://hooks.example.com/runs".into(),
            secret: "k".repeat(40),
        }),
        ..Default::default()
    }
}

#[test]
fn the_callback_fires_once_per_run_through_queue_restart_and_cancel() {
    let (_t, root) = root();
    let first = service(&root, QueueLimits::default());
    let seen = counting_hook(&first);
    measure(&first, None);
    pool(
        &first,
        json!({"name":"workers","workspaces":[{"host_id":"h1","name":"ebox-p1"}]}),
    );
    let ran = first
        .launch_with("h1", "ebox-a", body("run"), "owner", callback())
        .unwrap()
        .0;
    let cancelled = first
        .launch_with("h1", "ebox-b", body("cancel"), "owner", callback())
        .unwrap()
        .0;
    let pool_cancelled = first
        .launch_pool("workers", body("cancel"), "owner", callback())
        .unwrap()
        .0;
    first
        .cancel_run("h1", "ebox-b", cancelled["id"].as_str().unwrap())
        .unwrap();
    let view = first
        .cancel_pool_run("workers", pool_cancelled["id"].as_str().unwrap())
        .unwrap();
    assert_eq!(view["status"], "cancelled");
    assert_eq!(seen.lock().unwrap().len(), 2);
    assert_eq!(seen.lock().unwrap()[1].payload["pool"], "workers");
    assert_eq!(seen.lock().unwrap()[1].payload["status"], "cancelled");
    // A second cancel does not fire again.
    first
        .cancel_pool_run("workers", pool_cancelled["id"].as_str().unwrap())
        .unwrap();
    assert_eq!(seen.lock().unwrap().len(), 2);
    first.close();
    drop(first);
    // The queued run's callback survives the restart and fires when it ends.
    let second = service(&root, QueueLimits::default());
    let seen = counting_hook(&second);
    assert!(seen.lock().unwrap().is_empty());
    let gate = Arc::new(Gate::default());
    gate.install(&second);
    gate.open();
    measure(&second, Some(roomy()));
    second.start_scheduler();
    wait_for("finished", || {
        status(&second, "ebox-a", &ran["id"]) == "succeeded"
    });
    wait_for("callback", || seen.lock().unwrap().len() == 1);
    std::thread::sleep(Duration::from_millis(200));
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].id, ran["id"].as_str().unwrap());
    assert_eq!(seen[0].payload["status"], "succeeded");
}

#[test]
fn a_run_interrupted_by_a_restart_owes_its_callback_once() {
    let (_t, root) = root();
    let first = service(&root, QueueLimits::default());
    let gate = Arc::new(Gate::default());
    gate.install(&first);
    measure(&first, Some(roomy()));
    let run = first
        .launch_with("h1", "ebox-a", body("long"), "owner", callback())
        .unwrap()
        .0;
    gate.wait_started(1);
    // Simulate a crash: a new controller loads state while the run "runs".
    let second = service(&root, QueueLimits::default());
    let seen = counting_hook(&second);
    assert_eq!(seen.lock().unwrap().len(), 1);
    let payload = &seen.lock().unwrap()[0].payload;
    assert_eq!(payload["status"], "failed");
    assert_eq!(payload["execution_uncertain"], true);
    assert_eq!(payload["run_id"], run["id"]);
    // A further restart owes nothing.
    let third = service(&root, QueueLimits::default());
    assert!(counting_hook(&third).lock().unwrap().is_empty());
    first.close();
    gate.open();
}

/// What the stand-in guest lease probe answers.
#[derive(Clone, Copy, PartialEq)]
enum LeaseAnswer {
    Inactive,
    Active,
    Unreachable,
}
fn lease_probe(service: &AgentService, answer: Arc<Mutex<LeaseAnswer>>) -> Arc<AtomicUsize> {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    service.set_lease_probe(Arc::new(move |host, name, provider| {
        assert_eq!(
            (host.id.as_str(), name, provider),
            ("h1", "ebox-p1", "claude")
        );
        counter.fetch_add(1, Ordering::SeqCst);
        match *answer.lock().unwrap() {
            LeaseAnswer::Inactive => Ok(false),
            LeaseAnswer::Active => Ok(true),
            LeaseAnswer::Unreachable => Err(anyhow::anyhow!("ssh: connection refused")),
        }
    }));
    calls
}
/// A pool of one workspace whose first run fails after the guest may have
/// started (`guest_launch_failed`), leaving its execution uncertain.
fn uncertain_pool(limits: QueueLimits) -> (tempfile::TempDir, AgentService, String) {
    let (t, root) = root();
    let service = service(&root, limits);
    measure(&service, Some(roomy()));
    pool(
        &service,
        json!({"name":"workers","workspaces":[{"host_id":"h1","name":"ebox-p1"}]}),
    );
    let first = Arc::new(AtomicBool::new(true));
    service.set_run_executor(Arc::new(move |run: &AdmittedRun| {
        if first.swap(false, Ordering::SeqCst) {
            anyhow::bail!("guest helper exited before reporting");
        }
        let session = run.request.resume_id.clone().unwrap_or(SESSION.into());
        Ok(
            json!({"exit_code":0,"output":json!({"type":"result","session_id":session}).to_string(),
            "timed_out":false,"output_truncated":false}),
        )
    }));
    let (run, _) = service
        .launch_pool("workers", body("first"), TOKEN_A, LaunchOptions::default())
        .unwrap();
    let id = run["id"].as_str().unwrap().to_owned();
    wait_for("first run failed", || {
        service.get_pool_run("workers", &id).unwrap()["status"] == "failed"
    });
    let failed = service.get_pool_run("workers", &id).unwrap();
    assert_eq!(failed["execution_uncertain"], true);
    (t, service, id)
}
fn launch_second(service: &AgentService) -> String {
    service
        .launch_pool("workers", body("second"), TOKEN_A, LaunchOptions::default())
        .unwrap()
        .0["id"]
        .as_str()
        .unwrap()
        .to_owned()
}
fn waiting_reason(service: &AgentService, id: &str) -> String {
    service.get_pool_run("workers", id).unwrap()["waiting_reason"]
        .as_str()
        .unwrap_or("")
        .to_owned()
}

#[test]
fn a_pool_run_behind_an_uncertain_but_idle_workspace_is_placed() {
    let (_t, service, first) = uncertain_pool(QueueLimits::default());
    let answer = Arc::new(Mutex::new(LeaseAnswer::Inactive));
    let calls = lease_probe(&service, answer);
    let second = launch_second(&service);
    wait_for("second run succeeded", || {
        service.get_pool_run("workers", &second).unwrap()["status"] == "succeeded"
    });
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        service.get_pool_run("workers", &first).unwrap()["execution_uncertain"],
        false
    );
    let events = service.launch_events(&Default::default());
    let probe = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "uncertain_lease_probe")
        .expect("probe event")
        .clone();
    assert_eq!(probe["outcome"], "succeeded");
    assert_eq!(probe["reason"], "lease_inactive");
    assert_eq!(probe["pool"], "workers");
    assert_eq!(probe["run_id"], first.as_str());
}

#[test]
fn an_active_lease_keeps_the_fence_and_probing_is_rate_limited() {
    let (_t, service, _) = uncertain_pool(QueueLimits::default());
    let answer = Arc::new(Mutex::new(LeaseAnswer::Active));
    let calls = lease_probe(&service, answer.clone());
    let second = launch_second(&service);
    wait_for("lease reason", || {
        waiting_reason(&service, &second).contains("native session lease is still active")
    });
    let reason = waiting_reason(&service, &second);
    assert!(
        reason.starts_with(
            "waiting for a free workspace in pool workers (ebox-p1: execution uncertain"
        ),
        "{reason}"
    );
    // The scheduler passes every 2 seconds, yet probes at most once a minute.
    std::thread::sleep(Duration::from_millis(4500));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        service.get_pool_run("workers", &second).unwrap()["status"],
        "queued"
    );
    // An inactive lease seen on a later probe releases the workspace.
    *answer.lock().unwrap() = LeaseAnswer::Inactive;
    service.set_limits(QueueLimits {
        uncertain_probe_seconds: 1,
        ..Default::default()
    });
    wait_for("second run succeeded", || {
        service.get_pool_run("workers", &second).unwrap()["status"] == "succeeded"
    });
}

#[test]
fn a_failed_lease_probe_keeps_the_fence() {
    let limits = QueueLimits {
        uncertain_probe_seconds: 1,
        ..Default::default()
    };
    let (_t, service, first) = uncertain_pool(limits);
    let answer = Arc::new(Mutex::new(LeaseAnswer::Unreachable));
    let calls = lease_probe(&service, answer);
    let second = launch_second(&service);
    wait_for("probe failure reason", || {
        waiting_reason(&service, &second).contains("lease probe failed: ssh: connection refused")
    });
    // Repeated failures keep probing (each interval) and never clear the fence.
    wait_for("a second probe", || calls.load(Ordering::SeqCst) >= 2);
    assert_eq!(
        service.get_pool_run("workers", &second).unwrap()["status"],
        "queued"
    );
    assert_eq!(
        service.get_pool_run("workers", &first).unwrap()["execution_uncertain"],
        true
    );
    // One event per change, not one per probe.
    let events = service.launch_events(&Default::default());
    let probes = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "uncertain_lease_probe")
        .count();
    assert_eq!(probes, 1);
}
