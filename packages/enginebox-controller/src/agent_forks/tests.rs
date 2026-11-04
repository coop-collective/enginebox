//! State-machine tests with a fake host (sessions, node capabilities, audit)
//! and a fake node driving the report channel.
use super::*;

const OWNER: &str = "usr_aaaaaaaaaaaaaaaaaaaaaaaa";
const ORG: &str = "default";
const NODE: &str = "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn";
const SID: &str = "0123456789abcdef0123456789abcdef";
const BASE: &str = "1111111111111111111111111111111111111111";
const HEAD: &str = "2222222222222222222222222222222222222222";
const MERGED: &str = "3333333333333333333333333333333333333333";
const REVERTED: &str = "4444444444444444444444444444444444444444";
const KEY_ALICE: &str = "xk_live_alice_key_0123456789abcdef";
const KEY_BOB: &str = "xk_live_bob_key_0123456789abcdefgh";

struct Fake {
    session: Mutex<SessionInfo>,
    caps: Mutex<Option<Caps>>,
    audit: Mutex<Vec<(String, String, bool)>>,
}
impl Fake {
    fn new() -> Self {
        Self {
            session: Mutex::new(SessionInfo {
                id: SID.into(),
                org: ORG.into(),
                project: "qoh".into(),
                repository: "queen-of-hearts".into(),
                project_path: "unity-project".into(),
                started_by: OWNER.into(),
                editor: true,
                ready: true,
                open: true,
                node: Some(NODE.into()),
                node_epoch: 7,
                generation: 1,
            }),
            caps: Mutex::new(Some(Caps {
                provider: "git".into(),
                slots: 4,
                isolation: "accounts".into(),
            })),
            audit: Mutex::new(vec![]),
        }
    }
    fn actions(&self) -> Vec<String> {
        self.audit
            .lock()
            .unwrap()
            .iter()
            .map(|(a, _, ok)| format!("{a}:{ok}"))
            .collect()
    }
}
impl Host for Fake {
    fn session(&self, id: &str) -> Option<SessionInfo> {
        let s = self.session.lock().unwrap().clone();
        (s.id == id).then_some(s)
    }
    fn caps(&self, node: &str) -> Option<Caps> {
        (node == NODE)
            .then(|| self.caps.lock().unwrap().clone())
            .flatten()
    }
    fn xinf(&self) -> Result<Option<Arc<dyn gateway::Xinf>>> {
        Ok(None)
    }
    fn audit(&self, _: &str, actor: &str, action: &str, _: &str, ok: bool, _: Value) {
        self.audit
            .lock()
            .unwrap()
            .push((action.into(), actor.into(), ok));
    }
}

fn room(user: &str, role: principal::Role) -> Caller {
    Caller::Room(principal::Room {
        client: "ebc_gamejam".into(),
        room: "jam".into(),
        user: user.into(),
        name: format!("{user} name"),
        role,
    })
}
fn alice() -> Caller {
    room("alice", principal::Role::Builder)
}
fn bob() -> Caller {
    room("bob", principal::Role::Builder)
}
fn owner() -> Caller {
    Caller::Member(OWNER.into())
}

fn call(
    forks: &AgentForks,
    host: &Fake,
    caller: &Caller,
    method: &str,
    tail: &str,
    body: Value,
) -> (u16, Value) {
    let (path, query) = tail.split_once('?').unwrap_or((tail, ""));
    let full = format!("/v1/projects/qoh/engine-sessions/{SID}{path}");
    let route = route(method, &full).unwrap_or_else(|| panic!("no route {method} {full}"));
    let query = match parse_query(route.action, Some(query)) {
        Ok(q) => q,
        Err(e) => return status_of(&e),
    };
    let req = Request {
        route,
        query,
        body,
        org: ORG.into(),
        caller: caller.clone(),
        grant_member: Some(OWNER.into()),
        return_origins: vec!["https://gamejam.example".into()],
    };
    match forks.handle(host, req) {
        Ok(Reply::Json(status, v)) => (status, v),
        Ok(Reply::Diff(text)) => (200, json!({"diff": text})),
        Err(e) => status_of(&e),
    }
}
fn poll(forks: &AgentForks, host: &Fake) -> Value {
    let mut polled = json!({"sessions": [{"id": SID, "generation": 1}]});
    forks.decorate_poll(host, NODE, &mut polled).unwrap();
    polled["sessions"][0].clone()
}
fn report(forks: &AgentForks, host: &Fake, body: Value) -> Result<Value> {
    let mut body = body;
    body["generation"] = json!(1);
    forks.report(host, NODE, 7, &body)
}
fn ops_of(row: &Value, fork: &str) -> Vec<Value> {
    row["forks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == fork)
        .map(|f| f["ops"].as_array().unwrap().clone())
        .unwrap_or_default()
}
fn open() -> (tempfile::TempDir, AgentForks, Fake) {
    let temp = tempfile::tempdir().unwrap();
    let forks = AgentForks::open(temp.path()).unwrap();
    forks
        .gateway
        .put_key(ORG, &alice().person(), KEY_ALICE)
        .unwrap();
    forks
        .gateway
        .put_key(ORG, &bob().person(), KEY_BOB)
        .unwrap();
    forks
        .gateway
        .put_key(ORG, OWNER, "xk_live_owner_key_0123456789abcd")
        .unwrap();
    (temp, forks, Fake::new())
}
fn create(forks: &AgentForks, host: &Fake, who: &Caller, request: &str) -> (u16, Value) {
    call(
        forks,
        host,
        who,
        "POST",
        "/forks",
        json!({"request_id": request, "prompt": "Make the cards glow", "provider": "claude", "paths": ["unity-project/Assets/Art/**"]}),
    )
}
/// Create a fork, run it on the fake node to `head` with `changed`.
fn fork_with_changes(
    forks: &AgentForks,
    host: &Fake,
    who: &Caller,
    request: &str,
    changed: Value,
) -> String {
    let (status, created) = create(forks, host, who, request);
    assert_eq!(status, 202, "{created}");
    let fork = created["fork"]["id"].as_str().unwrap().to_owned();
    let row = poll(forks, host);
    let run_op = ops_of(&row, &fork)
        .into_iter()
        .find(|o| o["kind"] == "run")
        .expect("a run op");
    report(
        forks,
        host,
        json!({"fork": fork, "state": "idle", "detail": {"base": BASE}}),
    )
    .unwrap();
    report(forks, host, json!({"fork": fork, "op_id": run_op["op_id"], "state": "running", "detail": {"run": {"native_session_id": "sess-1"}}})).unwrap();
    report(
        forks,
        host,
        json!({"fork": fork, "op_id": run_op["op_id"], "state": "done",
            "detail": {"head": HEAD, "changed": changed, "run": {"spent_usd": 0.42, "requests": 7, "exit": 0}}}),
    )
    .unwrap();
    fork
}

#[test]
fn a_content_fork_runs_submits_merges_and_applies() {
    let (_t, forks, host) = open();
    let fork = {
        let (status, created) = create(&forks, &host, &alice(), "r1");
        assert_eq!(status, 202, "{created}");
        assert_eq!(created["fork"]["state"], "creating");
        assert_eq!(created["run"]["state"], "queued");
        created["fork"]["id"].as_str().unwrap().to_owned()
    };
    // Retrying the same request answers the same fork.
    let (status, again) = create(&forks, &host, &alice(), "r1");
    assert_eq!(
        (status, again["fork"]["id"].as_str()),
        (202, Some(fork.as_str()))
    );
    // The node gets the fork with a run op and the run's bearer, nothing else's.
    let row = poll(&forks, &host);
    assert_eq!(row["dev"]["capture_on_save"], true);
    let ops = ops_of(&row, &fork);
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0]["kind"], "run");
    assert_eq!(ops[0]["run"]["provider"], "claude");
    assert_eq!(ops[0]["run"]["prompt"], "Make the cards glow");
    assert_eq!(ops[0]["run"]["budget_usd"], 2.0);
    assert_eq!(ops[0]["run"]["timeout_seconds"], 1800);
    assert_eq!(ops[0]["secrets"]["gateway"]["access_token"], KEY_ALICE);
    assert!(!row.to_string().contains(KEY_BOB));
    // Nothing on disk holds the bearer.
    let disk = std::fs::read_dir(forks.root.join("forks")).unwrap();
    for entry in disk {
        let text = std::fs::read_to_string(entry.unwrap().path()).unwrap();
        assert!(!text.contains(KEY_ALICE));
    }
    let op = ops[0]["op_id"].clone();
    report(
        &forks,
        &host,
        json!({"fork": fork, "state": "idle", "detail": {"base": BASE}}),
    )
    .unwrap();
    report(
        &forks,
        &host,
        json!({"fork": fork, "op_id": op, "state": "running"}),
    )
    .unwrap();
    // Events are redacted and paged.
    let run_id = ops[0]["run"]["run_id"].as_str().unwrap().to_owned();
    forks
        .events_report(
            &host,
            NODE,
            7,
            &json!({"fork": fork, "run_id": run_id, "events": [
                {"kind": "text", "data": {"text": format!("using {KEY_ALICE} now")}},
                {"kind": "tool", "data": {"name": "Edit"}},
            ]}),
        )
        .unwrap();
    let (status, events) = call(
        &forks,
        &host,
        &alice(),
        "GET",
        &format!("/forks/{fork}/runs/{run_id}/events?after=0"),
        json!({}),
    );
    assert_eq!(status, 200);
    assert_eq!(events["events"].as_array().unwrap().len(), 2);
    assert!(!events.to_string().contains(KEY_ALICE));
    assert_eq!(events["next"], 2);
    let (_, later) = call(
        &forks,
        &host,
        &alice(),
        "GET",
        &format!("/forks/{fork}/runs/{run_id}/events?after=1"),
        json!({}),
    );
    assert_eq!(later["events"].as_array().unwrap().len(), 1);
    // The run ends with a head and changed paths, classified here.
    report(
        &forks,
        &host,
        json!({"fork": fork, "op_id": op, "state": "done", "detail": {
            "head": HEAD,
            "changed": [
                {"path": "unity-project/Assets/Art/glow.mat", "status": "A", "mode": 100644, "bytes": 900, "class": "code"},
                {"path": "unity-project/Assets/Scenes/Main.unity", "status": "M", "mode": "100644", "bytes": 40000},
            ],
            "run": {"spent_usd": "0.420000000", "requests": 7, "exit": 0},
        }}),
    )
    .unwrap();
    // A repeated report of a finished op is answered, not redone.
    assert_eq!(
        report(
            &forks,
            &host,
            json!({"fork": fork, "op_id": op, "state": "done"})
        )
        .unwrap()["done"],
        true
    );
    let diff = "diff --git a/unity-project/Assets/Art/glow.mat b/unity-project/Assets/Art/glow.mat\nnew file mode 100644\n--- /dev/null\n+++ b/unity-project/Assets/Art/glow.mat\n@@ -0,0 +1,2 @@\n+%YAML 1.1\n+m_Name: glow\n";
    forks
        .artifact(
            &host,
            NODE,
            7,
            &json!({"fork": fork, "head": HEAD, "kind": "diff", "body": diff}),
        )
        .unwrap();
    let (status, view) = call(
        &forks,
        &host,
        &alice(),
        "GET",
        &format!("/forks/{fork}"),
        json!({}),
    );
    assert_eq!(status, 200);
    assert_eq!(view["state"], "idle");
    assert_eq!(view["head"], HEAD);
    assert_eq!(view["changed"][0]["class"], "asset");
    assert_eq!(view["changed"][1]["class"], "scene");
    assert_eq!(view["runs"][0]["state"], "succeeded");
    assert_eq!(view["runs"][0]["spent_usd"], 0.42);
    assert_eq!(view["has_diff"], true);
    let (_, d) = call(
        &forks,
        &host,
        &alice(),
        "GET",
        &format!("/forks/{fork}/diff?path=unity-project/Assets/Art/glow.mat&limit=4096"),
        json!({}),
    );
    assert!(d["diff"].as_str().unwrap().contains("m_Name: glow"));
    // Submit: a snapshot op freezes the head, then checks pass and it merges.
    let (status, sub) = call(
        &forks,
        &host,
        &alice(),
        "POST",
        &format!("/forks/{fork}/submit"),
        json!({"request_id": "s1", "message": "Glowing cards", "expected_head": HEAD}),
    );
    assert_eq!(status, 202, "{sub}");
    assert_eq!(sub["submission"]["state"], "checking");
    let sub_id = sub["submission"]["id"].as_str().unwrap().to_owned();
    // No runs while a submission is open.
    let (status, _) = call(
        &forks,
        &host,
        &alice(),
        "POST",
        &format!("/forks/{fork}/runs"),
        json!({"request_id": "x", "prompt": "more"}),
    );
    assert_eq!(status, 409);
    let snapshot = ops_of(&poll(&forks, &host), &fork)[0].clone();
    assert_eq!(snapshot["kind"], "snapshot");
    assert_eq!(snapshot["snapshot"]["expected_head"], HEAD);
    report(&forks, &host, json!({"fork": fork, "op_id": snapshot["op_id"], "state": "done", "detail": {"head": HEAD, "findings": []}})).unwrap();
    let (_, subs) = call(
        &forks,
        &host,
        &alice(),
        "GET",
        "/submissions?state=merging",
        json!({}),
    );
    assert_eq!(subs["submissions"][0]["id"], sub_id.as_str());
    assert_eq!(subs["submissions"][0]["classes"]["asset"], 1);
    assert_eq!(subs["submissions"][0]["classes"]["scene"], 1);
    let merge = ops_of(&poll(&forks, &host), &fork)[0].clone();
    assert_eq!(merge["kind"], "merge");
    assert_eq!(merge["merge"]["fork_head"], HEAD);
    assert_eq!(merge["merge"]["expected_dev_head"], BASE);
    assert!(merge["merge"]["approved_by"].is_null());
    // Held (Play mode), then applied by the dev report.
    report(&forks, &host, json!({"fork": fork, "op_id": merge["op_id"], "state": "held", "detail": {"merge": {"dev_head": MERGED}, "apply": {"applied": false, "held_reason": "playing"}}})).unwrap();
    let (_, dev) = call(&forks, &host, &bob(), "GET", "/dev", json!({}));
    assert_eq!(dev["head"], MERGED);
    assert_eq!(dev["held"]["reason"], "playing");
    assert_eq!(dev["pending"][0], sub_id.as_str());
    forks
        .dev_report(&host, &host.session(SID).unwrap(), &json!({"head": MERGED, "applied": MERGED, "held": null, "compile": {"state": "ok", "errors": []}}))
        .unwrap();
    let (_, subs) = call(&forks, &host, &alice(), "GET", "/submissions", json!({}));
    assert_eq!(subs["submissions"][0]["state"], "applied");
    assert_eq!(subs["submissions"][0]["dev_head_after"], MERGED);
    let (_, dev) = call(&forks, &host, &alice(), "GET", "/dev", json!({}));
    assert_eq!(dev["applied"], MERGED);
    assert_eq!(dev["pending"], json!([]));
    assert_eq!(dev["compile"]["state"], "ok");
    // The fork is idle again; the merge op is done (finished when "done"
    // arrives for the op).
    report(&forks, &host, json!({"fork": fork, "op_id": merge["op_id"], "state": "done", "detail": {"merge": {"dev_head": MERGED}, "apply": {"applied": true}}})).unwrap();
    let (_, view) = call(
        &forks,
        &host,
        &alice(),
        "GET",
        &format!("/forks/{fork}"),
        json!({}),
    );
    assert_eq!(view["state"], "idle");
    assert!(
        host.actions()
            .contains(&"agent_fork.create:true".to_owned())
    );
    assert!(
        host.actions()
            .contains(&"agent_fork.submit:true".to_owned())
    );
    assert!(host.actions().contains(&"agent_fork.apply:true".to_owned()));
    // Records survive a reopen.
    let reopened = AgentForks::open(_t.path()).unwrap();
    assert_eq!(reopened.inner.lock().unwrap().forks.len(), 1);
}

#[test]
fn code_waits_for_the_owner_and_a_broken_compile_is_reverted() {
    let (_t, forks, host) = open();
    let fork = fork_with_changes(
        &forks,
        &host,
        &alice(),
        "r1",
        json!([{"path": "unity-project/Assets/Scripts/Glow.cs", "status": "A", "mode": 33188, "bytes": 300},
               {"path": "unity-project/Assets/Scripts/Glow.cs.meta", "status": "A", "mode": 33188, "bytes": 30}]),
    );
    // The dev head is known from the node before the merge.
    forks
        .dev_report(
            &host,
            &host.session(SID).unwrap(),
            &json!({"head": BASE, "applied": BASE, "compile": {"state": "ok", "errors": []}}),
        )
        .unwrap();
    let (_, sub) = call(
        &forks,
        &host,
        &alice(),
        "POST",
        &format!("/forks/{fork}/submit"),
        json!({"request_id": "s1", "message": "Glow script", "expected_head": HEAD}),
    );
    let sub_id = sub["submission"]["id"].as_str().unwrap().to_owned();
    let snapshot = ops_of(&poll(&forks, &host), &fork)[0].clone();
    report(&forks, &host, json!({"fork": fork, "op_id": snapshot["op_id"], "state": "done", "detail": {"head": HEAD}})).unwrap();
    let (_, subs) = call(
        &forks,
        &host,
        &owner(),
        "GET",
        "/submissions?state=awaiting_approval",
        json!({}),
    );
    assert_eq!(subs["submissions"][0]["id"], sub_id.as_str());
    assert_eq!(subs["submissions"][0]["classes"]["code"], 2);
    assert_eq!(subs["submissions"][0]["needs_approval"], true);
    // No merge op until approved.
    assert!(ops_of(&poll(&forks, &host), &fork).is_empty());
    // The author (a room principal) cannot approve; a wrong head is refused.
    let approve = format!("/submissions/{sub_id}/approve");
    let (status, _) = call(
        &forks,
        &host,
        &alice(),
        "POST",
        &approve,
        json!({"request_id": "a1", "expected_fork_head": HEAD}),
    );
    assert_eq!(status, 403);
    let (status, _) = call(
        &forks,
        &host,
        &owner(),
        "POST",
        &approve,
        json!({"request_id": "a1", "expected_fork_head": MERGED}),
    );
    assert_eq!(status, 409);
    let (status, approved) = call(
        &forks,
        &host,
        &owner(),
        "POST",
        &approve,
        json!({"request_id": "a2", "expected_fork_head": HEAD}),
    );
    assert_eq!(status, 202, "{approved}");
    assert_eq!(approved["submission"]["state"], "merging");
    let merge = ops_of(&poll(&forks, &host), &fork)[0].clone();
    assert_eq!(merge["merge"]["approved_by"], OWNER);
    assert_eq!(merge["merge"]["expected_dev_head"], BASE);
    report(&forks, &host, json!({"fork": fork, "op_id": merge["op_id"], "state": "done", "detail": {"merge": {"dev_head": MERGED}, "apply": {"applied": true}}})).unwrap();
    // The Editor recompiles with errors: the merge is reverted automatically.
    forks
        .dev_report(&host, &host.session(SID).unwrap(), &json!({"head": MERGED, "applied": MERGED, "compile": {"state": "errors", "errors": [{"file": "Assets/Scripts/Glow.cs", "line": 3, "message": "CS1002: ; expected"}]}}))
        .unwrap();
    let revert = ops_of(&poll(&forks, &host), &fork)[0].clone();
    assert_eq!(revert["kind"], "revert");
    assert_eq!(revert["revert"]["commit"], MERGED);
    assert_eq!(revert["revert"]["parent"], BASE);
    assert_eq!(revert["revert"]["expected_dev_head"], MERGED);
    assert_eq!(revert["revert"]["auto"], true);
    let (_, dev) = call(&forks, &host, &alice(), "GET", "/dev", json!({}));
    assert_eq!(dev["compile"]["state"], "errors");
    assert_eq!(dev["notices"][0]["kind"], "auto_revert");
    assert_eq!(dev["notices"][0]["author"]["principal"], "gj:jam:alice");
    assert_eq!(dev["notices"][0]["errors"][0]["line"], 3);
    assert_eq!(dev["last_revert"]["reverted"]["state"], "reverting");
    // A second error report does not revert twice.
    forks
        .dev_report(
            &host,
            &host.session(SID).unwrap(),
            &json!({"compile": {"state": "errors", "errors": []}}),
        )
        .unwrap();
    assert_eq!(ops_of(&poll(&forks, &host), &fork).len(), 1);
    report(&forks, &host, json!({"fork": fork, "op_id": revert["op_id"], "state": "done", "detail": {"merge": {"dev_head": REVERTED}, "apply": {"applied": true}}})).unwrap();
    let (_, subs) = call(&forks, &host, &alice(), "GET", "/submissions", json!({}));
    assert_eq!(subs["submissions"][0]["reverted"]["state"], "reverted");
    assert_eq!(subs["submissions"][0]["reverted"]["auto"], true);
    assert_eq!(
        subs["submissions"][0]["reverted"]["errors"][0]["message"],
        "CS1002: ; expected"
    );
    let (_, dev) = call(&forks, &host, &alice(), "GET", "/dev", json!({}));
    assert_eq!(dev["head"], REVERTED);
    assert!(
        host.actions()
            .contains(&"agent_fork.approve:true".to_owned())
    );
    assert_eq!(
        host.actions()
            .iter()
            .filter(|a| a.starts_with("agent_fork.revert"))
            .count(),
        2
    );
}

#[test]
fn checks_refuse_secrets_and_forbidden_paths() {
    let (_t, forks, host) = open();
    let fork = fork_with_changes(
        &forks,
        &host,
        &alice(),
        "r1",
        json!([{"path": "unity-project/Assets/Data/config.json", "status": "A", "bytes": 80},
               {"path": "unity-project/ProjectSettings/ProjectVersion.txt", "status": "M", "bytes": 80},
               {"path": "unity-project/Assets/big.bin", "status": "A", "bytes": 200 * 1024 * 1024}]),
    );
    let diff = "diff --git a/unity-project/Assets/Data/config.json b/unity-project/Assets/Data/config.json\n--- /dev/null\n+++ b/unity-project/Assets/Data/config.json\n@@ -0,0 +1 @@\n+{\"key\": \"xk_live_alice_key_0123456789abcdef\"}\n";
    forks
        .artifact(
            &host,
            NODE,
            7,
            &json!({"fork": fork, "head": HEAD, "kind": "diff", "body": diff}),
        )
        .unwrap();
    // The stored diff never holds the bearer Engine Box handed out.
    let stored =
        std::fs::read_to_string(forks.root.join("fork-diffs").join(format!("{fork}.diff")))
            .unwrap();
    assert!(!stored.contains(KEY_ALICE));
    let (_, sub) = call(
        &forks,
        &host,
        &alice(),
        "POST",
        &format!("/forks/{fork}/submit"),
        json!({"request_id": "s1", "message": "data", "expected_head": HEAD}),
    );
    let snapshot = ops_of(&poll(&forks, &host), &fork)[0].clone();
    report(&forks, &host, json!({"fork": fork, "op_id": snapshot["op_id"], "state": "done", "detail": {"head": HEAD, "findings": [{"rule": "aws_key", "path": "unity-project/Assets/Data/config.json", "line": 1}]}})).unwrap();
    let (_, subs) = call(&forks, &host, &alice(), "GET", "/submissions", json!({}));
    let s = &subs["submissions"][0];
    assert_eq!(s["id"], sub["submission"]["id"]);
    assert_eq!(s["state"], "rejected");
    let rules: Vec<&str> = s["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["rule"].as_str().unwrap())
        .collect();
    for rule in [
        "aws_key",
        "engine_version",
        "large_file_not_lfs",
        "known_secret",
    ] {
        assert!(rules.contains(&rule), "{rule} in {rules:?}");
    }
    assert!(!subs.to_string().contains("xk_live_"));
    assert_eq!(s["classes"]["refused"], 1);
    // The fork is idle again, and no merge op exists.
    assert!(ops_of(&poll(&forks, &host), &fork).is_empty());
    let (_, view) = call(
        &forks,
        &host,
        &alice(),
        "GET",
        &format!("/forks/{fork}"),
        json!({}),
    );
    assert_eq!(view["state"], "idle");
    assert!(
        host.actions()
            .contains(&"agent_fork.reject:true".to_owned())
    );
}

#[test]
fn people_see_only_their_forks_and_limits_hold() {
    let (_t, forks, host) = open();
    let (status, created) = create(&forks, &host, &alice(), "a1");
    assert_eq!(status, 202);
    let a1 = created["fork"]["id"].as_str().unwrap().to_owned();
    // Bob sees Alice's fork only as a claim; reading it is a 404.
    let (_, listed) = call(&forks, &host, &bob(), "GET", "/forks", json!({}));
    assert_eq!(listed["forks"][0]["title"], "Make the cards glow");
    assert_eq!(listed["forks"][0]["owner"]["name"], "alice name");
    assert!(listed["forks"][0].get("head").is_none());
    for path in [format!("/forks/{a1}"), format!("/forks/{a1}/diff")] {
        assert_eq!(call(&forks, &host, &bob(), "GET", &path, json!({})).0, 404);
    }
    for (path, body) in [
        (
            "submit",
            json!({"request_id": "x", "message": "m", "expected_head": HEAD}),
        ),
        ("rebase", json!({"request_id": "x"})),
        ("close", json!({})),
        ("runs", json!({"request_id": "x", "prompt": "p"})),
    ] {
        let (status, _) = call(
            &forks,
            &host,
            &bob(),
            "POST",
            &format!("/forks/{a1}/{path}"),
            body,
        );
        assert_eq!(status, 404, "{path}");
    }
    // Only the room owner lists everything in full.
    assert_eq!(
        call(&forks, &host, &bob(), "GET", "/forks?all=1", json!({})).0,
        403
    );
    let (_, all) = call(&forks, &host, &owner(), "GET", "/forks?all=1", json!({}));
    assert_eq!(all["forks"][0]["id"], a1.as_str());
    assert_eq!(
        call(&forks, &host, &owner(), "GET", "/forks?bogus=1", json!({})).0,
        400
    );
    // A viewer cannot start agents.
    let viewer = room("carol", principal::Role::Viewer);
    assert_eq!(create(&forks, &host, &viewer, "v1").0, 403);
    // Without a gateway connection: refused with a code.
    let (status, refused) = create(&forks, &host, &room("dave", principal::Role::Builder), "d1");
    assert_eq!(status, 409);
    assert_eq!(refused["code"], "gateway_required");
    // Three open forks per person.
    for r in ["a2", "a3"] {
        assert_eq!(create(&forks, &host, &alice(), r).0, 202);
    }
    let (status, refused) = create(&forks, &host, &alice(), "a4");
    assert_eq!(
        (status, refused["code"].as_str()),
        (409, Some("fork_limit"))
    );
    // Closing one frees a place.
    let (status, closed) = call(
        &forks,
        &host,
        &alice(),
        "POST",
        &format!("/forks/{a1}/close"),
        json!({}),
    );
    assert_eq!(status, 200, "{closed}");
    assert_eq!(closed["fork"]["state"], "closing");
    assert_eq!(create(&forks, &host, &alice(), "a4").0, 202);
    // Slots: at most three runs on the machine, then Bob goes before Alice's
    // fourth run (round robin by person).
    assert_eq!(create(&forks, &host, &bob(), "b1").0, 202);
    let row = poll(&forks, &host);
    let running: Vec<String> = row["forks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| {
            f["ops"]
                .as_array()
                .unwrap()
                .iter()
                .any(|o| o["kind"] == "run")
        })
        .map(|f| f["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(running.len(), 3);
    let inner = forks.inner.lock().unwrap();
    let bob_dispatched = inner
        .forks
        .values()
        .any(|f| f.owner.principal == "gj:jam:bob" && f.runs[0].state == RunState::Dispatched);
    assert!(bob_dispatched, "Bob's run is among the first three");
    drop(inner);
    // The closing fork is told to close.
    let closing = row["forks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == a1.as_str())
        .unwrap();
    assert_eq!(closing["desired"], "closed");
    report(&forks, &host, json!({"fork": a1, "state": "closed"})).unwrap();
    assert!(
        !poll(&forks, &host)["forks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["id"] == a1.as_str())
    );
    // A machine without isolated accounts runs only the room owner's agents.
    *host.caps.lock().unwrap() = Some(Caps {
        provider: "git".into(),
        slots: 1,
        isolation: "none".into(),
    });
    let (status, refused) = create(&forks, &host, &bob(), "b2");
    assert_eq!(
        (status, refused["code"].as_str()),
        (409, Some("isolation_required"))
    );
    assert_eq!(create(&forks, &host, &owner(), "o1").0, 202);
    *host.caps.lock().unwrap() = None;
    let (status, refused) = create(&forks, &host, &owner(), "o2");
    assert_eq!(
        (status, refused["code"].as_str()),
        (409, Some("forks_unsupported"))
    );
}

#[test]
fn nodes_are_fenced_runs_time_out_and_ended_sessions_fail_forks() {
    let (_t, forks, host) = open();
    let (_, created) = create(&forks, &host, &alice(), "r1");
    let fork = created["fork"]["id"].as_str().unwrap().to_owned();
    let row = poll(&forks, &host);
    let op = ops_of(&row, &fork)[0]["op_id"].clone();
    // Wrong epoch, generation or node: refused.
    assert!(
        forks
            .report(
                &host,
                NODE,
                8,
                &json!({"fork": fork, "generation": 1, "state": "idle"})
            )
            .is_err()
    );
    assert!(
        forks
            .report(
                &host,
                NODE,
                7,
                &json!({"fork": fork, "generation": 2, "state": "idle"})
            )
            .is_err()
    );
    assert!(
        forks
            .report(
                &host,
                "x",
                7,
                &json!({"fork": fork, "generation": 1, "state": "idle"})
            )
            .is_err()
    );
    assert!(
        report(
            &forks,
            &host,
            json!({"fork": fork, "op_id": "nope", "state": "done"})
        )
        .is_err()
    );
    report(
        &forks,
        &host,
        json!({"fork": fork, "op_id": op, "state": "running"}),
    )
    .unwrap();
    // The run outlives its 30 minutes: failed, and the node is told to cancel.
    {
        let mut inner = forks.inner.lock().unwrap();
        let f = inner.forks.get_mut(&fork).unwrap();
        f.runs[0].dispatched_at = Some(now() - RUN_SECONDS - RUN_GRACE_SECONDS - 1);
    }
    let row = poll(&forks, &host);
    let ops = ops_of(&row, &fork);
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0]["kind"], "cancel");
    assert!(
        !row.to_string().contains(KEY_ALICE),
        "no bearer after the run ended"
    );
    let (_, view) = call(
        &forks,
        &host,
        &alice(),
        "GET",
        &format!("/forks/{fork}"),
        json!({}),
    );
    assert_eq!(view["runs"][0]["state"], "failed");
    assert!(
        view["runs"][0]["error"]
            .as_str()
            .unwrap()
            .contains("30 minute")
    );
    // The session ends: the fork fails and leaves the poll.
    host.session.lock().unwrap().open = false;
    let (_, view) = call(
        &forks,
        &host,
        &alice(),
        "GET",
        &format!("/forks/{fork}"),
        json!({}),
    );
    assert_eq!(view["state"], "failed");
    assert!(poll(&forks, &host)["forks"].as_array().unwrap().is_empty());
}

#[test]
fn rebase_and_conflicts_keep_the_fork() {
    let (_t, forks, host) = open();
    let fork = fork_with_changes(
        &forks,
        &host,
        &alice(),
        "r1",
        json!([{"path": "unity-project/Assets/Scenes/Main.unity", "status": "M", "bytes": 10}]),
    );
    // Rebase needs a known dev head.
    assert_eq!(
        call(
            &forks,
            &host,
            &alice(),
            "POST",
            &format!("/forks/{fork}/rebase"),
            json!({"request_id": "rb"})
        )
        .0,
        409
    );
    forks
        .dev_report(
            &host,
            &host.session(SID).unwrap(),
            &json!({"head": MERGED, "applied": MERGED}),
        )
        .unwrap();
    let (_, sub) = call(
        &forks,
        &host,
        &alice(),
        "POST",
        &format!("/forks/{fork}/submit"),
        json!({"request_id": "s1", "message": "scene", "expected_head": HEAD}),
    );
    let snapshot = ops_of(&poll(&forks, &host), &fork)[0].clone();
    report(&forks, &host, json!({"fork": fork, "op_id": snapshot["op_id"], "state": "done", "detail": {"head": HEAD}})).unwrap();
    let merge = ops_of(&poll(&forks, &host), &fork)[0].clone();
    assert_eq!(merge["merge"]["expected_dev_head"], MERGED);
    // The dev head moved under the merge: retried against the new head.
    report(&forks, &host, json!({"fork": fork, "op_id": merge["op_id"], "state": "failed", "detail": {"merge": {"stale": true, "dev_head": REVERTED}}})).unwrap();
    let retry = ops_of(&poll(&forks, &host), &fork)[0].clone();
    assert_eq!(retry["merge"]["expected_dev_head"], REVERTED);
    report(&forks, &host, json!({"fork": fork, "op_id": retry["op_id"], "state": "conflict", "detail": {"merge": {"conflict_paths": ["unity-project/Assets/Scenes/Main.unity"]}}})).unwrap();
    let (_, subs) = call(&forks, &host, &alice(), "GET", "/submissions", json!({}));
    assert_eq!(subs["submissions"][0]["id"], sub["submission"]["id"]);
    assert_eq!(subs["submissions"][0]["state"], "conflict");
    assert_eq!(
        subs["submissions"][0]["conflict_paths"][0],
        "unity-project/Assets/Scenes/Main.unity"
    );
    // The fork is idle and can rebase onto the dev head.
    let (status, _) = call(
        &forks,
        &host,
        &alice(),
        "POST",
        &format!("/forks/{fork}/rebase"),
        json!({"request_id": "rb"}),
    );
    assert_eq!(status, 202);
    let rebase = ops_of(&poll(&forks, &host), &fork)[0].clone();
    assert_eq!(rebase["rebase"]["onto"], MERGED);
    report(&forks, &host, json!({"fork": fork, "op_id": rebase["op_id"], "state": "done", "detail": {"base": MERGED, "head": REVERTED}})).unwrap();
    let (_, view) = call(
        &forks,
        &host,
        &alice(),
        "GET",
        &format!("/forks/{fork}"),
        json!({}),
    );
    assert_eq!(view["base"], MERGED);
    assert_eq!(view["head"], REVERTED);
    // A run resumes the agent's native session.
    let (status, run) = call(
        &forks,
        &host,
        &alice(),
        "POST",
        &format!("/forks/{fork}/runs"),
        json!({"request_id": "r2", "prompt": "fix it", "resume": true}),
    );
    assert_eq!(status, 202, "{run}");
    let op = ops_of(&poll(&forks, &host), &fork)[0].clone();
    assert_eq!(op["run"]["resume_id"], "sess-1");
    assert_eq!(op["run"]["provider"], "claude");
}
