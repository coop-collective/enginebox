//! Explicit network qualification against original clients, without completing sign-in.
#![cfg(target_os = "linux")]

use anyhow::{Context, Result, ensure};
use enginebox_agents::{AgentService, auth, delegation, native, security};
use enginebox_core::{Fleet, Host};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn children(pid: u32) -> Result<BTreeSet<u32>> {
    let mut result = BTreeSet::new();
    let tasks = match fs::read_dir(format!("/proc/{pid}/task")) {
        Ok(tasks) => tasks,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(result),
        Err(error) => return Err(error.into()),
    };
    for task in tasks {
        let path = task?.path().join("children");
        match fs::read_to_string(path) {
            Ok(ids) => {
                for id in ids.split_whitespace() {
                    result.insert(id.parse()?);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(result)
}

fn descendants(pid: u32) -> Result<BTreeSet<u32>> {
    let mut found = BTreeSet::new();
    let mut pending = vec![pid];
    while let Some(parent) = pending.pop() {
        for child in children(parent)? {
            if found.insert(child) {
                pending.push(child);
            }
        }
        ensure!(found.len() <= 256, "Unexpected qualification process count");
    }
    Ok(found)
}

struct Cleanup(AgentService);
impl Drop for Cleanup {
    fn drop(&mut self) {
        self.0.close();
        let deadline = Instant::now() + Duration::from_secs(15);
        while self.0.pending() > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

fn credentials_absent(home: &Path) -> bool {
    [
        delegation::native_file("codex"),
        delegation::native_file("claude"),
        delegation::native_file("cursor"),
        delegation::TOKEN_FILE,
        ".config/enginebox/agents/cursor.env",
    ]
    .iter()
    .all(|name| !home.join(name).exists())
}

#[test]
#[ignore = "requires explicit Linux qualification opt-in, fresh private state and installed native clients; never signs in"]
fn native_authorization_prompts_cancel_without_credentials() -> Result<()> {
    ensure!(
        std::env::var("ENGINEBOX_NATIVE_AUTH_QUALIFY").as_deref() == Ok("1"),
        "Set ENGINEBOX_NATIVE_AUTH_QUALIFY=1 explicitly"
    );
    ensure!(
        unsafe { libc::geteuid() } != 0,
        "Run as an unprivileged user"
    );
    let root = PathBuf::from(
        std::env::var_os("ENGINEBOX_NATIVE_AUTH_ROOT")
            .context("Missing fresh qualification root")?,
    );
    let clients = PathBuf::from(
        std::env::var_os("ENGINEBOX_NATIVE_AUTH_CLIENT_STATE")
            .context("Missing isolated client installation")?,
    );
    ensure!(
        root.is_absolute() && clients.is_absolute(),
        "Use absolute qualification paths"
    );
    security::check_components(&root)?;
    security::private_dir(root.parent().context("Missing qualification parent")?)?;
    // No existing state, accounts or provider homes may be reused.
    fs::DirBuilder::new().mode(0o700).create(&root)?;
    let mut binaries = BTreeMap::new();
    for provider in enginebox_agents::PROVIDERS {
        let path = clients.join("agents/bin").join(native::binary(provider));
        ensure!(
            path.is_file(),
            "Qualified native client is missing: {provider}"
        );
        binaries.insert(provider.to_owned(), path.to_string_lossy().into_owned());
    }
    let fleet = Fleet::from_hosts(
        vec![Host {
            id: "unused-auth-fixture".into(),
            target: "unused-auth-fixture.invalid".into(),
            ..Default::default()
        }],
        &root,
    )?;
    let service = AgentService::with_binaries(&root, fleet, binaries)?;
    let _cleanup = Cleanup(service.clone());
    ensure!(
        descendants(std::process::id())?.is_empty(),
        "Qualification starts with existing child processes"
    );
    let mut results = Vec::new();
    for provider in enginebox_agents::PROVIDERS {
        let home = root.join("agents/homes").join(provider);
        ensure!(credentials_absent(&home), "Fresh home contains credentials");
        let started = Instant::now();
        let session = service.start(provider)?;
        let id = session["id"]
            .as_str()
            .context("Missing connection session")?;
        let deadline = Instant::now() + Duration::from_secs(60);
        let observed = loop {
            let current = service.session(provider, id)?;
            let prompt = current["authorization_url"].is_string()
                && match provider {
                    "codex" => current["user_code"].is_string(),
                    "claude" => current["input_required"] == true,
                    _ => true,
                };
            if prompt
                || !matches!(current["status"].as_str(), Some("starting" | "waiting"))
                || Instant::now() >= deadline
            {
                break current;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let prompt_seconds = started.elapsed().as_secs_f64();
        let native_pids = descendants(std::process::id())?;
        let authorization = observed["authorization_url"]
            .as_str()
            .and_then(|value| auth::authorization_url(provider, value))
            .and_then(|value| url::Url::parse(&value).ok());
        let waiting = observed["status"] == "waiting";
        let expected_input = match provider {
            "codex" => observed["user_code"].is_string(),
            "claude" => observed["input_required"] == true,
            _ => true,
        };
        let lease_path = format!(".local/state/enginebox/auth-{provider}.lock");
        let held_while_waiting = security::Lease::active(&home, &lease_path)?;
        let pending_while_waiting = service.pending() > 0;
        service.cancel(provider, id)?;
        let cancelled_at = Instant::now();
        while service.pending() > 0 && cancelled_at.elapsed() < Duration::from_secs(15) {
            std::thread::sleep(Duration::from_millis(25));
        }
        let final_session = service.session(provider, id)?;
        let lease_released = !security::Lease::active(&home, &lease_path)?;
        let children_gone = descendants(std::process::id())?.is_empty()
            && native_pids
                .iter()
                .all(|pid| !Path::new(&format!("/proc/{pid}")).exists());
        let clean =
            service.pending() == 0 && lease_released && children_gone && credentials_absent(&home);
        let passed = waiting
            && expected_input
            && authorization.is_some()
            && pending_while_waiting
            && held_while_waiting
            && !native_pids.is_empty()
            && final_session["status"] == "cancelled"
            && clean;
        results.push(json!({
            "provider":provider,"passed":passed,"prompt_seconds":prompt_seconds,
            "authorization_host":authorization.as_ref().and_then(url::Url::host_str),
            "authorization_path":authorization.as_ref().map(url::Url::path),
            "expected_prompt_input":expected_input,"observed_status":observed["status"],
            "native_processes_observed":native_pids.len(),"pending_while_waiting":pending_while_waiting,
            "lease_held_while_waiting":held_while_waiting,"cancel_seconds":cancelled_at.elapsed().as_secs_f64(),
            "final_status":final_session["status"],"pending_after_cancel":service.pending(),
            "native_processes_exited":children_gone,"lease_released":lease_released,
            "credentials_absent":credentials_absent(&home)
        }));
        security::write_json(
            &root,
            "receipt.json",
            &json!({
                "version":env!("CARGO_PKG_VERSION"),"tested_at":chrono::Utc::now().to_rfc3339(),
                "scope":"original native clients through Rust AgentService start/session/cancel",
                "providers":results,"sign_in_completed":false,"inference_performed":false,
                "existing_accounts_accessed":false,"passed":results.len()==3 && results.iter().all(|v|v["passed"]==true)
            }),
        )?;
        ensure!(
            clean,
            "Native cancellation cleanup failed; see the private qualification receipt"
        );
    }
    ensure!(
        results.iter().all(|v| v["passed"] == true),
        "Native prompt qualification failed; see the private receipt"
    );
    Ok(())
}
