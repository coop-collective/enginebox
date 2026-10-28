//! Scoped registration/liveness for a controller-approved Incus host.
//! Execution stays with the existing SSH adapter. This process never accepts
//! workload commands or stops VMs when a connection or credential is lost.
use crate::{
    native_node,
    release::{atomic_json, private_dir, reject_symlink_components},
};
use anyhow::{Context, Result, ensure};
use enginebox_node_protocol::{Budget, HEARTBEAT_SECONDS, ManagedHostBinding, machine_id_hash};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema: u32,
    pub controller: String,
    pub id: String,
    pub token: String,
    pub enrolled: bool,
    pub budget: Budget,
    pub binding: ManagedHostBinding,
}
fn digest(value: &str) -> String {
    hex::encode(Sha256::digest(
        serde_json::to_vec(value).expect("string encodes"),
    ))
}
async fn request(
    controller: &str,
    action: &str,
    token: &str,
    body: Value,
    test: bool,
) -> Result<Value> {
    native_node::post(
        &native_node::client()?,
        &native_node::controller_origin(controller, test)?,
        &format!("/{}/node/{action}", if test { "v1" } else { "api" }),
        token,
        None,
        body,
    )
    .await
}
fn read_config(path: &Path) -> Result<Config> {
    reject_symlink_components(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            std::fs::metadata(path)?.permissions().mode() & 0o077 == 0,
            "Managed node configuration must be owner-only"
        );
    }
    let bytes = std::fs::read(path)?;
    ensure!(
        bytes.len() <= 16384,
        "Managed node configuration is too large"
    );
    let config: Config = serde_json::from_slice(&bytes)?;
    ensure!(
        config.schema == 1 && enginebox_node_protocol::id(&config.id),
        "Invalid managed node configuration"
    );
    config.budget.validate()?;
    config.binding.validate()?;
    let (id, secret) = config
        .token
        .split_once('.')
        .context("Invalid managed node credential")?;
    ensure!(
        id == config.id && enginebox_node_protocol::hash(secret),
        "Invalid managed node credential"
    );
    Ok(config)
}
fn local_identity() -> Result<String> {
    ensure!(
        cfg!(target_os = "linux") && cfg!(target_arch = "x86_64"),
        "Managed Incus registration requires Linux x64"
    );
    machine_id_hash(&std::fs::read_to_string("/etc/machine-id")?)
}
fn fresh_config(controller: &str, id: &str, info: &Value, machine: &str) -> Result<Config> {
    ensure!(
        info["id"] == id && info["kind"] == "incus",
        "Unexpected registration response"
    );
    let binding: ManagedHostBinding = serde_json::from_value(info["binding"].clone())?;
    let budget: Budget = serde_json::from_value(info["budget"].clone())?;
    binding.validate()?;
    budget.validate()?;
    ensure!(
        machine == binding.machine_id_hash,
        "This registration code belongs to another machine"
    );
    Ok(Config {
        schema: 1,
        controller: controller.into(),
        id: id.into(),
        token: format!(
            "{id}.{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        ),
        enrolled: false,
        budget,
        binding,
    })
}
fn archive_previous(parent: &Path, previous: &Config) -> Result<()> {
    let directory = parent.join("retired");
    private_dir(&directory)?;
    let archive = directory.join(format!("{}.json", previous.id));
    reject_symlink_components(&archive)?;
    if archive.exists() {
        ensure!(
            serde_json::to_value(read_config(&archive)?)? == serde_json::to_value(previous)?,
            "Previous registration archive differs; inspect before retrying"
        );
    } else {
        atomic_json(&archive, previous)?;
    }
    Ok(())
}
async fn probe(binding: &ManagedHostBinding) -> Result<()> {
    ensure!(
        local_identity()? == binding.machine_id_hash,
        "This registration belongs to a different machine"
    );
    // The pre-existing privileged helper owns Incus discovery. Do not bootstrap
    // or change pools, profiles, projects or the host's other workspaces here.
    let helper = Path::new("/usr/local/bin/enginebox-host");
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let target = helper
            .canonicalize()
            .context("Install the approved Incus host helper first")?;
        for path in target.ancestors() {
            let meta = std::fs::metadata(path)?;
            ensure!(
                meta.uid() == 0 && meta.permissions().mode() & 0o022 == 0,
                "Host helper must be root owned and immutable to other users"
            );
        }
    }
    let mut child = Command::new(helper)
        .args([
            "inventory",
            "--pool",
            &binding.pool,
            "--project",
            &binding.project,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdout = child.stdout.take().context("Missing inventory output")?;
    let result = tokio::time::timeout(Duration::from_secs(8), async {
        let mut bytes = Vec::new();
        (&mut stdout)
            .take(4 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .await?;
        ensure!(
            bytes.len() <= 4 * 1024 * 1024,
            "Inventory exceeds its limit"
        );
        ensure!(child.wait().await?.success(), "Incus inventory failed");
        let value: Value = serde_json::from_slice(&bytes)?;
        ensure!(
            value["instances"].is_array()
                && value["cpu_total"].is_u64()
                && value["pool_total_bytes"].is_u64(),
            "Incomplete Incus inventory"
        );
        Ok::<_, anyhow::Error>(())
    })
    .await;
    result.context("Incus inventory timed out")??;
    Ok(())
}
pub async fn register(path: &Path, controller: &str, code: &str, test: bool) -> Result<Value> {
    let machine = local_identity()?;
    register_checked(path, controller, code, test, &machine, probe).await
}
async fn register_checked(
    path: &Path,
    controller: &str,
    code: &str,
    test: bool,
    machine: &str,
    verify: impl AsyncFn(&ManagedHostBinding) -> Result<()>,
) -> Result<Value> {
    let origin = native_node::controller_origin(controller, test)?.to_string();
    let (id, secret) = code.split_once('.').context("Invalid registration code")?;
    ensure!(
        enginebox_node_protocol::id(id) && enginebox_node_protocol::hash(secret),
        "Invalid registration code"
    );
    let parent = path.parent().context("Managed node directory required")?;
    private_dir(parent)?;
    reject_symlink_components(path)?;
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(parent.join("register.lock"))?;
    lock.try_lock_exclusive()?;
    let mut previous = None;
    let mut config = if path.exists() {
        let mut c = read_config(path)?;
        ensure!(
            native_node::controller_origin(&c.controller, test)?.to_string() == origin,
            "Existing registration belongs to another controller"
        );
        ensure!(
            machine == c.binding.machine_id_hash,
            "Existing registration belongs to another machine"
        );
        if c.id != id {
            let info = request(
                &origin,
                "enroll",
                code,
                json!({"kind":"incus-replace-info",
                "previous_id":c.id,"previous_secret_hash":digest(&c.token)}),
                test,
            )
            .await?;
            let next = fresh_config(&origin, id, &info, machine)?;
            ensure!(
                info["previous_id"] == c.id
                    && next.binding.host_id == c.binding.host_id
                    && next.binding.machine_id_hash == c.binding.machine_id_hash,
                "Replacement does not match the revoked local registration"
            );
            previous = Some(c);
            next
        } else {
            if !c.enrolled {
                // A consumed code may have expired after its acknowledgement was
                // lost. The already persisted scoped credential can prove that
                // enrollment without issuing or consuming a replacement identity.
                let session = uuid::Uuid::new_v4().to_string();
                if let Ok(hello) = request(
                    &c.controller,
                    "hello",
                    &c.token,
                    json!({"session":session}),
                    test,
                )
                .await
                {
                    ensure!(
                        hello["id"] == c.id
                            && hello["session"] == session
                            && hello["kind"] == "incus"
                            && hello["binding"] == serde_json::to_value(&c.binding)?
                            && hello["budget"] == serde_json::to_value(&c.budget)?,
                        "Recovered registration binding differs"
                    );
                    c.enrolled = true;
                    atomic_json(path, &c)?;
                }
            }
            if c.enrolled {
                return Ok(
                    json!({"id":c.id,"enrolled":true,"existing":true,"host_id":c.binding.host_id,"budget":c.budget}),
                );
            }
            c
        }
    } else {
        let info = request(&origin, "enroll", code, json!({"kind":"incus-info"}), test).await?;
        fresh_config(&origin, id, &info, machine)?
    };
    verify(&config.binding).await?;
    if let Some(previous) = &previous {
        // The old credential is explicitly revoked at the controller. Keep a
        // private copy before replacing it, including pending registrations.
        archive_previous(parent, previous)?;
    }
    // Persist the identity before the first consuming request. A lost response
    // can retry the same credential; it never creates a second registration.
    atomic_json(path, &config)?;
    let enrolled = request(&config.controller,"enroll",code,json!({"kind":"incus","secret_hash":digest(&config.token),"machine_id_hash":config.binding.machine_id_hash}),test).await?;
    ensure!(
        enrolled["id"] == config.id
            && enrolled["enrolled"] == true
            && enrolled["binding"] == serde_json::to_value(&config.binding)?
            && enrolled["budget"] == serde_json::to_value(&config.budget)?,
        "Controller changed the approved registration binding"
    );
    config.enrolled = true;
    atomic_json(path, &config)?;
    Ok(
        json!({"id":config.id,"enrolled":true,"host_id":config.binding.host_id,"budget":config.budget}),
    )
}
async fn session(config: &Config, status: &Path, test: bool) -> Result<()> {
    let session = uuid::Uuid::new_v4().to_string();
    let hello = request(
        &config.controller,
        "hello",
        &config.token,
        json!({"session":session}),
        test,
    )
    .await?;
    ensure!(
        hello["id"] == config.id
            && hello["session"] == session
            && hello["kind"] == "incus"
            && hello["binding"] == serde_json::to_value(&config.binding)?,
        "Controller returned a different managed host identity"
    );
    let epoch = hello["epoch"].as_u64().context("Missing node epoch")?;
    loop {
        let ready = probe(&config.binding).await.is_ok();
        let tools = json!({"backend":"incus","host_id":config.binding.host_id,"binding_digest":config.binding.binding_digest,"host_ready":ready,"worker_running":false});
        let response = request(
            &config.controller,
            "heartbeat",
            &config.token,
            json!({"session":session,"epoch":epoch,"tools":tools}),
            test,
        )
        .await?;
        atomic_json(
            status,
            &json!({"online":true,"id":config.id,"host_id":config.binding.host_id,"ready":ready,"last_seen":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs(),"blocked":response["blocked"]}),
        )?;
        tokio::time::sleep(Duration::from_secs(HEARTBEAT_SECONDS)).await;
    }
}
pub async fn run(path: &Path, test: bool) -> Result<()> {
    let config = read_config(path)?;
    native_node::controller_origin(&config.controller, test)?;
    ensure!(config.enrolled, "Managed host is not registered");
    ensure!(
        local_identity()? == config.binding.machine_id_hash,
        "Managed host identity changed"
    );
    let parent = path.parent().context("Managed node directory required")?;
    private_dir(parent)?;
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(parent.join("worker.lock"))?;
    lock.try_lock_exclusive()?;
    let status = parent.join("status.json");
    loop {
        if session(&config, &status, test).await.is_err() {
            atomic_json(
                &status,
                &json!({"online":false,"id":config.id,"host_id":config.binding.host_id,"error":"Managed node connection interrupted; registration or controller needs attention"}),
            )?;
            eprintln!(
                "Managed node connection interrupted; retrying without changing existing workspaces."
            );
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

#[cfg(test)]
mod registration_tests {
    use super::*;
    use axum::{extract::Request, middleware::Next, response::IntoResponse};
    use enginebox_controller::{Controller, contributors::Contributors, router};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[tokio::test]
    async fn revoked_replacement_archives_identity_and_recovers_lost_acknowledgment() {
        for old_enrolled in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap();
            let state = root.join("state");
            let config = root.join("fleet.toml");
            std::fs::write(&config, "schema_version=1\n[[hosts]]\nid='host'\ntarget='unused.invalid'\nmanaged_registration=true\n").unwrap();
            let host = serde_json::from_value(
                json!({"id":"host","label":"Host","target":"unused.invalid","managed_registration":true}),
            )
            .unwrap();
            let registry = Contributors::open(&state).unwrap();
            let budget = Budget {
                cpu: 2,
                memory_gib: 4,
                disk_gib: 20,
            };
            let machine = "b".repeat(64);
            let old = registry
                .issue_host(&host, machine.clone(), budget.clone())
                .unwrap();
            let old_id = old["id"].as_str().unwrap();
            let old_info = registry.host_info(old["code"].as_str().unwrap()).unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let controller = Controller::new(&config, &state, "x".repeat(32)).unwrap();
            let path = root.join("private/host.json");
            private_dir(path.parent().unwrap()).unwrap();
            // The fixture deliberately persists an origin without a trailing
            // slash, which must compare equal to its canonical URL form.
            let mut old_config = fresh_config(&origin, old_id, &old_info, &machine).unwrap();
            if old_enrolled {
                registry.enroll_host(old["code"].as_str().unwrap(), &json!({"kind":"incus","secret_hash":digest(&old_config.token),"machine_id_hash":machine})).unwrap();
                old_config.enrolled = true;
            }
            atomic_json(&path, &old_config).unwrap();
            let before = std::fs::read(&path).unwrap();
            // The owner explicitly retires either a complete registration or
            // one left pending before its enrollment request was consumed.
            registry.revoke(old_id).unwrap();
            let next = registry.issue_host(&host, machine.clone(), budget).unwrap();
            let code = next["code"].as_str().unwrap();
            controller.activate().unwrap();
            let calls = Arc::new(AtomicUsize::new(0));
            let app = router(controller.clone()).layer(axum::middleware::from_fn(
                move |request: Request, next: Next| {
                    let calls = calls.clone();
                    async move {
                        let enrollment = request.uri().path() == "/v1/node/enroll";
                        let response = next.run(request).await;
                        // Replacement-info, then the deliberately rejected probe's
                        // replacement-info, then consume. Lose only its response.
                        if enrollment
                            && response.status().is_success()
                            && calls.fetch_add(1, Ordering::SeqCst) == 2
                        {
                            return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                        }
                        response
                    }
                },
            ));
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let other_controller = origin.replace("127.0.0.1", "localhost");
            assert!(
                register_checked(
                    &path,
                    &other_controller,
                    code,
                    true,
                    &machine,
                    async |_| Ok(())
                )
                .await
                .is_err()
            );
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert!(
                register_checked(
                    &path,
                    &origin,
                    code,
                    true,
                    &machine,
                    async |_| anyhow::bail!("runtime not ready")
                )
                .await
                .is_err()
            );
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert!(!path.parent().unwrap().join("retired").exists());
            // Approved replacement archives first, then persists the new
            // identity before consuming its one-time code.
            assert!(
                register_checked(&path, &origin, code, true, &machine, async |_| Ok(()))
                    .await
                    .is_err()
            );
            let pending = read_config(&path).unwrap();
            assert_eq!(pending.id, next["id"]);
            assert!(!pending.enrolled);
            let retired_path = path
                .parent()
                .unwrap()
                .join("retired")
                .join(format!("{old_id}.json"));
            assert_eq!(
                serde_json::to_value(read_config(&retired_path).unwrap()).unwrap(),
                serde_json::to_value(&old_config).unwrap()
            );
            let invalid_code = format!("{}.{}", pending.id, "0".repeat(64));
            let recovered =
                register_checked(&path, &origin, &invalid_code, true, &machine, async |_| {
                    anyhow::bail!("recovery must not rerun provisioning")
                })
                .await
                .unwrap();
            assert_eq!(recovered["enrolled"], true);
            let complete = read_config(&path).unwrap();
            assert_eq!(complete.token, pending.token);
            let complete_bytes = std::fs::read(&path).unwrap();
            register_checked(&path, &origin, code, true, &machine, async |_| Ok(()))
                .await
                .unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), complete_bytes);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(retired_path)
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o077,
                    0
                );
            }
            server.abort();
        }
    }
}
