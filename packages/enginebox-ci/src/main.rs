//! Administrator-provisioned, one-shot Incus worker. All workflow execution is native GitHub runner execution.
use anyhow::{Context, Result, ensure};
use clap::Parser;
use enginebox_core::{
    Fleet, Resources, capacity, capacity::inventory, operation_lock, state, transport::shell_quote,
};
use enginebox_storage_layerfs::{
    Runtime as LayerRuntime, config::Config as LayerConfig, incus::Target,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::Read,
    path::PathBuf,
    time::{Duration, Instant},
};
mod preparation;

#[derive(Parser)]
#[command(
    version,
    about = "Run one native GitHub Actions runner in a disposable Engine Box CI VM"
)]
struct Args {
    #[arg(long)]
    config: PathBuf,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    schema: u32,
    fleet: PathBuf,
    state_root: PathBuf,
    host_id: String,
    template: String,
    resources: Resources,
    /// Exact official runner version installed in the credential-free template.
    runner_version: String,
    /// Optional Linux-node-local LayerFS adapter and approved per-repository bases.
    layerfs: Option<PathBuf>,
    #[serde(default)]
    projects: BTreeMap<String, PreparedProject>,
    #[serde(default)]
    acceleration: Option<preparation::Profile>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedProject {
    project: String,
    from: Option<String>,
    /// Optional administrator-owned fleet resolver. Credentials remain on the node.
    #[serde(default)]
    resolve: Option<Vec<String>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    schema: u32,
    name: String,
    job_id: u64,
    repository: String,
    jitconfig: String,
    timeout_seconds: u64,
    #[serde(default)]
    preparation: Option<preparation::Admission>,
}
fn validate(request: &Request, config: &Config) -> Result<()> {
    ensure!(
        request.schema == 1 && config.schema == 1 && request.job_id > 0,
        "Unsupported worker request"
    );
    enginebox_core::identifier(&request.name, true)?;
    ensure!(
        request.name.starts_with("ebox-ci-") && uuid::Uuid::parse_str(&request.name[8..]).is_ok(),
        "Use a unique disposable CI name"
    );
    ensure!(
        (60..=21600).contains(&request.timeout_seconds)
            && (1..=65536).contains(&request.jitconfig.len())
            && request
                .jitconfig
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"+/=_-".contains(&b)),
        "Invalid JIT input"
    );
    ensure!(
        config.runner_version.split('.').count() == 3
            && config
                .runner_version
                .split('.')
                .all(|p| p.parse::<u32>().is_ok()),
        "Pin an official runner version"
    );
    config.resources.validate(None)?;
    match (&config.acceleration, &request.preparation) {
        (Some(profile), Some(admission)) => {
            ensure!(
                config.layerfs.is_some(),
                "Accelerated runners require LayerFS storage"
            );
            preparation::validate(profile, admission)?;
        }
        (None, None) => ensure!(
            config.layerfs.is_none(),
            "LayerFS runners require repository-bound acceleration admission"
        ),
        _ => anyhow::bail!("Controller and worker preparation profiles must agree"),
    }
    Ok(())
}
fn run(config: Config, request: Request) -> Result<Value> {
    validate(&request, &config)?;
    state::private_dir(&config.state_root)?;
    let receipts = config.state_root.join("ci-receipts");
    state::private_dir(&receipts)?;
    let receipt_path = receipts.join(format!("{}.json", request.name));
    ensure!(
        !receipt_path.try_exists()?,
        "Worker request already has a receipt; inspect instead of replaying"
    );
    let _lease = state::lock(&receipts.join(format!("{}.lock", request.name)), false)?;
    let fleet = Fleet::load(&config.fleet, &config.state_root)?;
    let host = fleet.host(&config.host_id)?;
    ensure!(
        host.project.starts_with("enginebox-ci-") && host.target == "local",
        "CI worker requires a dedicated enginebox-ci-* project on the local Linux node"
    );
    ensure!(
        host.warm_pools.is_empty(),
        "CI workers use fresh clones, not agent reserves"
    );
    let bx = host.engine_box();
    let started = Instant::now();
    let mut receipt = json!({"schema":1,"name":request.name,"job_id":request.job_id,"repository":request.repository,"status":"preparing","runner_version":config.runner_version,"cleaned":false});
    state::write_json(&receipt_path, &receipt)?;
    let outcome = (|| -> Result<()> {
        if let Some(profile) = &config.acceleration {
            ensure!(
                fleet.template(&host, &config.template)?["manifest_digest"] == profile.image,
                "Preparation image must match the installed registered template manifest"
            );
        }
        let prepared = enginebox_core::preparation::prepare(
            &fleet,
            &host.id,
            "ci",
            &allocation_request(&request.name, &config.template, config.resources),
        )?;
        let uuid = bx.instance(&request.name)?["config"]["volatile.uuid"]
            .as_str()
            .context("Missing clone identity")?
            .to_owned();
        receipt["allocation_id"] = prepared["allocation"]["id"].clone();
        receipt["instance_uuid"] = json!(uuid);
        state::write_json(&receipt_path, &receipt)?;
        let checkout = format!("ci-{}", &request.name[8..]);
        let layer = if let Some(path) = &config.layerfs {
            let project = config
                .projects
                .get(&request.repository)
                .context("No approved LayerFS base for this repository")?;
            let runtime = LayerRuntime::new(LayerConfig::load(path)?)?;
            if let Some(command) = &project.resolve {
                ensure!(
                    !command.is_empty()
                        && PathBuf::from(&command[0]).is_absolute()
                        && project.from.is_none(),
                    "Use an absolute preparation resolver and immutable main"
                );
                let output = enginebox_core::transport::run_process(
                    command,
                    None,
                    Duration::from_secs(1200),
                )?;
                let resolved: Value = serde_json::from_slice(&output)?;
                let profile = config
                    .acceleration
                    .as_ref()
                    .context("Missing acceleration profile")?;
                ensure!(
                    resolved["schema"] == 1
                        && resolved["project"] == project.project
                        && resolved["from"].is_null()
                        && resolved["preparation"]["repository"]
                            .as_str()
                            .and_then(|id| id.parse::<u64>().ok())
                            == Some(profile.repository_id)
                        && resolved["preparation"]["namespace"]
                            == format!("github-installation-{}", profile.installation_id)
                        && resolved["preparation"]["trust"] == "repository-read"
                        && resolved["preparation"]["image"] == profile.image
                        && resolved["preparation"]["pnpm"] == profile.pnpm
                        && resolved["preparation"]["scripts"] == profile.scripts,
                    "Resolved preparation differs from worker admission"
                );
                receipt["preparation_key"] = resolved["preparation"]["key"].clone();
            }
            receipt["checkout"] = json!(checkout);
            state::write_json(&receipt_path, &receipt)?;
            runtime.fork(&project.project, &checkout, project.from.as_deref())?;
            runtime.incus_attach(
                &checkout,
                Target {
                    project: host.project.clone(),
                    instance: request.name.clone(),
                    owner: host.project.clone(),
                    uid: host.guest.uid,
                    gid: host.guest.gid,
                },
            )?;
            Some(runtime)
        } else {
            None
        };
        {
            let _host_lock = operation_lock(&host)?;
            owned(&bx.instance(&request.name)?, &receipt)?;
            let room = capacity(&host, &inventory(&host)?)?;
            ensure!(
                room["cpu_available"].as_u64().unwrap_or(0) >= config.resources.cpu
                    && room["memory_budget_available_bytes"].as_u64().unwrap_or(0)
                        >= config.resources.memory_gib * enginebox_core::GIB,
                "Insufficient capacity to start CI clone"
            );
            bx.call(&["start", &request.name])?;
        }
        enginebox_core::fleet::wait_ready_unlocked(&bx, &request.name)?;
        if let (Some(profile), Some(admission)) = (&config.acceleration, &request.preparation) {
            preparation::bootstrap(
                &bx,
                &request.name,
                &request.repository,
                admission,
                profile,
                request.timeout_seconds,
            )?;
        }
        receipt["status"] = json!("running");
        state::write_json(&receipt_path, &receipt)?;
        // Tokens never enter host command arguments, environment, logs or a reusable image.
        // The official runner requires the opaque JIT value on its own guest argv.
        let script = format!(
            r#"set -eu
umask 077
cd "$HOME"
{setup}
cd ci-runner
test "$(./bin/Runner.Listener --version)" = {version}
IFS= read -r jit
set +e
{guard}timeout --signal=TERM --kill-after=30s {seconds}s ./run.sh --jitconfig "$jit" > runner.log 2>&1
code=$?
unset jit
printf '%s\n' "$code" > exit-code
exit 0
"#,
            version = shell_quote(&config.runner_version),
            setup = if config.acceleration.is_some() {
                preparation::launch_setup()
            } else {
                "test ! -e ci-runner\nfor file in /opt/actions-runner/.runner /opt/actions-runner/.credentials /opt/actions-runner/.credentials_rsaparams; do test ! -e \"$file\"; done\ncp -a /opt/actions-runner ci-runner\n".into()
            },
            guard = if config.acceleration.is_some() {
                "setpriv --no-new-privs "
            } else {
                ""
            },
            seconds = request.timeout_seconds
        );
        let command = bx.exec_args(&request.name, &["bash".into(), "-c".into(), script])?;
        let stopped = std::sync::atomic::AtomicBool::new(false);
        let assignment: Result<Option<Value>> = std::thread::scope(|verify_scope| {
            // Verification runs while the synchronous native runner is alive, with no App credential in the guest.
            let verifier = request.preparation.as_ref().map(|admission| {
                verify_scope.spawn(|| {
                    preparation::verify(
                        &bx,
                        &request.name,
                        &request.repository,
                        admission,
                        &stopped,
                    )
                })
            });
            let outcome = bx.run_input(
                &command,
                format!("{}\n", request.jitconfig).as_bytes(),
                Duration::from_secs(request.timeout_seconds + 90),
            );
            stopped.store(true, std::sync::atomic::Ordering::SeqCst);
            let verified = verifier
                .map(|handle| {
                    handle
                        .join()
                        .map_err(|_| anyhow::anyhow!("Assignment verifier stopped"))
                })
                .transpose()?;
            outcome?;
            verified.transpose()
        });
        // Cleanup still runs for a failed preparation; its error is reported after exact owned deletion.
        if let Ok(Some(value)) = &assignment {
            receipt["assignment"] = value.clone();
        }
        owned(&bx.instance(&request.name)?, &receipt)?;
        // Export private diagnostics before deleting the disposable VM. Never return logs to the API.
        let logs = receipts.join(format!("{}-logs.tar", request.name));
        let log_command = bx.exec_args(
            &request.name,
            &[
                "tar".into(),
                "-cf".into(),
                "-".into(),
                "-C".into(),
                format!("{}/ci-runner", host.guest.home),
                "_diag".into(),
                "runner.log".into(),
                "exit-code".into(),
            ],
        )?;
        let bytes = bx.run(&log_command, Duration::from_secs(30))?;
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(logs)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        bx.call(&["stop", &request.name, "--timeout=60"])?;
        if let Some(runtime) = layer {
            runtime.incus_detach(&checkout)?;
            // This is a PRIVATE job checkpoint, never a reusable source or cache publication.
            runtime.operate(
                &checkout,
                "checkpoint",
                &[],
                120,
                "Private CI result; do not reuse as a base",
            )?;
            runtime.operate(&checkout, "close", &[], 120, "")?;
            receipt["private_checkout_retained"] = json!(true);
        }
        let _host_lock = operation_lock(&host)?;
        let vm = bx.instance(&request.name)?;
        owned(&vm, &receipt)?;
        ensure!(
            vm["status"] == "Stopped" && vm["stateful"] != true,
            "Refusing to delete running or stateful CI clone"
        );
        bx.call(&["delete", &request.name])?;
        receipt["cleaned"] = json!(true);
        assignment?;
        Ok(())
    })();
    receipt["duration_seconds"] = json!(started.elapsed().as_secs_f64());
    receipt["status"] = json!(if outcome.is_ok() {
        "finished"
    } else {
        "uncertain"
    });
    if outcome.is_err() {
        receipt["error"] = json!(
            "Inspect the exact disposable VM, allocation and storage receipts; no automatic replay or deletion after uncertainty."
        );
    }
    state::write_json(&receipt_path, &receipt)?;
    outcome?;
    Ok(receipt)
}
fn allocation_request(name: &str, template: &str, resources: Resources) -> Value {
    json!({"name":name,"template":template,"resources":resources})
}
fn owned(vm: &Value, receipt: &Value) -> Result<()> {
    ensure!(
        receipt["instance_uuid"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
            && receipt["allocation_id"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
            && vm["config"]["volatile.uuid"] == receipt["instance_uuid"]
            && vm["config"]["user.enginebox.allocation.id"] == receipt["allocation_id"]
            && vm["config"]["user.enginebox.pool.state"] == "preparing",
        "Disposable VM identity changed"
    );
    Ok(())
}
fn main() {
    let args = Args::parse();
    let result = (|| -> Result<Value> {
        ensure!(
            cfg!(target_os = "linux"),
            "CI worker requires a local Linux Incus node"
        );
        let config: Config = serde_json::from_value(state::read_json(&args.config)?)?;
        let mut input = vec![];
        std::io::stdin()
            .take(128 * 1024 + 1)
            .read_to_end(&mut input)?;
        ensure!(input.len() <= 128 * 1024, "Oversized CI request");
        run(config, serde_json::from_slice(&input)?)
    })();
    match result {
        Ok(value) => println!("{value}"),
        Err(_) => {
            eprintln!(
                "CI worker failed; inspect its private receipt and disposable environment. No request or credential was logged."
            );
            std::process::exit(1);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn worker_allocation_is_accepted_by_fleet_admission() {
        use enginebox_core::model::{Host, StaticTemplate};
        let temp = tempfile::tempdir().unwrap();
        let resources = Resources {
            cpu: 4,
            memory_gib: 4,
            disk_gib: 20,
        };
        let state_root = temp.path().join("state");
        let fleet = Fleet::from_hosts(
            vec![Host {
                id: "ci-local".into(),
                target: "local".into(),
                project: "enginebox-ci-test".into(),
                templates: vec![StaticTemplate {
                    id: "ci-linux".into(),
                    label: "CI fixture".into(),
                    source: "ebox-fixture-base".into(),
                    snapshot: "prepared-fixture".into(),
                    minimum: Some(resources),
                    recommended: Some(resources),
                }],
                ..Host::default()
            }],
            &state_root,
        )
        .unwrap();
        let request = allocation_request(
            "ebox-ci-0123456789abcdef0123456789abcdef",
            "ci-linux",
            resources,
        );
        fleet
            .validate_operation("ci-local", "create", &request)
            .unwrap();
        let too_small = allocation_request(
            "ebox-ci-0123456789abcdef0123456789abcdef",
            "ci-linux",
            Resources {
                cpu: 1,
                ..resources
            },
        );
        assert!(
            fleet
                .validate_operation("ci-local", "create", &too_small)
                .is_err()
        );
    }
    #[test]
    fn cleanup_is_bound_to_allocation_and_instance_identity() {
        let receipt = json!({"instance_uuid":"fixture-uuid","allocation_id":"fixture-allocation"});
        let mut vm = json!({"config":{"volatile.uuid":"fixture-uuid","user.enginebox.allocation.id":"fixture-allocation","user.enginebox.pool.state":"preparing"}});
        owned(&vm, &receipt).unwrap();
        assert!(
            owned(
                &json!({"config":{"user.enginebox.pool.state":"preparing"}}),
                &json!({})
            )
            .is_err()
        );
        vm["config"]["volatile.uuid"] = json!("replaced");
        assert!(owned(&vm, &receipt).is_err());
    }
}
