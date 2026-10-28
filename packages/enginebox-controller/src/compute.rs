//! One owner-visible identity for each configured host or enrolled worker.
//!
//! This is a projection of the existing registration authorities, not a second
//! scheduler or credential store. In particular, an Incus host must not also be
//! enrolled as a contributor just to appear in Compute.
use enginebox_core::{GIB, Host, capacity::number};
use serde_json::{Value, json};

fn resources(cpu: u64, memory: u64, disk: u64) -> Value {
    json!({"cpu":cpu,"memory_bytes":memory,"disk_bytes":disk})
}

pub fn hosts(fleet: &Value, configured: &[Host]) -> Vec<Value> {
    let now = chrono::Utc::now().timestamp();
    configured.iter().map(|host| {
        let row = fleet["hosts"].as_array().into_iter().flatten()
            .find(|row| row["id"] == host.id).cloned().unwrap_or(Value::Null);
        let online = row["online"] == true;
        let c = &row["capacity"];
        let records: Vec<_> = fleet["templates"].as_array().into_iter().flatten()
            .filter(|t| t["host_ids"].as_array().is_some_and(|ids| ids.contains(&json!(host.id))))
            .collect();
        let templates: Vec<_> = records.iter().filter_map(|t| t["id"].as_str()).collect();
        let running = row["instances"].as_array().into_iter().flatten()
            .filter(|i| i["managed"] == true && i["status"] == "Running").count();
        // Positive scalar room is not enough: a workspace also needs its
        // template minimum, state disk and a free instance slot.
        let fits: Option<Vec<bool>> = records.iter().map(|t| {
            let minimum = t["host_resources"][&host.id].get("minimum").unwrap_or(&t["minimum"]);
            let cpu = minimum["cpu"].as_u64()?;
            let memory = minimum["memory_gib"].as_u64()?;
            let disk = minimum["disk_gib"].as_u64()?;
            Some(number(&c["cpu_available"]) >= cpu
                && number(&c["memory_budget_available_bytes"]) >= memory.saturating_mul(GIB)
                && number(&c["disk_budget_available_bytes"]) >= disk.saturating_add(memory).saturating_add(2).saturating_mul(GIB))
        }).collect();
        let no_template_fits = match fits {
            Some(fits) if !fits.is_empty() => !fits.into_iter().any(|fits| fits),
            _ => c["max_additional"].as_object().is_some_and(|sizes| !sizes.is_empty() && sizes.values().all(|v| v.as_u64() == Some(0))),
        };
        let limited = online && (no_template_fits || ["cpu_available", "memory_budget_available_bytes", "disk_budget_available_bytes", "instance_slots_available"]
            .iter().any(|key| c[*key].as_u64() == Some(0)));
        let state = if !online { "offline" } else if templates.is_empty() { "preparing" }
            else if limited { "capacity_limited" } else { "ready" };
        let budget = if c.is_object() {
            resources(number(&c["cpu_total"]).saturating_sub(host.reserve_cpu).min(host.contribution_cpu.unwrap_or(u64::MAX)),
                number(&c["memory_total_bytes"]).saturating_sub(host.reserve_memory_gib * GIB).min(host.contribution_memory_gib.map(|v| v * GIB).unwrap_or(u64::MAX)),
                number(&c["pool_total_bytes"]).saturating_sub(host.reserve_disk_gib * GIB))
        } else { Value::Null };
        let available = if c.is_object() {
            resources(number(&c["cpu_available"]), number(&c["memory_budget_available_bytes"]), number(&c["disk_budget_available_bytes"]))
        } else { Value::Null };
        json!({"id":format!("host:{}",host.id),"source_id":host.id,"label":host.label,
            "backend":"incus","platform":"linux","registered":true,"online":online,
            "registration":{"method":"controller_config"},
            "runtime":{"state":state,"running_workspaces":if online {json!(running)} else {Value::Null},"active_job":null,"last_seen":if online {json!(now)} else {Value::Null}},
            "resources":{"budget":budget,"available":available},"templates":templates,
            "capabilities":{"vm_workspaces":true,"template_workspaces":false,"ci_jobs":false},
            "management":{"revoke":false,"reconcile":false,"limits":"controller_config"},
            "error":row.get("error").cloned().unwrap_or(Value::Null)})
    }).collect()
}

pub fn workers(catalog: &Value) -> Vec<Value> {
    catalog["nodes"].as_array().into_iter().flatten()
        .filter(|n| n["enrolled"] == true && n["revoked"] != true)
        .map(|n| {
            let online = n["online"] == true;
            let blocked = n["blocked"] == true;
            let tools = &n["tools"];
            let workspace = tools["workspace"] == true && n["workloads"]["agents"] == true;
            let ci = tools["ci"] == true && n["workloads"]["ci"] == true;
            let busy = !n["active_job"].is_null() || tools["worker_running"] == true;
            let state = if blocked { "needs_attention" } else if !online { "offline" }
                else if busy { "busy" } else if workspace || ci { "ready" } else { "preparing" };
            let budget = &n["budget"];
            json!({"id":format!("worker:{}",n["id"].as_str().unwrap_or_default()),"source_id":n["id"],"label":n["label"],
                "backend":tools["backend"].as_str().unwrap_or("unknown"),
                "platform":if tools["backend"] == "wsl2-layerfs" {"windows"} else {n["os"].as_str().unwrap_or("unknown")},
                "registered":true,"online":online,"registration":{"method":"enrollment"},
                "runtime":{"state":state,"running_workspaces":null,"active_job":n["active_job"],"last_seen":n["last_seen"]},
                "resources":{"budget":resources(number(&budget["cpu"]),number(&budget["memory_gib"])*GIB,number(&budget["disk_gib"])*GIB),"available":null},
                "templates":tools["templates"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>(),
                "capabilities":{"vm_workspaces":false,"template_workspaces":workspace,"ci_jobs":ci},
                "management":{"revoke":true,"reconcile":online && blocked && tools["worker_running"] == false,"limits":"local_installer"},
                "error":null})
        }).collect()
}

pub fn catalog(fleet: &Value, configured: &[Host], contributors: &Value) -> Value {
    let mut nodes = hosts(fleet, configured);
    nodes.extend(workers(contributors));
    nodes.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    json!({"schema_version":1,"nodes":nodes})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_authorities_have_stable_distinct_identities_without_copying_state() {
        let host = Host {
            id: "same".into(),
            label: "Server".into(),
            ..Host::default()
        };
        let fleet = json!({"hosts":[{"id":"same","online":false}],"templates":[]});
        let mut worker = json!({"id":"same","label":"Workstation","enrolled":true,"online":true,
            "budget":{"cpu":4,"memory_gib":8,"disk_gib":50},"workloads":{"agents":true},
            "tools":{"backend":"wsl2-layerfs","workspace":true,"worker_running":false}});
        let pending = json!({"id":"pending","enrolled":false});
        let revoked = json!({"id":"revoked","enrolled":true,"revoked":true,"online":true});
        let value = catalog(
            &fleet,
            std::slice::from_ref(&host),
            &json!({"nodes":[worker,pending,revoked]}),
        );
        assert_eq!(value["nodes"].as_array().unwrap().len(), 2);
        assert_eq!(value["nodes"][0]["id"], "host:same");
        assert_eq!(value["nodes"][0]["registered"], true);
        assert_eq!(value["nodes"][0]["resources"]["budget"], Value::Null);
        assert_eq!(value["nodes"][1]["id"], "worker:same");
        // worker_running describes an owned child process, not node health.
        assert_eq!(value["nodes"][1]["runtime"]["state"], "ready");
        assert_eq!(value["nodes"][1]["resources"]["available"], Value::Null);
        worker["blocked"] = json!(true);
        let blocked = workers(&json!({"nodes":[worker]}));
        assert_eq!(blocked[0]["runtime"]["state"], "needs_attention");
        assert_eq!(blocked[0]["management"]["reconcile"], true);
        assert_eq!(hosts(&json!({}), &[host])[0]["id"], "host:same");
    }

    #[test]
    fn host_allowance_respects_reserves_and_does_not_confuse_disk_free_with_admission() {
        let host = Host {
            id: "metal".into(),
            contribution_cpu: Some(12),
            contribution_memory_gib: Some(24),
            reserve_disk_gib: 10,
            ..Host::default()
        };
        let fleet = json!({"hosts":[{"id":"metal","online":true,"instances":[{"managed":true,"status":"Running"}],
            "capacity":{"cpu_total":32,"memory_total_bytes":64*GIB,"pool_total_bytes":200*GIB,"disk_free_bytes":100*GIB,
                "cpu_available":8,"memory_budget_available_bytes":20*GIB,"disk_budget_available_bytes":0,"instance_slots_available":4}}],
            "templates":[{"id":"native-dev","host_ids":["metal"]}]});
        let value = hosts(&fleet, &[host]);
        let n = &value[0];
        assert_eq!(n["online"], true);
        assert_eq!(n["runtime"]["state"], "capacity_limited");
        assert_eq!(n["runtime"]["running_workspaces"], 1);
        assert_eq!(n["resources"]["budget"], resources(12, 24 * GIB, 190 * GIB));
        assert_eq!(n["resources"]["available"]["disk_bytes"], 0);
        assert_eq!(n["management"]["revoke"], false);
    }

    #[test]
    fn host_readiness_honors_template_minimums_state_disk_and_slots() {
        let host = Host {
            id: "metal".into(),
            ..Host::default()
        };
        let mut fleet = json!({"hosts":[{"id":"metal","online":true,"capacity":{
            "cpu_available":2,"memory_budget_available_bytes":4*GIB,"disk_budget_available_bytes":33*GIB,"instance_slots_available":1}}],
            "templates":[{"id":"native","host_ids":["metal"],"minimum":{"cpu":2,"memory_gib":2,"disk_gib":30}}]});
        let state = |fleet: &Value| {
            hosts(fleet, std::slice::from_ref(&host))[0]["runtime"]["state"].clone()
        };
        // Root disk fits, but its state disk does not.
        assert_eq!(state(&fleet), "capacity_limited");
        fleet["hosts"][0]["capacity"]["disk_budget_available_bytes"] = json!(34 * GIB);
        assert_eq!(state(&fleet), "ready");
        fleet["hosts"][0]["capacity"]["instance_slots_available"] = json!(0);
        assert_eq!(state(&fleet), "capacity_limited");
        fleet["hosts"][0]["capacity"]["instance_slots_available"] = json!(1);
        fleet["templates"][0]["host_resources"]["metal"]["minimum"] =
            json!({"cpu":4,"memory_gib":2,"disk_gib":30});
        assert_eq!(state(&fleet), "capacity_limited");
    }

    #[test]
    fn workload_permission_is_not_readiness_and_stale_workers_cannot_reconcile() {
        let mut worker = json!({"id":"pc","enrolled":true,"online":true,"workloads":{"ci":true,"agents":true},
            "tools":{"backend":"wsl2-layerfs","ci":false,"workspace":false,"worker_running":false}});
        let project = |worker: &Value| workers(&json!({"nodes":[worker]})).remove(0);
        assert_eq!(project(&worker)["runtime"]["state"], "preparing");
        assert_eq!(project(&worker)["capabilities"]["ci_jobs"], false);
        worker["active_job"] = json!("owned-job");
        assert_eq!(project(&worker)["runtime"]["state"], "busy");
        worker["online"] = json!(false);
        assert_eq!(project(&worker)["runtime"]["state"], "offline");
        worker["blocked"] = json!(true);
        assert_eq!(project(&worker)["runtime"]["state"], "needs_attention");
        assert_eq!(project(&worker)["management"]["reconcile"], false);
        worker["tools"] = json!({"templates":["native-dev",null,{"invalid":"report"}]});
        assert_eq!(project(&worker)["backend"], "unknown");
        assert_eq!(project(&worker)["templates"], json!(["native-dev"]));
    }
}
