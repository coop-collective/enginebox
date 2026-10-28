use crate::{
    GIB,
    model::{Host, Resources, sizes},
    transport::{host_run, run_process},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};
pub fn number(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0)
}
pub fn resource_bytes(v: &Value, fallback: u64) -> u64 {
    if let Some(n) = v.as_u64() {
        return n;
    }
    let Some(s) = v.as_str() else { return fallback };
    let digits = s.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return fallback;
    }
    let factor = match &s[digits..] {
        "" => 1,
        "KB" => 1000,
        "MB" => 1000_u64.pow(2),
        "GB" => 1000_u64.pow(3),
        "TB" => 1000_u64.pow(4),
        "KiB" => 1024,
        "MiB" => 1024_u64.pow(2),
        "GiB" => GIB,
        "TiB" => 1024_u64.pow(4),
        _ => return fallback,
    };
    s[..digits]
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(factor))
        .unwrap_or(fallback)
}
pub fn cpu_count(v: &Value, fallback: u64) -> u64 {
    let value = v
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| v.to_string());
    if let Ok(n) = value.parse::<u64>() {
        return if n > 0 { n } else { fallback };
    }
    let mut cpus = BTreeSet::new();
    for part in value.split(',') {
        let mut bounds = part.split('-');
        let Some(lo) = bounds.next().and_then(|s| s.parse::<u64>().ok()) else {
            return fallback;
        };
        let hi = match bounds.next() {
            Some(s) => match s.parse::<u64>() {
                Ok(n) => n,
                Err(_) => return fallback,
            },
            None => lo,
        };
        if bounds.next().is_some() || hi < lo || hi - lo > 4096 {
            return fallback;
        }
        for n in lo..=hi {
            cpus.insert(n);
        }
        if cpus.len() > 4096 {
            return fallback;
        }
    }
    if cpus.is_empty() {
        fallback
    } else {
        cpus.len() as u64
    }
}
pub fn capacity(host: &Host, facts: &Value) -> Result<Value> {
    let items = facts["instances"]
        .as_array()
        .context("Host inventory lacks instances")?;
    for key in [
        "cpu_total",
        "memory_total_bytes",
        "memory_available_bytes",
        "disk_free_bytes",
        "pool_total_bytes",
        "pool_used_bytes",
    ] {
        ensure!(facts[key].is_u64(), "Host inventory lacks {key}");
    }
    // Stopped instances hold only disk. Frozen instances hold their RAM but
    // no CPU time. Running, transitional and unknown states hold both.
    let holds_memory = |v: &&Value| v["status"] != "Stopped";
    let holds_cpu = |v: &&Value| v["status"] != "Stopped" && v["status"] != "Frozen";
    let allocated_cpu = items
        .iter()
        .filter(holds_cpu)
        .map(|v| number(&v["cpu"]))
        .sum::<u64>();
    let allocated_memory = items
        .iter()
        .filter(holds_memory)
        .map(|v| number(&v["memory_bytes"]))
        .sum::<u64>();
    let contributed = |v: &&Value| v["managed"] == true || v["enginebox_owned"] == true;
    let contributed_cpu = items
        .iter()
        .filter(holds_cpu)
        .filter(contributed)
        .map(|v| number(&v["cpu"]))
        .sum::<u64>();
    let contributed_mem = items
        .iter()
        .filter(holds_memory)
        .filter(contributed)
        .map(|v| number(&v["memory_bytes"]))
        .sum::<u64>();
    let committed = items
        .iter()
        .filter(|v| v["pool"] == host.pool)
        .map(|v| number(&v["disk_bytes"]) + number(&v["state_disk_bytes"]))
        .sum::<u64>();
    // vCPU limits are ceilings: the physical cores left after the host
    // reserve admit `cpu_overcommit` times as many running vCPUs.
    let overcommit = host.cpu_overcommit();
    let cpu_budget = cpu_budget(
        number(&facts["cpu_total"]).saturating_sub(host.reserve_cpu),
        overcommit,
    );
    let mut cpu_room = cpu_budget.saturating_sub(allocated_cpu);
    let mut memory_room = number(&facts["memory_total_bytes"])
        .saturating_sub(host.reserve_memory_gib * GIB)
        .saturating_sub(allocated_memory)
        .min(
            number(&facts["memory_available_bytes"]).saturating_sub(host.reserve_memory_gib * GIB),
        );
    if let (Some(c), Some(m)) = (host.contribution_cpu, host.contribution_memory_gib) {
        cpu_room = cpu_room.min(c.saturating_sub(contributed_cpu));
        memory_room = memory_room.min((m * GIB).saturating_sub(contributed_mem));
    }
    let pool_free =
        number(&facts["pool_total_bytes"]).saturating_sub(number(&facts["pool_used_bytes"]));
    let disk_room = number(&facts["disk_free_bytes"])
        .min(pool_free)
        .min(number(&facts["pool_total_bytes"]).saturating_sub(committed))
        .saturating_sub(host.reserve_disk_gib * GIB);
    let count = items
        .iter()
        .filter(|v| v["managed"] == true && v["project"] == host.project)
        .count() as u64;
    let slots = host.max_instances.saturating_sub(count);
    let mut additional = serde_json::Map::new();
    for (name, size) in sizes().as_object().unwrap() {
        additional.insert(
            name.clone(),
            json!(
                slots
                    .min(cpu_room / number(&size["cpu"]))
                    .min(memory_room / (number(&size["memory_gib"]) * GIB))
                    .min(disk_room / ((host.disk_gib + number(&size["memory_gib"]) + 2) * GIB))
            ),
        );
    }
    Ok(
        json!({"cpu_total":facts["cpu_total"],"memory_total_bytes":facts["memory_total_bytes"],"memory_available_bytes":facts["memory_available_bytes"],"disk_free_bytes":facts["disk_free_bytes"],"pool_total_bytes":facts["pool_total_bytes"],"pool_free_bytes":pool_free,"reserve_cpu":host.reserve_cpu,"cpu_overcommit":overcommit,"cpu_budget":cpu_budget,"reserve_memory_bytes":host.reserve_memory_gib*GIB,"contribution_cpu":host.contribution_cpu,"contribution_memory_bytes":host.contribution_memory_gib.map(|m|m*GIB),"contribution_allocated_cpu":contributed_cpu,"contribution_allocated_memory_bytes":contributed_mem,"allocated_cpu":allocated_cpu,"allocated_memory_bytes":allocated_memory,"disk_committed_bytes":committed,"max_additional":additional,"cpu_available":cpu_room,"memory_budget_available_bytes":memory_room,"disk_budget_available_bytes":disk_room,"instance_slots_available":slots}),
    )
}
/// Running vCPUs admitted on `physical` cores at `ratio` overcommit.
pub fn cpu_budget(physical: u64, ratio: f64) -> u64 {
    let ratio = if ratio.is_finite() {
        ratio.max(1.0)
    } else {
        1.0
    };
    // Bounded inputs (cores <= 4096 * ratio <= 64) are exact in f64.
    (physical as f64 * ratio).floor() as u64
}
/// The first resource that stops a new instance of `r`, or `None` when it fits.
pub fn shortage(host: &Host, facts: &Value, r: Resources) -> Result<Option<String>> {
    let c = capacity(host, facts)?;
    let gib = |b: u64| b / GIB;
    Ok(if number(&c["instance_slots_available"]) < 1 {
        Some(format!(
            "no instance slots (max_instances {})",
            host.max_instances
        ))
    } else if number(&c["cpu_available"]) < r.cpu {
        Some(format!(
            "CPU: {} vCPU requested, {} of {} running vCPU budget free (overcommit {})",
            r.cpu,
            number(&c["cpu_available"]),
            number(&c["cpu_budget"]),
            host.cpu_overcommit()
        ))
    } else if number(&c["memory_budget_available_bytes"]) < r.memory_gib * GIB {
        Some(format!(
            "memory: {} GiB requested, {} GiB free after the {} GiB reserve",
            r.memory_gib,
            gib(number(&c["memory_budget_available_bytes"])),
            host.reserve_memory_gib
        ))
    } else if number(&c["disk_budget_available_bytes"]) < (r.disk_gib + r.memory_gib + 2) * GIB {
        Some(format!(
            "disk: {} GiB requested, {} GiB free after the {} GiB reserve",
            r.disk_gib + r.memory_gib + 2,
            gib(number(&c["disk_budget_available_bytes"])),
            host.reserve_disk_gib
        ))
    } else {
        None
    })
}
pub fn resource_fits(host: &Host, facts: &Value, r: Resources) -> Result<bool> {
    Ok(shortage(host, facts, r)?.is_none())
}
pub fn inventory(host: &Host) -> Result<Value> {
    let bytes = host_run(
        host,
        &[
            "enginebox-host".into(),
            "inventory".into(),
            "--pool".into(),
            host.pool.clone(),
            "--project".into(),
            host.project.clone(),
        ],
        None,
        Duration::from_secs(90),
    )?;
    Ok(serde_json::from_slice(&bytes)?)
}
pub fn incus_query(path: &str) -> Result<Value> {
    let argv = vec!["incus".into(), "query".into(), path.into()];
    Ok(serde_json::from_slice(&run_process(
        &argv,
        None,
        Duration::from_secs(30),
    )?)?)
}
pub fn memory() -> Result<(u64, u64)> {
    let text = std::fs::read_to_string("/proc/meminfo")?;
    let mut values = BTreeMap::new();
    for line in text.lines() {
        if let Some((key, val)) = line.split_once(':')
            && let Some(n) = val
                .split_whitespace()
                .next()
                .and_then(|s| s.parse::<u64>().ok())
        {
            values.insert(key, n * 1024);
        }
    }
    Ok((
        *values.get("MemTotal").context("MemTotal unavailable")?,
        *values
            .get("MemAvailable")
            .context("MemAvailable unavailable")?,
    ))
}
pub fn pool_metadata(vm: &Value, cfg: &Value) -> Value {
    let mut state = cfg["user.enginebox.pool.state"].clone();
    let mut claim = Value::Null;
    if let Some(d) = vm["description"]
        .as_str()
        .and_then(|s| s.strip_prefix("enginebox-pool:v1:"))
    {
        match serde_json::from_str::<Value>(d) {
            Ok(v)
                if v["state"] == "quarantined"
                    || (v["state"] == "claimed"
                        && v["workspace_name"]
                            .as_str()
                            .is_some_and(|s| crate::identifier(s, true).is_ok())) =>
            {
                state = v["state"].clone();
                claim = v;
            }
            _ => {
                state = json!("quarantined");
                claim = json!({"state":"quarantined","reason":"Invalid reservation; inspect before reuse"});
            }
        }
    }
    if cfg["user.enginebox.workspace"]
        .as_str()
        .is_some_and(|s| !s.is_empty())
        && claim.is_null()
    {
        state = json!("claimed");
        claim = json!({"state":"claimed","workspace_name":cfg["user.enginebox.workspace"]});
    }
    json!({"warm_pool_state":state,"pool_claim":claim,"workspace_name":claim.get("workspace_name").unwrap_or(&cfg["user.enginebox.workspace"]),"pool_generation":cfg["user.enginebox.pool.generation"],"pool_size":cfg["user.enginebox.pool.size"],"pool_created_at":cfg["user.enginebox.pool.created_at"]})
}
pub fn local_inventory(pool: &str, _project: &str) -> Result<Value> {
    crate::identifier(pool, false)?;
    let cpus = std::thread::available_parallelism()?.get() as u64;
    let (total, available) = memory()?;
    let data = incus_query("/1.0/instances?recursion=1&all-projects=true")?;
    let mut items = Vec::new();
    let mut leases: BTreeMap<(String, String), Value> = BTreeMap::new();
    for vm in data.as_array().context("Invalid Incus instances")? {
        let cfg = vm.get("expanded_config").unwrap_or(&vm["config"]);
        let devices = vm["expanded_devices"]
            .as_object()
            .context("Incus metadata lacks expanded devices")?;
        let project = vm["project"].as_str().unwrap_or("default");
        let root = devices
            .values()
            .find(|v| v["type"] == "disk" && v["path"] == "/")
            .cloned()
            .unwrap_or(Value::Null);
        let mut ips = BTreeSet::new();
        for (name, d) in devices {
            if d["type"] != "nic" {
                continue;
            }
            let network = d["network"].as_str().or_else(|| {
                if d["nictype"] == "bridged" {
                    d["parent"].as_str()
                } else {
                    None
                }
            });
            let Some(network) = network else { continue };
            let key = (project.to_owned(), network.to_owned());
            let lease = leases.entry(key).or_insert_with(|| {
                let p =
                    url::form_urlencoded::byte_serialize(project.as_bytes()).collect::<String>();
                let n =
                    url::form_urlencoded::byte_serialize(network.as_bytes()).collect::<String>();
                incus_query(&format!("/1.0/networks/{n}/leases?project={p}")).unwrap_or(json!([]))
            });
            let mac = d["hwaddr"]
                .as_str()
                .or_else(|| cfg[format!("volatile.{name}.hwaddr")].as_str());
            if let Some(mac) = mac {
                for lease in lease.as_array().into_iter().flatten() {
                    if lease["hwaddr"]
                        .as_str()
                        .is_some_and(|s| s.eq_ignore_ascii_case(mac))
                        && let Some(ip) = lease["address"].as_str()
                        && ip.parse::<std::net::Ipv4Addr>().is_ok()
                    {
                        ips.insert(ip.to_string());
                    }
                }
            }
        }
        let mut item = json!({"name":vm["name"],"project":project,"status":vm["status"],"type":vm["type"],"stateful":vm["stateful"].as_bool().unwrap_or(false),"cpu":cpu_count(&cfg["limits.cpu"],cpus),"memory_bytes":resource_bytes(&cfg["limits.memory"],total),"memory_usage_bytes":null,"pool":root["pool"],"disk_bytes":resource_bytes(&root["size"],30*GIB),"state_disk_bytes":resource_bytes(&root["size.state"],0),"managed":cfg["user.enginebox.fleet"]=="true","enginebox_owned":cfg["user.enginebox.owner"]==project,"template":cfg["user.enginebox.template"],"ipv4":ips});
        item.as_object_mut()
            .unwrap()
            .extend(pool_metadata(vm, cfg).as_object().unwrap().clone());
        items.push(item);
    }
    let space = incus_query(&format!("/1.0/storage-pools/{pool}/resources"))?["space"].clone();
    Ok(
        json!({"cpu_total":cpus,"memory_total_bytes":total,"memory_available_bytes":available,"disk_free_bytes":fs2::available_space("/var/lib/incus")?,"pool_total_bytes":space["total"],"pool_used_bytes":space["used"],"instances":items}),
    )
}
