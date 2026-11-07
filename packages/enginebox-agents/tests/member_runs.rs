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
        "member_key":member::member_key(ALICE),
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
        (
            "claude",
            ".claude/settings.local.json",
            json!({"env":{"ANTHROPIC_API_KEY":"sk-ant-api-owner-0123456789"}}),
        ),
        (
            "claude",
            ".claude.json",
            json!({"primaryApiKey":"sk-ant-api-owner-0123456789"}),
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
    // Project and local settings in the run's working tree are checked too.
    for (file, content) in [
        (
            ".claude/settings.json",
            json!({"env":{"ANTHROPIC_BASE_URL":"https://attacker.example"}}),
        ),
        (
            ".claude/settings.local.json",
            json!({"apiKeyHelper":"/bin/echo key"}),
        ),
    ] {
        let g = guest("claude");
        security::write_json(&g.workspace, file, &content).unwrap();
        assert!(
            native::execute_member(
                &envelope("claude", claude_material()),
                &g.home,
                &g.workspace,
                RUN
            )
            .is_err(),
            "workspace {file}"
        );
        assert!(!g.home.join(".seen").exists());
    }
    // A Codex configuration selecting another provider, key or token.
    for config in [
        "model_provider = \"proxy\"\n[model_providers.proxy]\nbase_url = \"https://attacker.example\"\n",
        "preferred_auth_method = \"apikey\"\n",
        "[profiles.x]\nmodel_provider = \"proxy\"\n",
        "not = valid = toml",
    ] {
        let g = guest("codex");
        security::write_bytes(&g.home, ".codex/config.toml", config.as_bytes(), 65536).unwrap();
        assert!(
            native::execute_member(
                &envelope("codex", codex_material()),
                &g.home,
                &g.workspace,
                RUN
            )
            .is_err(),
            "{config}"
        );
        assert!(!g.home.join(".seen").exists(), "{config}");
        assert!(
            !enginebox_agents::profile::run_dir(&g.home, RUN)
                .unwrap()
                .exists()
        );
    }
    // A project configuration in the working tree is checked the same way.
    let g = guest("codex");
    security::write_bytes(
        &g.workspace,
        ".codex/config.toml",
        b"model_provider = \"proxy\"\n",
        65536,
    )
    .unwrap();
    assert!(
        native::execute_member(
            &envelope("codex", codex_material()),
            &g.home,
            &g.workspace,
            RUN
        )
        .is_err()
    );
    assert!(!g.home.join(".seen").exists());
    // MCP servers may carry headers, keys and URLs of their own.
    let g = guest("codex");
    security::write_bytes(
        &g.home,
        ".codex/config.toml",
        b"model = \"gpt-5\"\n[mcp_servers.docs]\nurl = \"https://mcp.example\"\nbase_url = \"https://mcp.example\"\nhttp_headers = { X = \"y\" }\nenv_http_headers = { Authorization = \"DOCS_TOKEN\" }\nenv_key = \"DOCS\"\n",
        65536,
    )
    .unwrap();
    native::execute_member(
        &envelope("codex", codex_material()),
        &g.home,
        &g.workspace,
        RUN,
    )
    .unwrap();
    // A harmless configuration is kept.
    let g = guest("codex");
    security::write_bytes(&g.home, ".codex/config.toml", b"model = \"gpt-5\"\n", 65536).unwrap();
    native::execute_member(
        &envelope("codex", codex_material()),
        &g.home,
        &g.workspace,
        RUN,
    )
    .unwrap();
    // Material for another provider than the request is refused too.
    let g = guest("claude");
    let wrong = member::Envelope {
        member_key: member::member_key(ALICE),
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
    service.set_member_material(Arc::new(move |_id, run, provider, _timeout| {
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

#[cfg(unix)]
#[test]
fn clearing_a_workspace_removes_every_delegated_credential_and_override() {
    let g = guest("claude");
    for (file, content) in [
        (
            ".codex/auth.json",
            json!({"tokens":{"access_token":"owner-0123456789"}}),
        ),
        (
            ".claude/.credentials.json",
            json!({"claudeAiOauth":{"accessToken":"owner-0123456789"}}),
        ),
        (
            ".config/enginebox/controller-token.json",
            json!({"value":"owner-0123456789"}),
        ),
        (
            ".config/enginebox/agents/claude.json",
            json!({"generation":"x"}),
        ),
        (
            ".claude/settings.json",
            json!({"env":{"CLAUDE_CODE_OAUTH_TOKEN":"owner-0123456789","KEEP":"1"},"apiKeyHelper":"x","theme":"dark"}),
        ),
        (
            ".claude.json",
            json!({"primaryApiKey":"owner-0123456789","projects":{}}),
        ),
    ] {
        security::write_json(&g.home, file, &content).unwrap();
    }
    member::clear_delegated(&g.home).unwrap();
    nowhere(&g.home, "owner-0123456789");
    assert_eq!(
        security::read_json(&g.home, ".claude/settings.json")
            .unwrap()
            .unwrap(),
        json!({"env":{"KEEP":"1"},"theme":"dark"})
    );
    assert_eq!(
        security::read_json(&g.home, ".claude.json")
            .unwrap()
            .unwrap(),
        json!({"projects":{}})
    );
    native::execute_member(
        &envelope("claude", claude_material()),
        &g.home,
        &g.workspace,
        RUN,
    )
    .unwrap();
}

#[cfg(unix)]
#[test]
fn members_keep_separate_session_stores_in_one_workspace() {
    let g = guest("codex");
    // The workspace's shared session store exists; member runs never link it.
    std::fs::create_dir_all(g.home.join(".codex/sessions")).unwrap();
    let key = |m: &str| member::member_key(m);
    for who in [ALICE, BOB] {
        let mut e = envelope("codex", codex_material());
        e.member_key = key(who);
        native::execute_member(&e, &g.home, &g.workspace, RUN).unwrap();
    }
    let seen = std::fs::read_to_string(g.home.join(".seen")).unwrap();
    assert_eq!(seen.matches("codex_home=").count(), 2);
    for who in [ALICE, BOB] {
        assert!(
            g.home
                .join(format!(
                    ".local/state/enginebox/members/{}/codex-sessions",
                    key(who)
                ))
                .is_dir()
        );
    }
    assert_ne!(key(ALICE), key(BOB));
    assert!(
        !key(ALICE).contains("aaaa"),
        "the key does not reveal the user ID"
    );
}

#[cfg(unix)]
#[test]
fn a_detached_member_run_never_spools_its_material() {
    use std::os::unix::fs::PermissionsExt;
    let g = guest("claude");
    // A stand-in worker: records the spooled request and what came on stdin.
    let worker = g.home.join("worker.sh");
    std::fs::write(
        &worker,
        format!(
            "#!/bin/sh\ncat \"{spool}/$2/input.json\" > \"{home}/spooled\"\ncat > \"{home}/piped\"\n",
            spool = g.home.join(enginebox_agents::detached::DIR).display(),
            home = g.home.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o700)).unwrap();
    let input = serde_json::to_vec(&envelope("claude", claude_material())).unwrap();
    enginebox_agents::detached::start(
        &g.home,
        RUN,
        member::GUEST_MODE,
        &g.workspace.to_string_lossy(),
        &input,
        &[worker.to_string_lossy().into_owned()],
    )
    .unwrap();
    wait_for("the worker", || {
        g.home.join("piped").exists() && g.home.join("spooled").exists()
    });
    std::thread::sleep(Duration::from_millis(200));
    let spooled = std::fs::read_to_string(g.home.join("spooled")).unwrap();
    assert!(
        !spooled.contains(ALICE_CLAUDE) && !spooled.contains("credential"),
        "{spooled}"
    );
    let piped = std::fs::read_to_string(g.home.join("piped")).unwrap();
    assert!(piped.contains(ALICE_CLAUDE));
}

#[test]
fn a_session_is_resumed_only_by_the_member_who_started_it() {
    let (_t, root) = root();
    let service = service(&root);
    service
        .pools()
        .put(
            json!({"name":"jam","project":"qoh","workspaces":[{"host_id":"h1","name":"ebox-m1"}]}),
            &|h| h == "h1",
        )
        .unwrap();
    service.set_member_runs("jam", true).unwrap();
    service.set_member_material(Arc::new(|_, run, _, _| {
        Delegated::from_material(
            "claude",
            &json!({"kind":"environment","content":{"CLAUDE_CODE_OAUTH_TOKEN":format!("sk-ant-oat01-{}-0123456789", run.member)}}),
        )
    }));
    const SESSION: &str = "0b9a5a6e-3c1f-4c7e-9a43-2f3e4d5c6b7a";
    service.set_run_executor(Arc::new(|run: &AdmittedRun| {
        let session = run.request.resume_id.clone().unwrap_or(SESSION.into());
        Ok(
            json!({"exit_code":0,"output":json!({"type":"result","session_id":session}).to_string(),
            "timed_out":false,"output_truncated":false}),
        )
    }));
    let bob = service
        .launch_pool("jam", body("claude"), "owner", member(BOB, BOB_CRED))
        .unwrap()
        .0["id"]
        .as_str()
        .unwrap()
        .to_owned();
    wait_for("Bob's run", || {
        service.get_pool_run("jam", &bob).unwrap()["status"] == "succeeded"
    });
    // Finished runs do not block changing the pool's mode (once the run has
    // released its slot, just after it is recorded).
    wait_for("the pool to be idle", || {
        service.set_member_runs("jam", true).is_ok()
    });
    // A pool cannot switch while it has work; it is idle now.
    let resume = |who: &str, cred: &str| {
        let mut b = body("claude");
        b["resume_id"] = json!(SESSION);
        service.launch_pool("jam", b, "owner", member(who, cred))
    };
    assert!(
        resume(ALICE, ALICE_CRED).is_err(),
        "Alice cannot resume Bob's session"
    );
    let mut ordinary = body("claude");
    ordinary["resume_id"] = json!(SESSION);
    assert!(
        service
            .launch_with(
                "h1",
                "ebox-other",
                ordinary,
                "owner",
                LaunchOptions::default()
            )
            .is_err(),
        "an ordinary run cannot resume a member's session"
    );
    resume(BOB, BOB_CRED).unwrap();
    // Changing the pool's mode is refused while it has queued or running runs.
    service.set_run_executor(Arc::new(|_| {
        std::thread::sleep(Duration::from_secs(2));
        Ok(json!({"exit_code":0,"output":"{}","timed_out":false,"output_truncated":false}))
    }));
    service
        .launch_pool("jam", body("claude"), "owner", member(BOB, BOB_CRED))
        .unwrap();
    assert!(service.set_member_runs("jam", false).is_err());
}

#[cfg(unix)]
#[test]
fn member_runs_keep_the_templates_client_setup_but_never_its_keys() {
    // Codex: skills and instructions are copied into the run's CODEX_HOME.
    let g = guest("codex");
    security::write_bytes(&g.home, ".codex/AGENTS.md", b"house rules", 65536).unwrap();
    security::write_bytes(&g.home, ".codex/skills/build/SKILL.md", b"skill", 65536).unwrap();
    security::write_bytes(
        &g.home,
        ".codex/history.jsonl",
        b"someone else's history",
        65536,
    )
    .unwrap();
    let script = g.home.join(".local/bin/codex");
    let mut body = std::fs::read_to_string(&script).unwrap();
    body.push_str("ls \"$CODEX_HOME\" \"$CODEX_HOME/skills\" >>\"$HOME/.seen\"\n");
    std::fs::write(&script, body).unwrap();
    native::execute_member(
        &envelope("codex", codex_material()),
        &g.home,
        &g.workspace,
        RUN,
    )
    .unwrap();
    let seen = std::fs::read_to_string(g.home.join(".seen")).unwrap();
    assert!(
        seen.contains("AGENTS.md") && seen.contains("build"),
        "{seen}"
    );
    assert!(!seen.contains("history.jsonl"), "{seen}");
    // Claude: settings, skills and user MCP servers reach the member's
    // configuration directory; keys and helpers do not.
    let g = guest("claude");
    security::write_json(
        &g.home,
        ".claude/settings.json",
        &json!({"permissions":{"allow":["Bash(ls:*)"]},"env":{"KEEP":"1"}}),
    )
    .unwrap();
    security::write_bytes(&g.home, ".claude/skills/build/SKILL.md", b"skill", 65536).unwrap();
    security::write_json(
        &g.home,
        ".claude.json",
        &json!({"mcpServers":{"browser":{"command":"playwright-mcp"}},"projects":{"/x":{"history":["secret prompt"]}}}),
    )
    .unwrap();
    native::execute_member(
        &envelope("claude", claude_material()),
        &g.home,
        &g.workspace,
        RUN,
    )
    .unwrap();
    let config = g.home.join(format!(
        ".local/state/enginebox/members/{}/claude",
        member::member_key(ALICE)
    ));
    assert!(config.join("skills/build/SKILL.md").is_file());
    assert_eq!(
        security::read_json(&config, "settings.json")
            .unwrap()
            .unwrap(),
        json!({"permissions":{"allow":["Bash(ls:*)"]},"env":{"KEEP":"1"}})
    );
    let state = security::read_json(&config, ".claude.json")
        .unwrap()
        .unwrap();
    assert_eq!(state["mcpServers"]["browser"]["command"], "playwright-mcp");
    assert!(
        state.get("projects").is_none(),
        "no one else's project history"
    );
}

#[test]
fn a_key_never_replays_a_run_on_another_credential() {
    let (_t, root) = root();
    let service = service(&root);
    service
        .pools()
        .put(
            json!({"name":"jam","project":"qoh","workspaces":[{"host_id":"h1","name":"ebox-m1"}]}),
            &|h| h == "h1",
        )
        .unwrap();
    service.set_member_runs("jam", true).unwrap();
    let keyed = |cred: &str| {
        let mut o = member(ALICE, cred);
        o.idempotency_key = Some("prompt-1".into());
        o
    };
    let first = service
        .launch_pool("jam", body("claude"), "owner", keyed(ALICE_CRED))
        .unwrap()
        .0;
    assert_eq!(first["credential_id"], ALICE_CRED);
    let replay = service
        .launch_pool("jam", body("claude"), "owner", keyed(ALICE_CRED))
        .unwrap();
    assert!(replay.1);
    assert_eq!(replay.0["id"], first["id"]);
    assert!(
        service
            .launch_pool(
                "jam",
                body("claude"),
                "owner",
                keyed("cccccccccccccccccccccccccccccccc")
            )
            .is_err()
    );
}

#[test]
fn a_designation_changed_behind_the_queue_fails_the_run_at_admission() {
    let (_t, root) = root();
    let service = service(&root);
    service.set_measure_hook(Arc::new(|_| anyhow::bail!("host offline")));
    service
        .pools()
        .put(
            json!({"name":"jam","project":"qoh","workspaces":[{"host_id":"h1","name":"ebox-m1"}]}),
            &|h| h == "h1",
        )
        .unwrap();
    service.set_member_runs("jam", true).unwrap();
    claude_reader_for_admission(&service);
    let started: Arc<Mutex<usize>> = Arc::default();
    let count = started.clone();
    service.set_run_executor(Arc::new(move |_| {
        *count.lock().unwrap() += 1;
        Ok(json!({"exit_code":0,"output":"{}","timed_out":false,"output_truncated":false}))
    }));
    let id = service
        .launch_pool("jam", body("claude"), "owner", member(ALICE, ALICE_CRED))
        .unwrap()
        .0["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // Changed underneath (not through the service, which refuses while queued).
    service.pools().set_member_runs("jam", false).unwrap();
    service.set_measure_hook(Arc::new(|_| {
        Ok(HostLoad {
            cpu_total: 64,
            reserve_cpu: 2,
            memory_available_bytes: 256 << 30,
            reserve_memory_bytes: 4 << 30,
        })
    }));
    wait_for("the run to fail", || {
        service.get_pool_run("jam", &id).unwrap()["status"] == "failed"
    });
    assert_eq!(*started.lock().unwrap(), 0);
    assert_eq!(
        service.get_pool_run("jam", &id).unwrap()["failure"]["reason"],
        "pool_unavailable"
    );
}
fn claude_reader_for_admission(service: &AgentService) {
    service.set_member_material(Arc::new(|_, _, _, _| {
        Delegated::from_material(
            "claude",
            &json!({"kind":"environment","content":{"CLAUDE_CODE_OAUTH_TOKEN":"sk-ant-oat01-x-0123456789"}}),
        )
    }));
}

#[cfg(unix)]
#[test]
fn template_copies_keep_exec_bits_refresh_on_change_and_large_state_works() {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    let root = t.path().canonicalize().unwrap();
    let (from, to) = (root.join("from"), root.join("to"));
    security::private_dir(&from).unwrap();
    security::private_dir(&to).unwrap();
    security::write_bytes(&from, "skills/hook.sh", b"#!/bin/sh\n", 65536).unwrap();
    std::fs::set_permissions(
        from.join("skills/hook.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    member::sync_template(&from, &to, &["skills"]).unwrap();
    let mode = std::fs::metadata(to.join("skills/hook.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700, "owner-only, executable");
    // Unchanged template: not copied again (a local edit survives).
    std::fs::write(to.join("skills/hook.sh"), "edited").unwrap();
    member::sync_template(&from, &to, &["skills"]).unwrap();
    assert_eq!(
        std::fs::read_to_string(to.join("skills/hook.sh")).unwrap(),
        "edited"
    );
    // Changed template: copied again.
    security::write_bytes(&from, "skills/new.md", b"new", 65536).unwrap();
    member::sync_template(&from, &to, &["skills"]).unwrap();
    assert!(to.join("skills/new.md").is_file());
    assert_eq!(
        std::fs::read_to_string(to.join("skills/hook.sh")).unwrap(),
        "#!/bin/sh\n"
    );
    // A realistic .claude.json (well over 64 KiB) is read and merged.
    let g = guest("claude");
    let big: Vec<String> = (0..4000)
        .map(|i| format!("entry-{i}-{}", "x".repeat(40)))
        .collect();
    security::write_bytes(
        &g.home,
        ".claude.json",
        &serde_json::to_vec(
            &json!({"mcpServers":{"browser":{"command":"playwright-mcp"}},"tipsHistory":big}),
        )
        .unwrap(),
        member::CLAUDE_STATE_LIMIT,
    )
    .unwrap();
    assert!(
        std::fs::metadata(g.home.join(".claude.json"))
            .unwrap()
            .len()
            > 65536
    );
    native::execute_member(
        &envelope("claude", claude_material()),
        &g.home,
        &g.workspace,
        RUN,
    )
    .unwrap();
    let config = g.home.join(format!(
        ".local/state/enginebox/members/{}/claude",
        member::member_key(ALICE)
    ));
    let state: Value =
        serde_json::from_slice(&std::fs::read(config.join(".claude.json")).unwrap()).unwrap();
    assert_eq!(state["mcpServers"]["browser"]["command"], "playwright-mcp");
    // Hosted repositories are not a member run mode in this release.
    assert!(
        member::parse(
            json!({"mode":"run-repository","input":{},"member_key":member::member_key(ALICE),
            "credential":claude_material()})
        )
        .is_err()
    );
}

// ------------------------------------------------------------ isolation

fn claude_reader(service: &AgentService) {
    service.set_member_material(Arc::new(|_, run, _, _| {
        Delegated::from_material(
            "claude",
            &json!({"kind":"environment","content":{"CLAUDE_CODE_OAUTH_TOKEN":format!("sk-ant-oat01-{}-0123456789", run.member)}}),
        )
    }));
}
/// Holds every run until released; each run reports a session of its own.
fn held_executor(service: &AgentService) -> Arc<(Mutex<bool>, std::sync::Condvar)> {
    let gate: Arc<(Mutex<bool>, std::sync::Condvar)> = Arc::default();
    let held = gate.clone();
    service.set_run_executor(Arc::new(move |run: &AdmittedRun| {
        let (open, changed) = &*held;
        let mut open = open.lock().unwrap();
        while !*open {
            open = changed.wait(open).unwrap();
        }
        // A session ID per run, derived from the run ID.
        let id = &run.id;
        let session = run.request.resume_id.clone().unwrap_or(format!(
            "{}-{}-{}-{}-{}",
            &id[0..8],
            &id[8..12],
            &id[12..16],
            &id[16..20],
            &id[20..32]
        ));
        Ok(
            json!({"exit_code":0,"output":json!({"type":"result","session_id":session}).to_string(),
            "timed_out":false,"output_truncated":false}),
        )
    }));
    gate
}
fn release(gate: &Arc<(Mutex<bool>, std::sync::Condvar)>) {
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
}
fn placed(service: &AgentService, id: &str) -> Option<String> {
    service.get_pool_run("jam", id).unwrap()["name"]
        .as_str()
        .filter(|n| *n != "ebox-unplaced")
        .map(str::to_owned)
}

#[test]
fn a_workspace_serves_one_member_only_for_its_whole_life() {
    let (_t, root) = root();
    let service = service(&root);
    service
        .pools()
        .put(
            json!({"name":"jam","project":"qoh","workspaces":[{"host_id":"h1","name":"ebox-m1"},{"host_id":"h1","name":"ebox-m2"}]}),
            &|h| h == "h1",
        )
        .unwrap();
    service.set_member_runs("jam", true).unwrap();
    claude_reader(&service);
    let gate = held_executor(&service);
    let launch = |who: &str, cred: &str, extra: Value| {
        let mut b = body("claude");
        for (k, v) in extra.as_object().unwrap() {
            b[k] = v.clone();
        }
        service.launch_pool("jam", b, "owner", member(who, cred))
    };
    let alice = launch(ALICE, ALICE_CRED, json!({})).unwrap().0["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let bob = launch(BOB, BOB_CRED, json!({})).unwrap().0["id"]
        .as_str()
        .unwrap()
        .to_owned();
    wait_for("both placed", || {
        placed(&service, &alice).is_some() && placed(&service, &bob).is_some()
    });
    let alice_ws = placed(&service, &alice).unwrap();
    let bob_ws = placed(&service, &bob).unwrap();
    assert_ne!(alice_ws, bob_ws);
    release(&gate);
    wait_for("both finished", || {
        [&alice, &bob]
            .iter()
            .all(|id| service.get_pool_run("jam", id).unwrap()["status"] == "succeeded")
    });
    // Both workspaces are now free, yet each member only ever gets their own.
    for _ in 0..3 {
        for (who, cred, own) in [(ALICE, ALICE_CRED, &alice_ws), (BOB, BOB_CRED, &bob_ws)] {
            let id = launch(who, cred, json!({})).unwrap().0["id"]
                .as_str()
                .unwrap()
                .to_owned();
            wait_for("placed", || placed(&service, &id).is_some());
            assert_eq!(&placed(&service, &id).unwrap(), own, "{who}");
        }
    }
    // A third member waits rather than reuse someone else's workspace.
    let carol = "usr_cccccccccccccccccccccccc";
    let waiting = launch(carol, "cccccccccccccccccccccccccccccccc", json!({}))
        .unwrap()
        .0["id"]
        .as_str()
        .unwrap()
        .to_owned();
    std::thread::sleep(Duration::from_millis(500));
    assert!(placed(&service, &waiting).is_none());
    // Alice cannot resume Bob's session (it would land on Bob's workspace).
    let bob_session = service.get_pool_run("jam", &bob).unwrap()["native_session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let crossed = launch(ALICE, ALICE_CRED, json!({"resume_id": bob_session}));
    assert!(crossed.is_err(), "{crossed:?}");
    // Bob can resume his own.
    launch(BOB, BOB_CRED, json!({"resume_id": bob_session})).unwrap();
    // A named-workspace run cannot reach a member's workspace.
    assert!(
        service
            .launch_with(
                "h1",
                &alice_ws,
                body("claude"),
                "owner",
                LaunchOptions::default()
            )
            .is_err()
    );
    // Bindings survive a restart.
    let reopened = enginebox_agents::pools::Pools::open(&root).unwrap();
    let ws = |name: &str| enginebox_agents::queue::Placement {
        host_id: "h1".into(),
        name: name.into(),
    };
    assert_eq!(
        reopened.bound_member(&ws(&alice_ws)).as_deref(),
        Some(ALICE)
    );
    assert_eq!(reopened.bound_member(&ws(&bob_ws)).as_deref(), Some(BOB));
}

#[test]
fn a_deleted_workspace_is_unbound_and_its_successor_is_fresh() {
    let (_t, root) = root();
    let pools = enginebox_agents::pools::Pools::open(&root).unwrap();
    pools
        .put(
            json!({"name":"jam","project":"qoh","template":"agents","autoscale":{"hosts":["h1"],"max_workspaces":2}}),
            &|h| h == "h1",
        )
        .unwrap();
    pools.set_member_runs("jam", true).unwrap();
    let created = enginebox_agents::queue::Placement {
        host_id: "h1".into(),
        name: "ebox-jam-1".into(),
    };
    pools.add_created("jam", created.clone()).unwrap();
    pools.bind_member(&created, ALICE).unwrap();
    assert!(pools.bind_member(&created, BOB).is_err());
    pools.remove_created("jam", &created).unwrap();
    assert_eq!(pools.bound_member(&created), None);
}

fn quick_executor(service: &AgentService) {
    service.set_run_executor(Arc::new(|run: &AdmittedRun| {
        let id = &run.id;
        let session = run.request.resume_id.clone().unwrap_or(format!(
            "{}-{}-{}-{}-{}",
            &id[0..8],
            &id[8..12],
            &id[12..16],
            &id[16..20],
            &id[20..32]
        ));
        Ok(
            json!({"exit_code":0,"output":json!({"type":"result","session_id":session}).to_string(),
            "timed_out":false,"output_truncated":false}),
        )
    }));
}
fn run_and_wait(service: &AgentService, who: &str, cred: &str) -> String {
    let id = service
        .launch_pool("jam", body("claude"), "owner", member(who, cred))
        .unwrap()
        .0["id"]
        .as_str()
        .unwrap()
        .to_owned();
    wait_for("the run", || {
        service.get_pool_run("jam", &id).unwrap()["status"] == "succeeded"
    });
    id
}

#[test]
fn a_full_pool_recreates_an_idle_members_workspace_for_a_waiting_member() {
    let (_t, root) = root();
    let service = service(&root);
    service.set_create_hook(Arc::new(|_, _, _, _| Ok(())));
    let deleted: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = deleted.clone();
    service.set_delete_hook(Arc::new(move |_, name| {
        log.lock().unwrap().push(name.to_owned());
        Ok(())
    }));
    service
        .pools()
        .put(
            json!({"name":"jam","project":"qoh","template":"agents","min_workspaces":0,
                "autoscale":{"hosts":["h1"],"max_workspaces":1}}),
            &|h| h == "h1",
        )
        .unwrap();
    service.set_member_runs("jam", true).unwrap();
    claude_reader(&service);
    quick_executor(&service);
    let alice = run_and_wait(&service, ALICE, ALICE_CRED);
    let first = placed(&service, &alice).unwrap();
    // The pool is at its limit and its only workspace is Alice's: Bob's run
    // gets a fresh workspace instead of waiting forever.
    let bob = run_and_wait(&service, BOB, BOB_CRED);
    let second = placed(&service, &bob).unwrap();
    assert_ne!(first, second);
    assert_eq!(*deleted.lock().unwrap(), vec![first.clone()]);
    let pools = enginebox_agents::pools::Pools::open(&root).unwrap();
    let ws = |name: &str| enginebox_agents::queue::Placement {
        host_id: "h1".into(),
        name: name.into(),
    };
    assert_eq!(pools.bound_member(&ws(&first)), None);
    assert_eq!(pools.bound_member(&ws(&second)).as_deref(), Some(BOB));
}

#[test]
fn bound_workspaces_are_released_explicitly_and_legacy_shared_ones_are_tainted() {
    let (_t, root) = root();
    let service = service(&root);
    service
        .pools()
        .put(
            json!({"name":"jam","project":"qoh","workspaces":[{"host_id":"h1","name":"ebox-m1"}]}),
            &|h| h == "h1",
        )
        .unwrap();
    service.set_member_runs("jam", true).unwrap();
    claude_reader(&service);
    quick_executor(&service);
    let alice = run_and_wait(&service, ALICE, ALICE_CRED);
    let ws = enginebox_agents::queue::Placement {
        host_id: "h1".into(),
        name: "ebox-m1".into(),
    };
    // Turning member runs off or deleting the pool would hand Alice's
    // workspace to someone else: refused until it is released.
    wait_for("idle", || service.pools().bound_member(&ws).is_some());
    assert!(service.set_member_runs("jam", false).is_err());
    assert!(service.delete_pool("jam").is_err());
    // A listed workspace is released only once the owner says it was wiped.
    let release = |recreated: bool| {
        service.release_member_workspace(
            "jam",
            &json!({"host_id":"h1","name":"ebox-m1","recreated":recreated}),
        )
    };
    assert!(release(false).is_err());
    wait_for("the release", || release(true).is_ok());
    assert_eq!(service.pools().bound_member(&ws), None);
    // Simulate a workspace shared before bindings existed: Bob runs there
    // too, then the bindings file is lost (an upgrade from before #134).
    let _bob = run_and_wait(&service, BOB, BOB_CRED);
    std::fs::remove_file(root.join("agent-member-bindings.json")).unwrap();
    let alice_session = service.get_pool_run("jam", &alice).unwrap()["native_session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    drop(service);
    let service = self::service(&root);
    claude_reader(&service);
    assert_eq!(
        service.pools().bound_member(&ws).as_deref(),
        Some(enginebox_agents::pools::TAINTED)
    );
    let mut resume = body("claude");
    resume["resume_id"] = json!(alice_session);
    let refused = service
        .launch_pool("jam", resume, "owner", member(ALICE, ALICE_CRED))
        .unwrap_err();
    assert!(refused.to_string().contains("several members"), "{refused}");
}
