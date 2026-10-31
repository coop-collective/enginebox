use anyhow::{Result, ensure};
use enginebox_agents::{self as agents, auth, browser, delegation, native, security};
use serde_json::{Value, json};
use std::{io::Read, path::Path};
fn input(limit: usize) -> Result<Value> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "Request exceeds bound.");
    Ok(serde_json::from_slice(&bytes)?)
}
fn input_limit(mode: &str) -> usize {
    match mode {
        "run-profile" => agents::profile::MAX_ENVELOPE_BYTES,
        "run-repository" => agents::repository::MAX_ENVELOPE_BYTES,
        _ => 16384,
    }
}
/// One synchronous native run (the `run*` modes, also a detached worker's body).
fn execute(
    mode: &str,
    home: &Path,
    workspace: &str,
    id: Option<&str>,
    input: Value,
) -> Result<Value> {
    let workspace = Path::new(workspace);
    Ok(match mode {
        "run" => serde_json::to_value(native::execute_scoped(
            &native::RunRequest::parse(input)?,
            home,
            workspace,
            id,
        )?)?,
        "run-profile" => {
            let envelope: agents::profile::Envelope = serde_json::from_value(input)?;
            let id = id.ok_or_else(|| anyhow::anyhow!("Missing run ID."))?;
            serde_json::to_value(native::execute_profiled(&envelope, home, workspace, id)?)?
        }
        "run-repository" => {
            let envelope: agents::repository::Envelope = serde_json::from_value(input)?;
            let id = id.ok_or_else(|| anyhow::anyhow!("Missing run ID."))?;
            serde_json::to_value(native::execute_with_repository(
                &envelope, home, workspace, id,
            )?)?
        }
        _ => anyhow::bail!("Unknown run mode."),
    })
}
fn run() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let mode = args.first().map(String::as_str).unwrap_or("--help");
    if matches!(mode, "--help" | "-h") {
        println!(
            "enginebox-guest {}\nNative workspace helper\n\nCommands:\n  run WORKSPACE [ID]  Execute bounded native CLI request from JSON stdin\n  run-profile WORKSPACE ID  Execute a native CLI request under a profile envelope from JSON stdin\n  run-repository WORKSPACE ID  Execute a native CLI request with a hosted repository envelope from JSON stdin\n  detach MODE WORKSPACE ID  Start a run* request from stdin in its own session and return\n  collect ID          Read a detached run: finished (with result), failed, running, lost or unknown\n  forget ID           Remove a collected run's spool\n  cancel PROVIDER ID  Request cancellation of one controller run\n  lease PROVIDER      Report the native session writer lease\n  transfer            Deliver access-only native credentials from JSON stdin\n  status PROVIDER|all  Read native connection status\n  browser [OPTIONS]   Run the official Playwright MCP with a private profile\n  cursor-sandbox --home HOME --user USER --workspace PATH [--apply]",
            env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    }
    if mode == "--version" {
        println!("enginebox-guest {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if mode == "browser" {
        return browser::run(&args[1..]);
    }
    if mode == "screenshot" {
        use std::io::Write;
        std::io::stdout().write_all(&browser::screenshot()?)?;
        return Ok(());
    }
    if mode == "cursor-sandbox" {
        println!("{}", browser::cursor_sandbox(&args[1..])?);
        return Ok(());
    }
    let home = std::env::var("HOME")?;
    let home = Path::new(&home);
    let result = match mode {
        "run" | "run-profile" | "run-repository" => {
            let workspace = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("Missing workspace."))?;
            ensure!(
                mode == "run" || args.len() == 3,
                "Expected workspace and run ID."
            );
            let input = input(input_limit(mode))?;
            execute(
                mode,
                home,
                workspace,
                args.get(2).map(String::as_str),
                input,
            )?
        }
        // Start a run in its own session and return at once; `collect` reads its result.
        "detach" => {
            ensure!(args.len() == 4, "Expected mode, workspace and run ID.");
            let limit = input_limit(&args[1]);
            let mut raw = Vec::new();
            std::io::stdin()
                .take(limit as u64 + 1)
                .read_to_end(&mut raw)?;
            ensure!(raw.len() <= limit, "Request exceeds bound.");
            let exe = std::env::current_exe()?.display().to_string();
            agents::detached::start(home, &args[3], &args[1], &args[2], &raw, &[exe])?
        }
        "detached-worker" => {
            ensure!(args.len() == 2, "Expected run ID.");
            let id = args[1].clone();
            agents::detached::run_worker(home, &id, |mode, workspace, input| {
                execute(mode, home, workspace, Some(&id), input)
            })?;
            json!({"ok":true})
        }
        "collect" => {
            ensure!(args.len() == 2, "Expected run ID.");
            agents::detached::collect(home, &args[1])?
        }
        "forget" => {
            ensure!(args.len() == 2, "Expected run ID.");
            agents::detached::forget(home, &args[1])?;
            json!({"ok":true})
        }
        "cancel" => {
            ensure!(args.len() == 3, "Expected provider and run ID.");
            native::request_cancel(home, &args[1], &args[2])?;
            json!({"ok":true,"cancel_requested":true})
        }
        "lease" => {
            let p = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("Missing provider."))?;
            json!({"active":native::lease_active(home,p)?})
        }
        "transfer" => {
            let id = delegation::identity(
                &[
                    Path::new("/var/lib/enginebox/instance-uuid"),
                    Path::new("/etc/machine-id"),
                ],
                0,
            )?;
            delegation::transfer(home, &input(security::LIMIT)?, &id)?
        }
        "status" => {
            let p = args.get(1).map(String::as_str).unwrap_or("all");
            let providers = if p == "all" {
                agents::PROVIDERS.to_vec()
            } else {
                vec![agents::provider(p)?]
            };
            let env = native::environment(home)?;
            let statuses = std::thread::scope(|scope| {
                providers
                    .iter()
                    .map(|p| {
                        let env = &env;
                        scope.spawn(move || {
                            let exe = native::find_binary(native::binary(p), env);
                            auth::probe(p, exe.as_deref(), env)
                        })
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|handle| handle.join().unwrap_or(Value::Null))
                    .collect::<Vec<_>>()
            });
            json!({"type":"statuses","providers":statuses})
        }
        _ => anyhow::bail!("Unknown guest command."),
    };
    println!("{result}");
    Ok(())
}
fn main() {
    #[cfg(unix)]
    unsafe {
        libc::umask(0o077);
    }
    if run().is_err() {
        eprintln!(
            "enginebox-guest: operation failed; check installation, private paths and session leases."
        );
        println!(
            "{}",
            json!({"error":"Native guest operation failed; check installation, private paths and session leases."})
        );
        std::process::exit(1);
    }
}
