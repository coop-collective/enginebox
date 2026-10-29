//! Durable stopped allocation prerequisite for future checkout attachment.
//! This deliberately has no start, release, deletion or retry operation.
use crate::{
    Fleet,
    fleet::{CopyMode, required},
    node_protocol, state,
    transport::{host_run, operation_lock_wait},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};

fn receipt_path(fleet: &Fleet, host: &str, name: &str) -> Result<PathBuf> {
    Ok(fleet
        .registry
        .root
        .parent()
        .context("Missing controller state root")?
        .join("allocations")
        .join(format!("{}.json", state::digest(&json!([host, name])))))
}

/// Copy a credential-free template into a stopped, reserved VM. The caller must
/// authorize the repository alias; this function never imports or attaches it.
/// CPU/memory admission is checked now and MUST be checked again before start.
pub fn prepare(
    fleet: &Fleet,
    host_id: &str,
    storage_project: &str,
    params: &Value,
) -> Result<Value> {
    fleet.assert_admission(host_id)?;
    crate::identifier(storage_project, false)?;
    fleet.validate_operation(host_id, "create", params)?;
    let host = fleet.host(host_id)?;
    let _controller = fleet.controller_lock(host_id)?;
    let _lease = operation_lock_wait(&host, Duration::from_secs(30))?;
    fleet.assert_admission(host_id)?;
    // Use the same executable resolution as actual inventory/host operations.
    let protocol: Value = serde_json::from_slice(&host_run(
        &host,
        &["enginebox-host".into(), "protocol".into()],
        None,
        Duration::from_secs(30),
    )?)?;
    node_protocol::require_host(&protocol)?;
    let name = required(params, "name")?;
    let path = receipt_path(fleet, host_id, name)?;
    ensure!(
        !path.try_exists()?,
        "Allocation already has a durable receipt; inspect it and its VM instead of retrying or reusing the name"
    );
    let mut receipt = json!({"schema_version":1,"id":uuid::Uuid::new_v4().to_string(),"host_id":host_id,"host_binding_digest":state::digest(&host.binding()),"project":host.project,"name":name,"storage_project":storage_project,"request":params,"node_protocol":protocol,"status":"preparing","created_at":chrono::Utc::now(),"automatic_replay":false});
    state::write_json(&path, &receipt)?;
    let result = fleet.copy_locked(
        &host,
        name,
        required(params, "template")?,
        params.get("size").and_then(Value::as_str),
        params.get("resources"),
        CopyMode::Reserved(&receipt),
    );
    match result {
        Ok(result) => {
            receipt["status"] = json!("prepared");
            receipt["result"] = result.clone();
            state::write_json(&path, &receipt)?;
            Ok(
                json!({"allocation":receipt,"workspace":result,"next":"Attachment finalization is not implemented; VM remains reserved and cannot receive agent access"}),
            )
        }
        Err(error) => {
            receipt["status"] = json!("uncertain");
            receipt["error"] = json!(
                "Preparation did not complete; inspect the exact receipt and node resources. No cleanup or retry was attempted."
            );
            state::write_json(&path, &receipt)?;
            Err(error).context("Stopped allocation retained for inspection; do not reuse its name")
        }
    }
}

pub fn inspect(fleet: &Fleet, host_id: &str, name: &str) -> Result<Value> {
    crate::identifier(name, true)?;
    let host = fleet.host(host_id)?;
    let receipt = state::read_json(&receipt_path(fleet, host_id, name)?)?;
    ensure!(
        receipt["schema_version"] == 1
            && receipt["host_id"] == host_id
            && receipt["name"] == name
            && receipt["host_binding_digest"] == state::digest(&host.binding()),
        "Allocation receipt does not match this fleet registration"
    );
    Ok(receipt)
}
