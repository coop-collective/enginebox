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
async fn probe(binding: &ManagedHostBinding) -> Result<()> {
    ensure!(
        local_identity()? == binding.machine_id_hash,
        "This registration belongs to a different machine"
    );
    // The pre-existing privileged helper owns Incus discovery. Do not bootstrap
    // or change pools, profiles, projects or the host's other workspaces here.
    let helper = Path::new("/usr/local/bin/enginebox-host");
    let target = helper
        .canonicalize()
        .context("Install the approved Incus host helper first")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
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
    let mut config = if path.exists() {
        let mut c = read_config(path)?;
        ensure!(
            c.id == id && c.controller == origin,
            "Existing registration belongs to another controller or identity"
        );
        ensure!(
            local_identity()? == c.binding.machine_id_hash,
            "Existing registration belongs to another machine"
        );
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
    } else {
        let info = request(&origin, "enroll", code, json!({"kind":"incus-info"}), test).await?;
        ensure!(
            info["id"] == id && info["kind"] == "incus",
            "Unexpected registration response"
        );
        let binding: ManagedHostBinding = serde_json::from_value(info["binding"].clone())?;
        let budget: Budget = serde_json::from_value(info["budget"].clone())?;
        binding.validate()?;
        budget.validate()?;
        ensure!(
            local_identity()? == binding.machine_id_hash,
            "This registration code belongs to another machine"
        );
        Config {
            schema: 1,
            controller: origin,
            id: id.into(),
            token: format!(
                "{id}.{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            ),
            enrolled: false,
            budget,
            binding,
        }
    };
    probe(&config.binding).await?;
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
