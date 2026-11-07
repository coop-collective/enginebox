//! A member run through the real workspace path (docs/MEMBER-SUBSCRIPTIONS.md):
//! placement, `prepare()` (inventory, host lock, guest version check, clearing
//! the workspace of the controller's accounts), the detached `run-member`
//! guest command, `collect` and the recorded result. Only the host boundary
//! is a stand-in: `sudo` on PATH answers `enginebox-host inventory|lock` and
//! runs `incus exec … -- enginebox-guest …` as this release's real guest
//! helper against a temporary guest home. It is the only test in this binary
//! because it changes PATH.
#![cfg(unix)]
use enginebox_agents::{
    AgentService,
    member::Delegated,
    queue::{HostLoad, MemberRun, QueueLimits},
    security,
    service::LaunchOptions,
};
use enginebox_core::{Fleet, GuestConfig, Host};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

const ALICE: &str = "usr_aaaaaaaaaaaaaaaaaaaaaaaa";
const ALICE_CRED: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const ALICE_TOKEN: &str = "sk-ant-oat01-alice-real-path-0123456789";
const OWNER_TOKEN: &str = "sk-ant-oat01-owner-controller-0123456789";

fn executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn a_member_run_prepares_a_real_workspace_without_the_controllers_accounts() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().canonicalize().unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let controller = root.join("controller");
    let home = root.join("guest-home");
    let workspace = root.join("guest-workspace");
    let bin = root.join("bin");
    for dir in [&controller, &home, &workspace, &bin] {
        security::private_dir(dir).unwrap();
    }
    // The guest home as the controller left it: its own Claude account
    // delegated (settings override, delegation marker, native login).
    security::write_json(
        &home,
        ".claude/settings.json",
        &json!({"env":{"CLAUDE_CODE_OAUTH_TOKEN":OWNER_TOKEN},"theme":"dark"}),
    )
    .unwrap();
    security::write_json(
        &home,
        ".config/enginebox/agents/claude.json",
        &json!({"generation":"x"}),
    )
    .unwrap();
    security::write_json(
        &home,
        ".claude/.credentials.json",
        &json!({"claudeAiOauth":{"accessToken":OWNER_TOKEN}}),
    )
    .unwrap();
    // The native client: records what it authenticates with and prints it.
    security::safe_path(&home, ".local/bin/claude", true).unwrap();
    executable(
        &home.join(".local/bin/claude"),
        "#!/bin/sh\nif [ \"$1\" = --version ]; then echo '2.1.302 (Claude Code)'; exit 0; fi\ncat >/dev/null\n\
         { echo \"token=$CLAUDE_CODE_OAUTH_TOKEN\"; echo \"config=$CLAUDE_CONFIG_DIR\"; echo \"argv=$*\"; } >>\"$HOME/.seen\"\n\
         echo \"leak $CLAUDE_CODE_OAUTH_TOKEN\"\nprintf '{\"type\":\"result\",\"session_id\":\"0b9a5a6e-3c1f-4c7e-9a43-2f3e4d5c6b7a\"}\\n'\n",
    );
    // The host boundary: `sudo -n enginebox-host …` and `sudo -n incus … exec`.
    let guest = env!("CARGO_BIN_EXE_enginebox-guest");
    let inventory = json!({"instances":[{"project":"enginebox","name":"ebox-m1","managed":true,
        "pool":"default","status":"Running","template":null}]});
    executable(
        &bin.join("sudo"),
        &format!(
            r#"#!/bin/bash
shift
if [ "$1" = enginebox-host ]; then
  case "$2" in
    inventory) printf '%s' '{inventory}' ;;
    lock) echo locked; cat >/dev/null ;;
    *) exit 3 ;;
  esac
  exit 0
fi
if [ "$1" = incus ]; then
  home=""; cwd=""
  while [ "$#" -gt 0 ] && [ "$1" != "--" ]; do
    case "$1" in --env=HOME=*) home="${{1#--env=HOME=}}" ;; --cwd=*) cwd="${{1#--cwd=}}" ;; esac
    shift
  done
  shift
  cd "$cwd" || exit 5
  if [ "$1" = /usr/local/bin/enginebox-guest ]; then
    shift
    HOME="$home" exec '{guest}' "$@"
  fi
  HOME="$home" exec "$@"
fi
exit 4
"#,
            inventory = inventory,
            guest = guest
        ),
    );
    std::os::unix::fs::symlink(guest, bin.join("enginebox-guest")).unwrap();
    // Only this test runs in this binary, before any other thread uses PATH.
    unsafe {
        std::env::set_var(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        );
    }
    let host = Host {
        id: "h1".into(),
        target: "local".into(),
        project: "enginebox".into(),
        pool: "default".into(),
        max_agent_runs: Some(2),
        guest: GuestConfig {
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
            home: home.to_string_lossy().into_owned(),
            workspace: workspace.to_string_lossy().into_owned(),
            ..Default::default()
        },
        ..Default::default()
    };
    let fleet = Fleet::from_hosts(vec![host], &controller).unwrap();
    let service =
        AgentService::with_limits(&controller, fleet, BTreeMap::new(), QueueLimits::default())
            .unwrap();
    service.set_measure_hook(Arc::new(|_| {
        Ok(HostLoad {
            cpu_total: 64,
            reserve_cpu: 2,
            memory_available_bytes: 256 << 30,
            reserve_memory_bytes: 4 << 30,
        })
    }));
    service
        .pools()
        .put(
            json!({"name":"jam","project":"qoh","workspaces":[{"host_id":"h1","name":"ebox-m1"}]}),
            &|h| h == "h1",
        )
        .unwrap();
    service.set_member_runs("jam", true).unwrap();
    service.set_member_material(Arc::new(|_, run, provider, _| {
        anyhow::ensure!(run.member == ALICE && provider == "claude");
        Delegated::from_material(
            "claude",
            &json!({"kind":"environment","content":{"CLAUDE_CODE_OAUTH_TOKEN":ALICE_TOKEN}}),
        )
    }));
    // An ordinary run cannot reach the member-runs workspace directly.
    assert!(
        service
            .launch_with(
                "h1",
                "ebox-m1",
                json!({"provider":"claude","prompt":"x"}),
                "owner",
                LaunchOptions::default()
            )
            .is_err()
    );
    let id = service
        .launch_pool(
            "jam",
            json!({"provider":"claude","prompt":"make the jump higher","timeout_seconds":60}),
            "owner",
            LaunchOptions {
                member: Some(MemberRun {
                    member: ALICE.into(),
                    credential_id: ALICE_CRED.into(),
                }),
                ..Default::default()
            },
        )
        .unwrap()
        .0["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let deadline = Instant::now() + Duration::from_secs(60);
    let run = loop {
        let run = service.get_pool_run("jam", &id).unwrap();
        if !["queued", "running"].contains(&run["status"].as_str().unwrap_or("")) {
            break run;
        }
        assert!(Instant::now() < deadline, "{run}");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(run["status"], "succeeded", "{run}");
    // The CLI authenticated only as Alice, in her own configuration directory.
    let seen = std::fs::read_to_string(home.join(".seen")).unwrap();
    assert!(seen.contains(&format!("token={ALICE_TOKEN}")), "{seen}");
    assert!(
        seen.contains(&format!(
            "config={}/.local/state/enginebox/members/",
            home.display()
        )),
        "{seen}"
    );
    assert!(!seen.contains(OWNER_TOKEN));
    assert!(
        !seen
            .lines()
            .any(|l| l.starts_with("argv=") && l.contains(ALICE_TOKEN))
    );
    // The controller's delegation was cleared before the run; nothing of
    // Alice's material remains in the guest home or the controller's state.
    assert!(!home.join(".claude/.credentials.json").exists());
    assert!(!home.join(".config/enginebox/agents/claude.json").exists());
    let settings = security::read_json(&home, ".claude/settings.json")
        .unwrap()
        .unwrap();
    assert_eq!(settings, json!({"env":{},"theme":"dark"}));
    assert!(!run.to_string().contains(ALICE_TOKEN));
    for dir in [&home, &controller, &workspace] {
        for path in walk(dir) {
            if path.file_name().is_some_and(|n| n == ".seen") {
                continue;
            }
            let text =
                String::from_utf8_lossy(&std::fs::read(&path).unwrap_or_default()).into_owned();
            assert!(
                !text.contains(ALICE_TOKEN) && !text.contains(OWNER_TOKEN),
                "{} holds a token",
                path.display()
            );
        }
    }
    let _: Value = run;
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = vec![];
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = e.path();
        match std::fs::symlink_metadata(&path) {
            Ok(m) if m.is_dir() => out.extend(walk(&path)),
            Ok(m) if m.is_file() => out.push(path),
            _ => {}
        }
    }
    out
}
