//! Process-boundary tests run the actual Fleet against an executable Rust Incus fixture.
//! Each scenario has its own child environment; tests never replace the parent PATH.
use anyhow::{Context, Result, bail, ensure};
#[cfg(unix)]
use enginebox_core::GIB;
use enginebox_core::{
    Fleet, Host, Resources, capacity,
    model::{StaticTemplate, WarmPolicy},
    warm,
};
use serde_json::{Value, json};
use std::{io::Write, path::PathBuf, process::Command};
fn fixture_root() -> PathBuf {
    PathBuf::from(std::env::var("EBOX_TEST_ROOT").unwrap())
}
fn load() -> Result<Value> {
    Ok(serde_json::from_slice(&std::fs::read(
        fixture_root().join("state.json"),
    )?)?)
}
fn save(v: &Value) -> Result<()> {
    std::fs::write(fixture_root().join("state.json"), serde_json::to_vec(v)?)?;
    Ok(())
}
fn log(args: &[String]) -> Result<()> {
    use std::fs::OpenOptions;
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(fixture_root().join("events.jsonl"))?;
    serde_json::to_writer(&mut f, args)?;
    f.write_all(b"\n")?;
    Ok(())
}
fn fixture_program(kind: &str, args: &[String]) -> Result<()> {
    if kind == "sudo" {
        let args = if args.first().is_some_and(|a| a == "-n") {
            &args[1..]
        } else {
            args
        };
        let status = Command::new(&args[0]).args(&args[1..]).status()?;
        std::process::exit(status.code().unwrap_or(1));
    }
    if kind == "enginebox-host" {
        match args.first().map(String::as_str) {
            Some("inventory") => {
                println!("{}", load()?["facts"]);
                return Ok(());
            }
            Some("sync-guest-tools") => {
                let mut data = load()?;
                ensure!(
                    data["facts"]["instances"][0]["status"] == "Running",
                    "Migration attempted on frozen VM"
                );
                ensure!(
                    data["facts"]["instances"][0]["pool_claim"]["state"] == "claimed",
                    "Legacy warm helper migration preceded durable assignment"
                );
                log(args)?;
                data["native_helpers"] = json!(true);
                save(&data)?;
                println!(
                    "{{\"changed\":[\"enginebox-host\",\"enginebox-guest\",\"enginebox-mcp\"]}}"
                );
                return Ok(());
            }
            Some("lock") => {
                use fs2::FileExt;
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(fixture_root().join("host.lock"))?;
                file.try_lock_exclusive()?;
                println!("locked");
                std::io::stdout().flush()?;
                std::io::copy(&mut std::io::stdin(), &mut std::io::sink())?;
                return Ok(());
            }
            _ => bail!("Unexpected native-host fixture call"),
        }
    }
    ensure!(kind == "incus", "Unknown fixture executable");
    let args = if args.first().is_some_and(|a| a == "--project") {
        &args[2..]
    } else {
        args
    };
    log(args)?;
    let mut data = load()?;
    match args[0].as_str() {
        "query" => {
            let path = &args[1];
            if path.contains("/snapshots/") {
                println!("{}", data["snapshot"]);
            } else {
                let name = path
                    .split("/instances/")
                    .nth(1)
                    .context("Unexpected query")?
                    .split('?')
                    .next()
                    .unwrap();
                let vm = data["facts"]["instances"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|v| v["name"] == name)
                    .context("Unknown VM")?;
                println!(
                    "{}",
                    json!({"name":vm["name"],"status":vm["status"],"stateful":vm["stateful"].as_bool().unwrap_or(false),"type":"virtual-machine","devices":{"root":{"type":"disk","path":"/","pool":"pool","size":"30GiB","size.state":"4GiB"}},"expanded_devices":{"root":{"type":"disk","path":"/","pool":"pool","size":"30GiB","size.state":"4GiB"}}})
                );
            }
        }
        "config" => {
            if args[1] == "device" {
                return Ok(());
            }
            ensure!(args[1] == "set", "Unexpected config command");
            let vm = data["facts"]["instances"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|v| v["name"] == args[2])
                .context("Unknown VM")?;
            for arg in &args[3..] {
                if arg == "--property" {
                    continue;
                }
                let (k, v) = arg.split_once('=').context("Invalid config arg")?;
                if k == "description" {
                    vm["description"] = json!(v);
                    let metadata = capacity::pool_metadata(
                        vm,
                        &json!({"user.enginebox.pool.state":vm["warm_pool_state"]}),
                    );
                    vm.as_object_mut()
                        .unwrap()
                        .extend(metadata.as_object().unwrap().clone());
                } else {
                    ensure!(
                        vm["status"] != "Frozen",
                        "Config changes must not notify frozen guest"
                    );
                    match k {
                        "user.enginebox.pool.state" => vm["warm_pool_state"] = json!(v),
                        "user.enginebox.workspace" => vm["workspace_name"] = json!(v),
                        _ => bail!("Unexpected config key"),
                    }
                }
            }
            save(&data)?;
        }
        "start" => {
            let vm = data["facts"]["instances"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|v| v["name"] == args[1])
                .context("Unknown VM")?;
            if vm["warm_pool_state"] == "claimed" {
                ensure!(
                    vm["pool_claim"]["state"] == "claimed",
                    "Claim was not persisted before wake"
                );
            }
            if std::env::var("EBOX_TEST_SCENARIO").as_deref() == Ok("wake-failure") {
                bail!("Simulated start failure after claim");
            }
            vm["status"] = json!("Running");
            save(&data)?;
        }
        "pause" | "stop" => {
            let vm = data["facts"]["instances"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|v| v["name"] == args[1])
                .context("Unknown VM")?;
            vm["status"] = json!(if args[0] == "pause" {
                "Frozen"
            } else {
                "Stopped"
            });
            save(&data)?;
        }
        "exec" => {
            let last = args.last().unwrap();
            if last.ends_with("--version") {
                ensure!(
                    data["native_helpers"] != false,
                    "Legacy guest has no native helper"
                );
                let binary = last
                    .split_whitespace()
                    .find(|s| s.starts_with("enginebox-"))
                    .context("Unknown version probe")?;
                println!("{binary} {}", env!("CARGO_PKG_VERSION"));
            } else if last.contains("enginebox-host readiness") {
                println!("{{\"ready\":true,\"identity\":true,\"dns\":true}}");
            } else if last.contains("enginebox-guest lease") {
                println!(
                    "{}",
                    json!({"active":std::env::var("EBOX_TEST_SCENARIO").as_deref()==Ok("active-session")})
                );
            } else {
                bail!("Unexpected guest execution")
            }
        }
        "copy" => {
            let mut vm = data["facts"]["instances"][0].clone();
            vm["name"] = json!(args[2]);
            vm["status"] = json!("Stopped");
            vm["warm_pool_state"] = Value::Null;
            vm["pool_claim"] = Value::Null;
            vm["workspace_name"] = Value::Null;
            for pair in args.windows(2) {
                if pair[0] == "-c"
                    && let Some((k, v)) = pair[1].split_once('=')
                {
                    match k {
                        "user.enginebox.pool.state" => vm["warm_pool_state"] = json!(v),
                        "user.enginebox.pool.generation" => vm["pool_generation"] = json!(v),
                        "user.enginebox.pool.size" => vm["pool_size"] = json!(v),
                        "user.enginebox.pool.created_at" => vm["pool_created_at"] = json!(v),
                        _ => {}
                    }
                }
            }
            data["facts"]["instances"].as_array_mut().unwrap().push(vm);
            save(&data)?;
        }
        _ => bail!("Unexpected Incus command: {}", args[0]),
    };
    Ok(())
}
fn host() -> Host {
    Host {
        id: "primary".into(),
        label: "Fixture".into(),
        target: "local".into(),
        project: "enginebox".into(),
        pool: "pool".into(),
        templates: vec![StaticTemplate {
            id: "native".into(),
            label: "Native".into(),
            source: "ebox-base".into(),
            snapshot: "prepared".into(),
            minimum: Some(Resources {
                cpu: 2,
                memory_gib: 2,
                disk_gib: 30,
            }),
            recommended: Some(Resources {
                cpu: 2,
                memory_gib: 2,
                disk_gib: 30,
            }),
        }],
        warm_pools: vec![WarmPolicy {
            template: "native".into(),
            size: "recommended".into(),
            ready: 1,
        }],
        ..Default::default()
    }
}
fn scenario() -> Result<()> {
    let root = fixture_root();
    let name = std::env::var("EBOX_TEST_SCENARIO")?;
    let f = Fleet::from_hosts(vec![host()], &root.join("controller"))?;
    match name.as_str() {
        "warm-success" | "legacy-warm" => {
            let value = f.operate(
                "primary",
                "allocate",
                &json!({"name":"ebox-private","template":"native"}),
            )?;
            ensure!(
                value["name"] == "ebox-warm-reserve"
                    && value["workspace_name"] == "ebox-private"
                    && value["startup"] == "warm",
                "Incorrect allocation result"
            );
            let state = load()?;
            ensure!(
                state["facts"]["instances"][0]["pool_claim"]["workspace_name"] == "ebox-private",
                "Claim not durable"
            );
            ensure!(
                f.operate(
                    "primary",
                    "allocate",
                    &json!({"name":"ebox-private","template":"native"})
                )
                .is_err(),
                "Duplicate requested workspace accepted"
            );
            let events = std::fs::read_to_string(root.join("events.jsonl"))?;
            ensure!(
                !events.lines().any(|line| line.starts_with("[\"copy\"")),
                "Warm assignment performed cold copy"
            );
            let claim = events.find("description=").unwrap();
            let start = events.find("[\"start\"").unwrap();
            ensure!(claim < start, "Claim committed after wake");
            if name == "legacy-warm" {
                let migration = events
                    .find("[\"sync-guest-tools\"")
                    .context("Legacy native helper migration not invoked")?;
                let readiness = events
                    .find("enginebox-host readiness")
                    .context("Missing readiness check")?;
                ensure!(
                    start < migration && migration < readiness,
                    "Legacy helper migration must follow wake and precede readiness"
                );
            }
        }
        "wake-failure" => {
            ensure!(
                f.operate(
                    "primary",
                    "allocate",
                    &json!({"name":"ebox-private","template":"native"})
                )
                .is_err(),
                "Injected wake failure ignored"
            );
            let state = load()?;
            ensure!(
                state["facts"]["instances"][0]["pool_claim"]["state"] == "claimed",
                "Failed wake returned private VM to pool"
            );
            ensure!(
                f.account_workspace("primary", "ebox-warm-reserve").is_err(),
                "Frozen VM authorized for credential attach"
            );
            ensure!(
                f.operate(
                    "primary",
                    "allocate",
                    &json!({"name":"ebox-private","template":"native"})
                )
                .is_err(),
                "Durable requested name reusable after failure"
            );
        }
        "active-session" => {
            let mut data = load()?;
            data["facts"]["instances"][0]["status"] = json!("Running");
            data["facts"]["instances"][0]["warm_pool_state"] = json!("claimed");
            data["facts"]["instances"][0]["pool_claim"] =
                json!({"state":"claimed","workspace_name":"ebox-private"});
            save(&data)?;
            ensure!(
                f.operate("primary", "hibernate", &json!({"name":"ebox-warm-reserve"}))
                    .is_err(),
                "Active native session permitted hibernate"
            );
            ensure!(
                load()?["facts"]["instances"][0]["status"] == "Running",
                "Guard failure mutated VM"
            );
        }
        "quarantine-blocks-refill" => {
            let mut data = load()?;
            data["facts"]["instances"][0]["warm_pool_state"] = json!("quarantined");
            data["facts"]["instances"][0]["pool_claim"] = json!({"state":"quarantined"});
            save(&data)?;
            warm::reconcile_once(&f, &host())?;
            ensure!(
                load()?["facts"]["instances"].as_array().unwrap().len() == 1,
                "Poisoned generation refilled automatically"
            );
        }
        "fresh-refill" => {
            let mut data = load()?;
            data["facts"]["instances"][0]["pool_claim"] =
                json!({"state":"claimed","workspace_name":"ebox-old"});
            data["facts"]["instances"][0]["warm_pool_state"] = json!("claimed");
            save(&data)?;
            let v = warm::reconcile_once(&f, &host())?;
            ensure!(
                v["warm_pool_state"] == "ready",
                "Fresh reserve not published"
            );
            let state = load()?;
            ensure!(
                state["facts"]["instances"][1]["status"] == "Frozen",
                "Reserve not frozen"
            );
            ensure!(
                state["facts"]["instances"][0]["pool_claim"]["workspace_name"] == "ebox-old",
                "Private workspace recycled"
            );
        }
        _ => bail!("Unknown scenario"),
    };
    Ok(())
}
#[cfg(unix)]
fn tests() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for scenario_name in [
        "warm-success",
        "wake-failure",
        "active-session",
        "quarantine-blocks-refill",
        "fresh-refill",
        "legacy-warm",
    ] {
        let root = tempfile::tempdir()?;
        let bin = root.path().join("bin");
        std::fs::create_dir(&bin)?;
        let current = std::env::current_exe()?;
        for name in ["incus", "sudo", "enginebox-host"] {
            let p = bin.join(name);
            std::fs::write(
                &p,
                format!(
                    "#!/bin/sh\nexec {} --fixture {name} \"$@\"\n",
                    enginebox_core::transport::shell_quote(current.to_str().unwrap())
                ),
            )?;
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755))?;
        }
        let h = host();
        let template =
            json!({"id":"native","label":"Native","source":"ebox-base","snapshot":"prepared"});
        let snapshot = json!({"name":"ebox-base/prepared","created_at":"2026-10-19T00:00:00Z","config":{"user.enginebox.clean-base":"true"},"stateful":false,"type":"virtual-machine","expanded_devices":{"root":{"type":"disk","path":"/","pool":"pool","size":"30GiB","size.state":"4GiB"}}});
        let generation = warm::generation(
            &h,
            &template,
            Resources {
                cpu: 2,
                memory_gib: 2,
                disk_gib: 30,
            },
            &snapshot,
        );
        let vm = json!({"name":"ebox-warm-reserve","project":"enginebox","pool":"pool","status":"Frozen","managed":true,"type":"virtual-machine","cpu":2,"memory_bytes":2*GIB,"disk_bytes":30*GIB,"state_disk_bytes":4*GIB,"template":"native","warm_pool_state":"ready","pool_claim":null,"pool_generation":generation,"pool_size":"recommended","pool_created_at":chrono::Utc::now()});
        let data = json!({"native_helpers":scenario_name!="legacy-warm","snapshot":snapshot,"facts":{"cpu_total":32,"memory_total_bytes":64*GIB,"memory_available_bytes":50*GIB,"disk_free_bytes":500*GIB,"pool_total_bytes":500*GIB,"pool_used_bytes":10*GIB,"instances":[vm]}});
        std::fs::write(root.path().join("state.json"), serde_json::to_vec(&data)?)?;
        let result = Command::new(&current)
            .arg("--scenario")
            .env("EBOX_TEST_ROOT", root.path())
            .env("EBOX_TEST_SCENARIO", scenario_name)
            .env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH")?),
            )
            .output()?;
        ensure!(
            result.status.success(),
            "{scenario_name} failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        println!("test {scenario_name} ... ok");
    }
    Ok(())
}
#[cfg(not(unix))]
fn tests() -> Result<()> {
    println!(
        "Incus process transport fixtures require Unix; pure capability/security tests cover other platforms"
    );
    Ok(())
}
fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let result = if args.first().is_some_and(|a| a == "--fixture") {
        fixture_program(&args[1], &args[2..])
    } else if args.first().is_some_and(|a| a == "--scenario") {
        scenario()
    } else {
        tests()
    };
    if let Err(e) = result {
        eprintln!("{e:#}");
        std::process::exit(1);
    }
}
