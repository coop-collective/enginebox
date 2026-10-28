use crate::{
    Fleet, GIB,
    capacity::{inventory, number, resource_fits},
    fleet::{check_name, required, template_snapshot, wait_ready},
    model::{Host, Resources, preset},
    state,
    transport::operation_lock,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::time::Instant;
const PREFIX: &str = "enginebox-pool:v1:";
pub fn reservation(state: &str, fields: Value) -> Result<String> {
    ensure!(
        ["claimed", "quarantined"].contains(&state),
        "Invalid reservation state"
    );
    let mut v = fields;
    ensure!(v.is_object(), "Reservation fields must be an object");
    v["state"] = json!(state);
    Ok(format!("{PREFIX}{}", serde_json::to_string(&v)?))
}
pub fn generation(h: &Host, t: &Value, r: Resources, snapshot: &Value) -> String {
    let mut s = serde_json::Map::new();
    for k in [
        "name",
        "created_at",
        "architecture",
        "type",
        "stateful",
        "ephemeral",
        "config",
        "devices",
        "expanded_config",
        "expanded_devices",
        "profiles",
    ] {
        s.insert(k.into(), snapshot[k].clone());
    }
    let mut template = serde_json::Map::new();
    for k in ["id", "source", "snapshot", "revision", "manifest_digest"] {
        template.insert(k.into(), t[k].clone());
    }
    state::digest_ascii(
        &json!({"template":template,"resources":r,"snapshot":s,"pool":h.pool,"project":h.project,"network":h.network,"guest":h.guest}),
    )
}
pub fn eligible(vm: &Value, h: &Host, id: &str, r: Resources, expected: &str) -> bool {
    vm["project"] == h.project
        && vm["pool"] == h.pool
        && vm["managed"] == true
        && vm["warm_pool_state"] == "ready"
        && vm["pool_claim"].is_null()
        && vm["status"] == "Frozen"
        && vm["template"] == id
        && vm["pool_generation"] == expected
        && number(&vm["cpu"]) == r.cpu
        && number(&vm["memory_bytes"]) == r.memory_gib * GIB
        && number(&vm["disk_bytes"]) == r.disk_gib * GIB
        && number(&vm["state_disk_bytes"]) >= (r.memory_gib + 2) * GIB
}
fn error_path(f: &Fleet, h: &Host, id: &str, size: &str) -> std::path::PathBuf {
    f.registry.root.join("sync").join(format!(
        "warm-{}.json",
        state::digest(&json!([h.id, id, size]))
    ))
}
fn record_error(f: &Fleet, h: &Host, id: &str, size: &str, error: Option<&str>) -> Result<()> {
    state::write_json(
        &error_path(f, h, id, size),
        &json!({"error":error.map(|s|s.chars().take(500).collect::<String>()),"updated_at":chrono::Utc::now()}),
    )
}
pub fn summary(f: &Fleet, h: &Host, facts: &Value) -> Value {
    let mut records = Vec::new();
    for p in &h.warm_pools {
        let mut v =
            json!({"template":p.template,"size":p.size,"target":p.ready,"ready":0,"preparing":0});
        let result = (|| -> Result<()> {
            let t = f.template(h, &p.template)?;
            let r = preset(h, &t, Some(&p.size), None)?;
            let s = template_snapshot(h, &t)?;
            let g = generation(h, &t, r, &s);
            let items = facts["instances"].as_array().context("Invalid inventory")?;
            v["resources"] = json!(r);
            v["ready"] = json!(
                items
                    .iter()
                    .filter(|i| eligible(i, h, &p.template, r, &g))
                    .count()
            );
            v["preparing"] = json!(
                items
                    .iter()
                    .filter(|i| i["project"] == h.project
                        && i["pool_generation"] == g
                        && i["pool_claim"].is_null()
                        && i["warm_pool_state"] == "preparing")
                    .count()
            );
            if let Ok(error) = state::read_json(&error_path(f, h, &p.template, &p.size))
                && !error["error"].is_null()
            {
                v["error"] = error["error"].clone();
            }
            Ok(())
        })();
        if let Err(e) = result {
            v["error"] = json!(e.to_string().chars().take(500).collect::<String>());
        }
        records.push(v);
    }
    json!(records)
}
pub fn allocate(
    f: &Fleet,
    h: &Host,
    name: &str,
    id: &str,
    size: Option<&str>,
    custom: Option<&Value>,
) -> Result<Value> {
    f.assert_admission(&h.id)?;
    let facts = inventory(h)?;
    check_name(h, name, &facts)?;
    let t = f.template(h, id)?;
    let r = preset(h, &t, size, custom)?;
    let snapshot = template_snapshot(h, &t)?;
    let expected = generation(h, &t, r, &snapshot);
    let slot = facts["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|vm| eligible(vm, h, id, r, &expected));
    let Some(slot) = slot else {
        let mut v = f.create_locked(h, name, id, size, custom, None)?;
        v["workspace_name"] = json!(name);
        v["startup"] = json!("cold");
        return Ok(v);
    };
    let actual = required(slot, "name")?;
    let bx = h.engine_box();
    let started = Instant::now();
    let description = reservation(
        "claimed",
        json!({"workspace_name":name,"job_id":uuid::Uuid::new_v4().simple().to_string(),"generation":expected,"claimed_at":chrono::Utc::now()}),
    )?;
    // First-class description does not contact frozen guest. This irrevocable durable claim precedes wake.
    bx.call(&[
        "config",
        "set",
        actual,
        "--property",
        &format!("description={description}"),
    ])?;
    let result = (|| -> Result<Value> {
        bx.lifecycle(actual, "resume")?;
        bx.call(&[
            "config",
            "set",
            actual,
            "user.enginebox.pool.state=claimed",
            &format!("user.enginebox.workspace={name}"),
        ])?;
        wait_ready(&bx, actual)
    })();
    let readiness=result.with_context(||format!("Workspace reserved as {actual}; inspect and resume that instance. It will never return to the pool"))?;
    Ok(
        json!({"name":actual,"workspace_name":name,"status":"Running","startup":"warm","copy_seconds":0,"ready_seconds":started.elapsed().as_secs_f64(),"readiness":readiness}),
    )
}
fn quarantine(h: &Host, name: &str, reason: &str) -> Result<()> {
    h.engine_box().call(&[
        "config",
        "set",
        name,
        "--property",
        &format!(
            "description={}",
            reservation("quarantined", json!({"reason":reason}))?
        ),
    ])?;
    Ok(())
}
pub fn reconcile_once(f: &Fleet, h: &Host) -> Result<Value> {
    f.assert_admission(&h.id)?;
    if h.warm_pools.is_empty() {
        return Ok(Value::Null);
    }
    let mut slot = None;
    {
        let _controller = f.controller_lock(&h.id)?;
        let _lock = operation_lock(h)?;
        f.assert_admission(&h.id)?;
        let mut facts = inventory(h)?;
        for p in &h.warm_pools {
            if p.ready == 0 {
                record_error(f, h, &p.template, &p.size, None)?;
                continue;
            }
            let prepared = (|| -> Result<(Resources, String)> {
                let t = f.template(h, &p.template)?;
                let r = preset(h, &t, Some(&p.size), None)?;
                let s = template_snapshot(h, &t)?;
                Ok((r, generation(h, &t, r, &s)))
            })();
            let (r, expected) = match prepared {
                Ok(v) => v,
                Err(e) => {
                    record_error(f, h, &p.template, &p.size, Some(&e.to_string()))?;
                    continue;
                }
            };
            let members = facts["instances"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .filter(|vm| {
                    vm["project"] == h.project
                        && vm["managed"] == true
                        && vm["pool"] == h.pool
                        && vm["template"] == p.template
                        && vm["pool_size"] == p.size
                        && ["ready", "preparing", "quarantined"]
                            .contains(&vm["warm_pool_state"].as_str().unwrap_or(""))
                })
                .collect::<Vec<_>>();
            let mut ready = 0;
            let mut preparing = 0;
            let mut blocked = false;
            for vm in members {
                if vm["warm_pool_state"] == "quarantined" && vm["pool_generation"] == expected {
                    blocked = true;
                }
                if !vm["pool_claim"].is_null() {
                    continue;
                }
                let state = vm["warm_pool_state"].as_str().unwrap_or("");
                let old = state == "preparing"
                    && vm["pool_created_at"]
                        .as_str()
                        .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
                        .is_none_or(|t| {
                            chrono::Utc::now().signed_duration_since(t).num_seconds() > 600
                        });
                let stale = vm["pool_generation"] != expected
                    || (state == "ready" && vm["status"] != "Frozen")
                    || old;
                if stale && state != "quarantined" {
                    quarantine(
                        h,
                        required(vm, "name")?,
                        "Stale or interrupted unassigned reserve; inspect before removal",
                    )?;
                    vm["warm_pool_state"] = json!("quarantined");
                }
                if vm["pool_generation"] == expected && vm["warm_pool_state"] == "quarantined" {
                    blocked = true;
                }
                if eligible(vm, h, &p.template, r, &expected) {
                    ready += 1;
                }
                if vm["pool_generation"] == expected && vm["warm_pool_state"] == "preparing" {
                    preparing += 1;
                }
            }
            if blocked {
                record_error(
                    f,
                    h,
                    &p.template,
                    &p.size,
                    Some("Unassigned reserve needs inspection; automatic refill paused"),
                )?;
                continue;
            }
            if ready + preparing >= p.ready {
                record_error(f, h, &p.template, &p.size, None)?;
                continue;
            }
            if !resource_fits(h, &facts, r)? {
                record_error(
                    f,
                    h,
                    &p.template,
                    &p.size,
                    Some("Warm refill is waiting for CPU, RAM, disk or instance capacity"),
                )?;
                continue;
            }
            let name = format!(
                "ebox-warm-{}",
                &uuid::Uuid::new_v4().simple().to_string()[..16]
            );
            let metadata = json!({"state":"preparing","generation":expected,"size":p.size,"created_at":chrono::Utc::now().to_rfc3339()});
            if let Err(e) =
                f.create_locked(h, &name, &p.template, Some(&p.size), None, Some(&metadata))
            {
                record_error(f, h, &p.template, &p.size, Some(&e.to_string()))?;
                return Err(e);
            }
            slot = Some((name, expected, p.clone()));
            break;
        }
    }
    let Some((name, expected, p)) = slot else {
        return Ok(Value::Null);
    };
    // Boot readiness runs outside lifecycle lock; a reserve cannot be allocated while preparing.
    let prepared = (|| -> Result<()> {
        crate::fleet::wait_ready_unlocked(&h.engine_box(), &name)?;
        let _controller = f.controller_lock(&h.id)?;
        let _lock = operation_lock(h)?;
        f.assert_admission(&h.id)?;
        let facts = inventory(h)?;
        let vm = facts["instances"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["project"] == h.project && v["name"] == name)
            .context("Preparing reserve disappeared")?;
        ensure!(
            vm["warm_pool_state"] == "preparing"
                && vm["pool_claim"].is_null()
                && vm["pool_generation"] == expected
                && vm["status"] == "Running",
            "Prepared reserve changed while booting; inspect before reuse"
        );
        h.engine_box()
            .call(&["config", "set", &name, "user.enginebox.pool.state=ready"])?;
        h.engine_box().lifecycle(&name, "pause")?;
        Ok(())
    })();
    match prepared {
        Ok(()) => {
            record_error(f, h, &p.template, &p.size, None)?;
            Ok(json!({"host_id":h.id,"name":name,"warm_pool_state":"ready"}))
        }
        Err(e) => {
            record_error(f, h, &p.template, &p.size, Some(&e.to_string()))?;
            if let Ok(_guard) = operation_lock(h)
                && let Ok(facts) = inventory(h)
                && let Some(vm) = facts["instances"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|v| v["name"] == name && v["project"] == h.project)
                && vm["managed"] == true
                && vm["pool_claim"].is_null()
                && vm["pool_generation"] == expected
            {
                let _ = quarantine(h, &name, "Warm preparation failed; inspect before removal");
            }
            bail!("Warm reserve preparation failed: {e}")
        }
    }
}
