//! Member subscription runs (docs/MEMBER-SUBSCRIPTIONS.md): the guest gets
//! one member's access material on stdin, puts it only in the run's private
//! directory or environment, scrubs it from output and removes it; a
//! workspace holding any other credential refuses the run (no fallback); and
//! the queue keeps member runs and ordinary runs apart.
use enginebox_agents::{
    AgentService,
    member::{self, Delegated},
    native,
    queue::{HostLoad, MemberRun, QueueLimits},
    security,
    service::{AdmittedRun, LaunchOptions},
};
use enginebox_core::{Fleet, Host};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const RUN: &str = "0123456789abcdef0123456789abcdef";
const ALICE: &str = "usr_aaaaaaaaaaaaaaaaaaaaaaaa";
const BOB: &str = "usr_bbbbbbbbbbbbbbbbbbbbbbbb";
const ALICE_CRED: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const BOB_CRED: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const ALICE_ACCESS: &str = "alice-access-token-0123456789";
const ALICE_ID: &str = "alice-identity-token-0123456789";
const ALICE_CLAUDE: &str = "sk-ant-oat01-alice-access-0123456789";

fn codex_material() -> Delegated {
    Delegated::from_material(
        "codex",
        &json!({"kind":"native","content":{"auth_mode":"chatgptAuthTokens","OPENAI_API_KEY":null,
            "tokens":{"access_token":ALICE_ACCESS,"id_token":ALICE_ID,"account_id":"acct-alice","refresh_token":""},
            "last_refresh":"2026-11-07T00:00:00Z"},"expires_at":4102444800.0}),
    )
    .unwrap()
}
fn claude_material() -> Delegated {
    Delegated::from_material(
        "claude",
        &json!({"kind":"environment","content":{"CLAUDE_CODE_OAUTH_TOKEN":ALICE_CLAUDE},"expires_at":4102444800.0}),
    )
    .unwrap()
}

#[test]
fn material_is_access_only_never_a_refresh_token() {
    let mut refresh = codex_material();
    refresh.content["tokens"]["refresh_token"] = json!("rt-0123456789");
    assert!(refresh.validate().is_err());
    let mut extra = claude_material();
    extra.content["CLAUDE_CODE_OAUTH_REFRESH_TOKEN"] = json!("rt-0123456789");
    assert!(extra.validate().is_err());
    let mut key = codex_material();
    key.content["OPENAI_API_KEY"] = json!("sk-0123456789");
    assert!(key.validate().is_err());
    let mut other = claude_material();
    other.provider = "cursor".into();
    assert!(other.validate().is_err());
    // Debug never prints material.
    let shown = format!("{:?}", codex_material());
    assert!(!shown.contains(ALICE_ACCESS) && !shown.contains(ALICE_ID));
}

#[cfg(unix)]
struct Box_ {
    _t: tempfile::TempDir,
    home: PathBuf,
    workspace: PathBuf,
}
#[cfg(unix)]
fn guest(provider: &str) -> Box_ {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    let root = t.path().canonicalize().unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let home = root.join("home");
    let workspace = root.join("workspace");
    security::private_dir(&home).unwrap();
    security::private_dir(&workspace).unwrap();
    let (name, version) = match provider {
        "codex" => ("codex", "codex-cli 0.163.0"),
        _ => ("claude", "2.1.310 (Claude Code)"),
    };
    security::safe_path(&home, &format!(".local/bin/{name}"), true).unwrap();
    let exe = home.join(format!(".local/bin/{name}"));
    // Records argv, what the CLI can authenticate with, and prints both
    // secrets so the helper must scrub them.
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = --version ]; then echo '{version}'; exit 0; fi\ncat >/dev/null\n\
         {{ echo \"argv=$*\"; echo \"codex_home=$CODEX_HOME\"; echo \"oauth=$CLAUDE_CODE_OAUTH_TOKEN\"; \
         if [ -n \"$CODEX_HOME\" ]; then echo \"auth=$(cat \"$CODEX_HOME/auth.json\")\"; fi; }} >>\"$HOME/.seen\"\n\
         echo \"leak $CLAUDE_CODE_OAUTH_TOKEN\"\n\
         if [ -n \"$CODEX_HOME\" ]; then cat \"$CODEX_HOME/auth.json\"; fi\n"
    );
    std::fs::write(&exe, script).unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
    Box_ {
        _t: t,
        home,
        workspace,
    }
}
#[cfg(unix)]
fn envelope(provider: &str, credential: Delegated) -> member::Envelope {
    member::parse(json!({
        "mode":"run",
        "input":{"provider":provider,"prompt":"build it"},
        "credential":credential,
    }))
    .unwrap()
}
/// No file under `dir` contains `needle` (the seen log aside).
#[cfg(unix)]
fn nowhere(dir: &Path, needle: &str) {
    for entry in walk(dir) {
        if entry.file_name().is_some_and(|n| n == ".seen") {
            continue;
        }
        if let Ok(bytes) = std::fs::read(&entry) {
            assert!(
                !String::from_utf8_lossy(&bytes).contains(needle),
                "{} still holds the member's material",
                entry.display()
            );
        }
    }
}
#[cfg(unix)]
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let path = e.path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}

#[cfg(unix)]
#[test]
fn a_codex_member_run_reads_only_its_own_private_codex_home_and_leaves_nothing() {
    let g = guest("codex");
    let out = native::execute_member(
        &envelope("codex", codex_material()),
        &g.home,
        &g.workspace,
        RUN,
    )
    .unwrap();
    assert_eq!(out.exit_code, 0);
    let seen = std::fs::read_to_string(g.home.join(".seen")).unwrap();
    let run_dir = enginebox_agents::profile::run_dir(&g.home, RUN).unwrap();
    assert!(
        seen.contains(&format!(
            "codex_home={}",
            run_dir.join("codex-home").display()
        )),
        "{seen}"
    );
    assert!(
        seen.contains(ALICE_ACCESS),
        "the CLI saw the member's access"
    );
    assert!(seen.contains("\"refresh_token\":\"\""), "{seen}");
    // Never in argv, never in output.
    let argv = seen.lines().find(|l| l.starts_with("argv=")).unwrap();
    assert!(!argv.contains(ALICE_ACCESS) && !argv.contains(ALICE_ID));
    assert!(!out.output.contains(ALICE_ACCESS) && !out.output.contains(ALICE_ID));
    assert!(out.output.contains(native::REDACTED));
    // Removed at the end; the workspace's own ~/.codex never got it.
    assert!(!run_dir.exists());
    assert!(!g.home.join(".codex/auth.json").exists());
    nowhere(&g.home, ALICE_ACCESS);
    nowhere(&g.workspace, ALICE_ACCESS);
}

#[cfg(unix)]
#[test]
fn a_claude_member_run_gets_its_token_only_in_its_environment() {
    let g = guest("claude");
    let out = native::execute_member(
        &envelope("claude", claude_material()),
        &g.home,
        &g.workspace,
        RUN,
    )
    .unwrap();
    assert_eq!(out.exit_code, 0);
    let seen = std::fs::read_to_string(g.home.join(".seen")).unwrap();
    assert!(seen.contains(&format!("oauth={ALICE_CLAUDE}")), "{seen}");
    let argv = seen.lines().find(|l| l.starts_with("argv=")).unwrap();
    assert!(!argv.contains(ALICE_CLAUDE));
    assert!(!out.output.contains(ALICE_CLAUDE), "{}", out.output);
    nowhere(&g.home, ALICE_CLAUDE);
    nowhere(&g.workspace, ALICE_CLAUDE);
    // A later ordinary run in the same workspace does not inherit it.
    std::fs::remove_file(g.home.join(".seen")).unwrap();
    let plain = native::RunRequest::parse(json!({"provider":"claude","prompt":"x"})).unwrap();
    native::execute(&plain, &g.home, &g.workspace).unwrap();
    let seen = std::fs::read_to_string(g.home.join(".seen")).unwrap();
    assert!(seen.contains("oauth=\n"), "{seen}");
}

#[cfg(unix)]
#[test]
fn a_member_run_never_falls_back_to_a_workspace_credential() {
    // Each of these would let the CLI authenticate as someone else (the
    // controller's accounts or a settings override): the run is refused.
    let cases: Vec<(&str, &str, Value)> = vec![
        (
            "codex",
            ".codex/auth.json",
            json!({"tokens":{"access_token":"owner-access-0123456789"}}),
        ),
        (
            "claude",
            ".claude/.credentials.json",
            json!({"claudeAiOauth":{"accessToken":"owner-0123456789"}}),
        ),
        (
            "claude",
            ".config/enginebox/controller-token.json",
            json!({"kind":"setup_token","value":"owner-0123456789"}),
        ),
        (
            "claude",
            ".config/enginebox/agents/claude.json",
            json!({"generation":"x"}),
        ),
        (
            "claude",
            ".claude/settings.json",
            json!({"env":{"CLAUDE_CODE_OAUTH_TOKEN":"owner-0123456789"}}),
        ),
        (
            "claude",
            ".claude/settings.json",
            json!({"env":{"ANTHROPIC_BASE_URL":"https://attacker.example"}}),
        ),
        (
            "claude",
            ".claude/settings.json",
            json!({"apiKeyHelper":"/bin/echo owner"}),
        ),
    ];
    for (provider, file, content) in cases {
        let g = guest(provider);
        security::write_json(&g.home, file, &content).unwrap();
        let credential = if provider == "codex" {
            codex_material()
        } else {
            claude_material()
        };
        let refused =
            native::execute_member(&envelope(provider, credential), &g.home, &g.workspace, RUN);
        assert!(refused.is_err(), "{file} {content}");
        assert!(
            !g.home.join(".seen").exists(),
            "the CLI never started ({file})"
        );
    }
    // Material for another provider than the request is refused too.
    let g = guest("claude");
    let wrong = member::Envelope {
        mode: "run".into(),
        input: json!({"provider":"claude","prompt":"x"}),
        credential: codex_material(),
    };
    assert!(native::execute_member(&wrong, &g.home, &g.workspace, RUN).is_err());
}

// ------------------------------------------------------------ the queue

fn root() -> (tempfile::TempDir, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().canonicalize().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    // Deliberately no controller account at all: member runs never need one.
    (t, path)
}
fn service(root: &Path) -> AgentService {
    let host = Host {
        id: "h1".into(),
        target: "h1.invalid".into(),
        max_agent_runs: Some(4),
        ..Default::default()
    };
    let fleet = Fleet::from_hosts(vec![host], root).unwrap();
    let service =
        AgentService::with_limits(root, fleet, BTreeMap::new(), QueueLimits::default()).unwrap();
    service.set_measure_hook(Arc::new(|_| {
        Ok(HostLoad {
            cpu_total: 64,
            reserve_cpu: 2,
            memory_available_bytes: 256 << 30,
            reserve_memory_bytes: 4 << 30,
        })
    }));
    service
}
fn body(provider: &str) -> Value {
    json!({"provider":provider,"prompt":"make the jump higher"})
}
fn member(who: &str, credential: &str) -> LaunchOptions {
    LaunchOptions {
        member: Some(MemberRun {
            member: who.into(),
            credential_id: credential.into(),
        }),
        ..Default::default()
    }
}
fn wait_for(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn each_member_run_gets_only_its_own_members_material() {
    let (_t, root) = root();
    let service = service(&root);
    service
        .pools()
        .put(
            json!({"name":"jam","project":"qoh","workspaces":[{"host_id":"h1","name":"ebox-m1"},{"host_id":"h1","name":"ebox-m2"}]}),
            &|h| h == "h1",
        )
        .unwrap();
    service
        .pools()
        .put(
            json!({"name":"ordinary","project":"qoh","workspaces":[{"host_id":"h1","name":"ebox-o1"}]}),
            &|h| h == "h1",
        )
        .unwrap();
    // Not yet a member-runs pool: member runs are refused there.
    assert!(
        service
            .launch_pool("jam", body("claude"), "owner", member(ALICE, ALICE_CRED))
            .is_err()
    );
    service.pools().set_member_runs("jam", true).unwrap();
    // A member-runs pool shares no workspace with another pool.
    assert!(
        service
            .pools()
            .put(
                json!({"name":"sneaky","project":"qoh","workspaces":[{"host_id":"h1","name":"ebox-m1"}]}),
                &|h| h == "h1",
            )
            .is_err()
    );
    // Ordinary runs never land on a member-runs pool, and member runs never
    // on an ordinary pool or a named workspace.
    assert!(
        service
            .launch_pool("jam", body("claude"), "owner", LaunchOptions::default())
            .is_err()
    );
    assert!(
        service
            .launch_pool(
                "ordinary",
                body("claude"),
                "owner",
                member(ALICE, ALICE_CRED)
            )
            .is_err()
    );
    assert!(
        service
            .launch_with(
                "h1",
                "ebox-o1",
                body("claude"),
                "owner",
                member(ALICE, ALICE_CRED)
            )
            .is_err()
    );
    // The material reader answers per member and records who asked.
    let asked: Arc<Mutex<Vec<MemberRun>>> = Arc::default();
    let log = asked.clone();
    service.set_member_material(Arc::new(move |run, provider| {
        log.lock().unwrap().push(run.clone());
        anyhow::ensure!(provider == "claude");
        let token = match (run.member.as_str(), run.credential_id.as_str()) {
            (ALICE, ALICE_CRED) => "sk-ant-oat01-alice-0123456789",
            (BOB, BOB_CRED) => "sk-ant-oat01-bob-0123456789",
            _ => anyhow::bail!("not this member's credential"),
        };
        Delegated::from_material(
            "claude",
            &json!({"kind":"environment","content":{"CLAUDE_CODE_OAUTH_TOKEN":token}}),
        )
    }));
    let delivered: Arc<Mutex<BTreeMap<String, String>>> = Arc::default();
    let sink = delivered.clone();
    service.set_run_executor(Arc::new(move |run: &AdmittedRun| {
        let token = run
            .member_credential
            .as_ref()
            .map(|c| {
                c.content["CLAUDE_CODE_OAUTH_TOKEN"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .unwrap_or_else(|| "none".into());
        sink.lock().unwrap().insert(run.id.clone(), token);
        Ok(json!({"exit_code":0,"output":"{}","timed_out":false,"output_truncated":false}))
    }));
    let alice = service
        .launch_pool(
            "jam",
            body("claude"),
            "service-token:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            member(ALICE, ALICE_CRED),
        )
        .unwrap()
        .0["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let bob = service
        .launch_pool(
            "jam",
            body("claude"),
            "service-token:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            member(BOB, BOB_CRED),
        )
        .unwrap()
        .0["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // Bob's run naming Alice's credential gets nothing: the reader refuses
    // and the run fails, never falling back to anything.
    let crossed = service
        .launch_pool(
            "jam",
            body("claude"),
            "service-token:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            member(BOB, ALICE_CRED),
        )
        .unwrap()
        .0["id"]
        .as_str()
        .unwrap()
        .to_owned();
    wait_for("runs to finish", || {
        [&alice, &bob, &crossed].iter().all(|id| {
            let s = service.get_pool_run("jam", id).unwrap()["status"]
                .as_str()
                .unwrap()
                .to_owned();
            s == "succeeded" || s == "failed"
        })
    });
    let delivered = delivered.lock().unwrap();
    assert_eq!(delivered[&alice], "sk-ant-oat01-alice-0123456789");
    assert_eq!(delivered[&bob], "sk-ant-oat01-bob-0123456789");
    assert!(
        !delivered.contains_key(&crossed),
        "the crossed run never started"
    );
    assert_eq!(
        service.get_pool_run("jam", &crossed).unwrap()["status"],
        "failed"
    );
    // Views and ownership: each member sees only their own.
    assert_eq!(
        service.get_member_run("qoh", ALICE, &alice).unwrap()["credential_id"],
        ALICE_CRED
    );
    assert!(service.get_member_run("qoh", BOB, &alice).is_err());
    assert!(service.get_member_run("other", ALICE, &alice).is_err());
    assert!(service.cancel_member_run("qoh", BOB, &alice).is_err());
    let listed = service.list_member_runs("qoh", ALICE);
    let ids: Vec<&str> = listed["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![alice.as_str()]);
    // Member runs carry no secret in their queue or detail files.
    for dir in ["agent-run-queue", "agent-run-details", "agent-runs"] {
        for entry in std::fs::read_dir(root.join(dir))
            .into_iter()
            .flatten()
            .flatten()
        {
            let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
            assert!(!text.contains("sk-ant-oat01"), "{}", entry.path().display());
        }
    }
}

#[test]
fn a_member_run_without_a_reader_fails_rather_than_using_anything_else() {
    let (_t, root) = root();
    let service = service(&root);
    service
        .pools()
        .put(
            json!({"name":"jam","project":"qoh","workspaces":[{"host_id":"h1","name":"ebox-m1"}]}),
            &|h| h == "h1",
        )
        .unwrap();
    service.pools().set_member_runs("jam", true).unwrap();
    let started: Arc<Mutex<usize>> = Arc::default();
    let count = started.clone();
    service.set_run_executor(Arc::new(move |_| {
        *count.lock().unwrap() += 1;
        Ok(json!({"exit_code":0,"output":"{}","timed_out":false,"output_truncated":false}))
    }));
    let id = service
        .launch_pool("jam", body("codex"), "owner", member(ALICE, ALICE_CRED))
        .unwrap()
        .0["id"]
        .as_str()
        .unwrap()
        .to_owned();
    wait_for("the run to fail", || {
        service.get_pool_run("jam", &id).unwrap()["status"] == "failed"
    });
    assert_eq!(*started.lock().unwrap(), 0);
}
