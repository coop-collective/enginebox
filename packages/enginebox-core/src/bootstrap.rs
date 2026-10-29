//! Inspect-first provisioning. Plans are pure; apply re-plans under the same host lease after every step.
use crate::{
    GIB,
    capacity::{number, resource_bytes},
    model::{GuestConfig, Host, Resources},
    state,
    transport::{EngineBox, ensure_no_external_checkout, host_run, run_process},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
const OWNER: &str = "user.enginebox.owner";
const RECIPE: &str = "user.enginebox.recipe";
const PREPARED: &str = "user.enginebox.prepared";
pub fn defaults() -> Value {
    json!({"schema_version":1,"host":{"transport":"ssh","target":"","data_dir":"/var/lib/enginebox","install_packages":false,"disk_reserve":"5GiB","memory_reserve":"1GiB","admission":{}},"incus":{"project":"enginebox","profile":"enginebox"},"storage":{"mode":"existing","pool":"","size":"120GiB"},"network":{"mode":"existing","name":"","subnet":"10.203.0.1/24"},"workspace":{"name":"ebox-dev","image":"images:ubuntu/24.04/cloud","cpu":4,"memory":"4GiB","disk":"30GiB","state_disk":"8GiB","user":"agent","uid":1001,"gid":1001,"home":"/home/agent","path":"/workspace","ready_timeout":300},"agents":{"install":["codex","cursor","claude"]},"modules":{"desktop":false,"browser":true,"operator_mcp":true,"guest_computer_mcp":false,"ci":false}})
}
fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .with_context(|| format!("Missing text field {key}"))
}
fn name(v: &str) -> Result<()> {
    ensure!(
        regex::Regex::new(r"^[a-z][a-z0-9-]{0,48}$")?.is_match(v),
        "Names must begin with lowercase letter and contain at most 49 lowercase letters, digits or hyphens"
    );
    Ok(())
}
fn path(v: &str) -> Result<()> {
    ensure!(
        v != "/"
            && regex::Regex::new(r"^/[a-zA-Z0-9_./-]+$")?.is_match(v)
            && !v.split('/').any(|p| p == ".."),
        "Expected absolute non-root path without '..'"
    );
    Ok(())
}
pub fn size(v: &str) -> Result<u64> {
    ensure!(
        regex::Regex::new(r"^[1-9][0-9]*(MiB|GiB|TiB)$")?.is_match(v),
        "Sizes need positive MiB/GiB/TiB"
    );
    let n = resource_bytes(&json!(v), 0);
    ensure!(n > 0, "Size overflow");
    Ok(n)
}
pub fn normalize(raw: Value) -> Result<Value> {
    let mut c = defaults();
    let map = raw
        .as_object()
        .context("Bootstrap configuration must be an object")?;
    for (k, v) in map {
        ensure!(c.get(k).is_some(), "Unknown bootstrap section {k}");
        if k == "schema_version" {
            ensure!(v == 1, "Unsupported bootstrap schema");
            continue;
        }
        for (key, value) in v
            .as_object()
            .context("Bootstrap sections must be objects")?
        {
            let default = c[k]
                .get(key)
                .with_context(|| format!("Unknown {k}.{key}"))?;
            ensure!(
                std::mem::discriminant(default) == std::mem::discriminant(value),
                "Wrong type for {k}.{key}"
            );
            c[k][key] = value.clone();
        }
    }
    let h = &c["host"];
    ensure!(
        ["local", "ssh"].contains(&text(h, "transport")?),
        "Transport must be local or ssh"
    );
    if h["transport"] == "ssh" {
        let host = Host {
            id: "bootstrap".into(),
            target: text(h, "target")?.into(),
            ..Default::default()
        };
        host.validate()?;
    }
    path(text(h, "data_dir")?)?;
    size(text(h, "disk_reserve")?)?;
    size(text(h, "memory_reserve")?)?;
    for k in ["project", "profile"] {
        name(text(&c["incus"], k)?)?;
    }
    let storage = &c["storage"];
    let network = &c["network"];
    let ws = &c["workspace"];
    name(text(storage, "pool")?)?;
    name(text(network, "name")?)?;
    ensure!(
        ["existing", "development-loop"].contains(&text(storage, "mode")?),
        "Storage mode must be existing or development-loop"
    );
    size(text(storage, "size")?)?;
    ensure!(
        ["existing", "managed"].contains(&text(network, "mode")?),
        "Network mode must be existing or managed"
    );
    let subnet: ipnet::Ipv4Net = text(network, "subnet")?.parse()?;
    ensure!(
        (16..=28).contains(&subnet.prefix_len())
            && subnet.addr().is_private()
            && subnet.network().is_private()
            && subnet.broadcast().is_private()
            && subnet.addr() != subnet.network()
            && subnet.addr() != subnet.broadcast(),
        "Use RFC1918 subnet /16–/28 with usable bridge address"
    );
    name(text(ws, "name")?)?;
    crate::identifier(text(ws, "name")?, true)?;
    ensure!(
        ws["image"] == "images:ubuntu/24.04/cloud",
        "Only Ubuntu24.04 cloud is qualified"
    );
    for (k, lo, hi) in [
        ("cpu", 1, 512),
        ("uid", 1000, 60000),
        ("gid", 1000, 60000),
        ("ready_timeout", 30, 1800),
    ] {
        ensure!(
            ws[k].as_u64().is_some_and(|n| (lo..=hi).contains(&n)),
            "Invalid workspace.{k}"
        );
    }
    guest(&c)?.validate()?;
    path(text(ws, "home")?)?;
    path(text(ws, "path")?)?;
    ensure!(
        text(ws, "home")?.starts_with("/home/") && ws["home"] != ws["path"],
        "Guest home must be under /home and differ from workspace"
    );
    let mem = size(text(ws, "memory")?)?;
    let disk = size(text(ws, "disk")?)?;
    let state_size = size(text(ws, "state_disk")?)?;
    ensure!(
        (GIB..=512 * GIB).contains(&mem) && disk >= 12 * GIB && state_size >= mem + GIB,
        "Invalid RAM/root/state disk sizes"
    );
    if storage["mode"] == "development-loop" {
        ensure!(
            size(text(storage, "size")?)? >= disk + state_size + size(text(h, "disk_reserve")?)?,
            "Loop pool cannot cover commitments and reserve"
        );
    }
    ensure!(
        c["modules"]["ci"] == false,
        "CI module is not implemented by bootstrap"
    );
    ensure!(
        c["modules"]["guest_computer_mcp"] != true || c["modules"]["desktop"] == true,
        "Computer MCP needs desktop"
    );
    let clients = c["agents"]["install"]
        .as_array()
        .context("Agent install must be a list")?;
    let mut seen = std::collections::HashSet::new();
    for v in clients {
        let v = v.as_str().context("Agent name must be text")?;
        ensure!(
            ["codex", "claude", "cursor"].contains(&v) && seen.insert(v),
            "Invalid or duplicate native client"
        );
    }
    let admission = &h["admission"];
    if admission.as_object().is_some_and(|m| !m.is_empty()) {
        let keys = admission.as_object().unwrap();
        for k in [
            "reserve_cpu",
            "reserve_memory_gib",
            "reserve_disk_gib",
            "max_instances",
        ] {
            ensure!(keys.contains_key(k), "Admission requires {k}");
        }
        ensure!(
            keys.keys().all(|k| [
                "reserve_cpu",
                "reserve_memory_gib",
                "reserve_disk_gib",
                "max_instances",
                "contribution_cpu",
                "contribution_memory_gib",
                "cpu_overcommit"
            ]
            .contains(&k.as_str())),
            "Unknown admission field"
        );
        ensure!(
            keys.contains_key("contribution_cpu") == keys.contains_key("contribution_memory_gib"),
            "Contribution limits must be paired"
        );
        for (k, v) in keys {
            if k == "cpu_overcommit" {
                ensure!(
                    v.as_f64()
                        .is_some_and(|r| r.is_finite() && (1.0..=64.0).contains(&r)),
                    "Invalid admission {k}"
                );
                continue;
            }
            let (lo, hi) = match k.as_str() {
                "max_instances" => (1, 256),
                "contribution_cpu" => (1, 4096),
                "contribution_memory_gib" => (1, 16384),
                _ => (0, 16384),
            };
            ensure!(
                v.as_u64().is_some_and(|n| (lo..=hi).contains(&n)),
                "Invalid admission {k}"
            );
        }
    }
    Ok(c)
}
pub fn load_config(p: &Path) -> Result<Value> {
    let v: toml::Value = toml::from_str(&std::fs::read_to_string(p)?)?;
    normalize(serde_json::to_value(v)?)
}
pub fn fingerprint(c: &Value) -> String {
    let mut c = c.clone();
    c.as_object_mut().unwrap().remove("host");
    state::digest(&c)
}
pub fn guest(c: &Value) -> Result<GuestConfig> {
    let w = &c["workspace"];
    Ok(GuestConfig {
        uid: number(&w["uid"]) as u32,
        gid: number(&w["gid"]) as u32,
        user: text(w, "user")?.into(),
        home: text(w, "home")?.into(),
        workspace: text(w, "path")?.into(),
        display: ":99".into(),
    })
}
fn host(c: &Value) -> Result<Host> {
    Ok(Host {
        id: "bootstrap".into(),
        target: if c["host"]["transport"] == "local" {
            "local".into()
        } else {
            text(&c["host"], "target")?.into()
        },
        project: text(&c["incus"], "project")?.into(),
        pool: text(&c["storage"], "pool")?.into(),
        guest: guest(c)?,
        ..Default::default()
    })
}
pub fn runtime_environment(c: &Value) -> Value {
    json!({"ENGINEBOX_HOST":if c["host"]["transport"]=="local"{json!("local")}else{c["host"]["target"].clone()},"ENGINEBOX_PROJECT":c["incus"]["project"],"ENGINEBOX_GUEST_USER":c["workspace"]["user"],"ENGINEBOX_GUEST_UID":number(&c["workspace"]["uid"]).to_string(),"ENGINEBOX_GUEST_GID":number(&c["workspace"]["gid"]).to_string(),"ENGINEBOX_GUEST_HOME":c["workspace"]["home"],"ENGINEBOX_GUEST_WORKSPACE":c["workspace"]["path"]})
}
pub fn inventory(c: &Value) -> Result<Value> {
    if c["host"]["transport"] == "local" {
        return inspect_local(c);
    }
    let bytes = host_run(
        &host(c)?,
        &[
            "enginebox-host".into(),
            "bootstrap".into(),
            "inspect".into(),
        ],
        Some(&serde_json::to_vec(c)?),
        Duration::from_secs(180),
    )?;
    Ok(serde_json::from_slice(&bytes)?)
}
fn optional_probe(argv: &[&str]) -> Option<String> {
    run_process(
        &argv.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        None,
        Duration::from_secs(15),
    )
    .ok()
    .map(|b| String::from_utf8_lossy(&b).trim().to_owned())
}
pub fn inspect_local(c: &Value) -> Result<Value> {
    let os = std::fs::read_to_string("/etc/os-release")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), json!(v.trim_matches('"'))))
        .collect::<serde_json::Map<_, _>>();
    let incus = optional_probe(&["incus", "--version"]).is_some();
    let server = if incus {
        crate::capacity::incus_query("/1.0").ok()
    } else {
        None
    };
    let memory = crate::capacity::memory().unwrap_or((0, 0));
    let pool = text(&c["storage"], "pool")?;
    let project = text(&c["incus"], "project")?;
    let mut result = json!({"schema_version":1,"system":if cfg!(target_os="linux"){"Linux"}else{std::env::consts::OS},"architecture":std::env::consts::ARCH,"uid":current_uid(),"kvm":std::fs::OpenOptions::new().read(true).write(true).open("/dev/kvm").is_ok(),"cpu_count":std::thread::available_parallelism().map(|n|n.get()).unwrap_or(0),"memory_total":memory.0,"memory_available":memory.1,"host_disk_free":fs2::available_space("/").unwrap_or(0),"incus_disk_free":fs2::available_space(if Path::new("/var/lib/incus").exists(){"/var/lib/incus"}else{"/var/lib"}).unwrap_or(0),"incus_present":incus,"os":os,"server":server,"qemu_version":optional_probe(&["qemu-system-x86_64","--version"]),"errors":[],"firewall_tools":[],"routes":optional_probe(&["ip","-j","-4","route","show"]).and_then(|s|serde_json::from_str::<Value>(&s).ok()).unwrap_or(json!([]))});
    for (key, path) in [
        ("projects", "/1.0/projects?recursion=1".to_owned()),
        ("storage", "/1.0/storage-pools?recursion=1".to_owned()),
        ("networks", "/1.0/networks?recursion=1".to_owned()),
        (
            "profiles",
            format!("/1.0/profiles?project={project}&recursion=1"),
        ),
        (
            "instances",
            format!("/1.0/instances?project={project}&recursion=1"),
        ),
    ] {
        result[key] = if server.is_some() {
            crate::capacity::incus_query(&path).unwrap_or(json!([]))
        } else {
            json!([])
        };
    }
    result["pool_resources"] =
        crate::capacity::incus_query(&format!("/1.0/storage-pools/{pool}/resources"))
            .unwrap_or(Value::Null);
    if let Ok(facts) = crate::capacity::local_inventory(pool, project) {
        result["fleet_facts"] = facts;
    }
    Ok(result)
}
pub fn current_uid() -> u32 {
    #[cfg(unix)]
    {
        unsafe { libc::geteuid() }
    }
    #[cfg(not(unix))]
    {
        u32::MAX
    }
}
pub fn profile_definition(c: &Value) -> Value {
    let w = &c["workspace"];
    json!({"description":"Engine Box managed native-agent workspace","config":{OWNER:c["incus"]["project"],"boot.autostart":"false","limits.cpu":number(&w["cpu"]).to_string(),"limits.memory":w["memory"],"migration.stateful":"true","cloud-init.user-data":"#cloud-config\npreserve_hostname: true\n","cloud-init.network-config":"version: 2\nethernets:\n  enginebox:\n    match:\n      name: 'en*'\n    dhcp4: true\n    dhcp-identifier: mac\n"},"devices":{"root":{"type":"disk","path":"/","pool":c["storage"]["pool"],"size":w["disk"],"size.state":w["state_disk"]},"eth0":{"type":"nic","network":c["network"]["name"],"name":"eth0"}}})
}
fn owned(v: &Value, c: &Value) -> bool {
    v["config"][OWNER] == c["incus"]["project"]
}
fn by_name<'a>(v: &'a Value, key: &str, name: &str) -> Option<&'a Value> {
    v[key]
        .as_array()
        .into_iter()
        .flatten()
        .find(|i| i["name"] == name)
}
pub fn build_plan(c: &Value, o: &Value) -> Result<Value> {
    let c = normalize(c.clone())?;
    let mut errors = o["errors"].as_array().cloned().unwrap_or_default();
    let mut warnings = Vec::new();
    let mut actions: Vec<&str> = Vec::new();
    let ws = &c["workspace"];
    let h = &c["host"];
    let storage = &c["storage"];
    let network = &c["network"];
    macro_rules! reject {
        ($condition:expr,$message:expr) => {
            if $condition {
                errors.push(json!($message));
            }
        };
    }
    reject!(
        o["system"] != "Linux"
            || !["x86_64", "amd64"].contains(&o["architecture"].as_str().unwrap_or("")),
        "Bootstrap requires Linux x86_64"
    );
    reject!(
        o["uid"] != 0,
        "Inspection/apply requires root through noninteractive sudo"
    );
    reject!(o["kvm"] != true, "Host needs readable/writable /dev/kvm");
    reject!(
        number(&ws["cpu"]) > number(&o["cpu_count"]),
        "Workspace CPU exceeds logical host CPU count"
    );
    if o["incus_present"] != true {
        if h["install_packages"] != true {
            errors.push(json!(
                "Incus absent; set host.install_packages explicitly to bootstrap dependencies"
            ));
        } else if o["os"]["ID"] != "ubuntu"
            || !["24.04", "26.04"].contains(&o["os"]["VERSION_ID"].as_str().unwrap_or(""))
        {
            errors.push(json!(
                "Automatic package installation supports Ubuntu24.04/26.04 only"
            ));
        } else {
            actions.push("install_packages");
        }
    } else if o["server"].is_null() {
        errors.push(json!(
            "Incus daemon unavailable; start/repair it explicitly"
        ));
    } else {
        let environment = &o["server"]["environment"];
        let version = environment["server_version"].as_str().unwrap_or("");
        reject!(
            !version_at_least(version, (6, 0)),
            "Incus >=6.0 is required"
        );
        reject!(
            !o["server"]["api_extensions"]
                .as_array()
                .is_some_and(|v| v.iter().any(|x| x == "migration_stateful")),
            "Incus lacks stateful VM capability"
        );
        reject!(
            environment["driver"]
                .as_str()
                .is_some_and(|s| !s.contains("qemu")),
            "Incus lacks QEMU driver"
        );
        reject!(
            !version_at_least(o["qemu_version"].as_str().unwrap_or(""), (8, 2)),
            "QEMU >=8.2 is required"
        );
    }
    let instance = by_name(o, "instances", text(ws, "name")?);
    let pool = by_name(o, "storage", text(storage, "pool")?);
    if instance.is_none_or(|i| i["status"] == "Stopped" && i["config"][PREPARED] != fingerprint(&c))
    {
        reject!(
            number(&o["memory_available"])
                < size(text(ws, "memory")?)? + size(text(h, "memory_reserve")?)?,
            "Available RAM below workspace plus reserve"
        );
    }
    let disk_need =
        size(text(ws, "disk")?)? + size(text(ws, "state_disk")?)? + size(text(h, "disk_reserve")?)?;
    match pool {
        None => {
            if storage["mode"] == "existing" {
                errors.push(json!("Existing storage pool not found"));
            } else {
                actions.push("create_pool");
                reject!(
                    number(&o["incus_disk_free"])
                        < size(text(storage, "size")?)? + size(text(h, "disk_reserve")?)?,
                    "Free disk below bounded loop pool plus reserve"
                );
            }
        }
        Some(p) => {
            if storage["mode"] == "development-loop" {
                reject!(
                    !owned(p, &c),
                    "Existing loop pool is not owned by this installation"
                );
                reject!(
                    p["driver"] != "btrfs" || p["config"]["size"] != storage["size"],
                    "Managed pool differs; resize/migrate explicitly"
                );
            }
            reject!(
                !["btrfs", "zfs", "lvm", "dir"].contains(&p["driver"].as_str().unwrap_or("")),
                "Unsupported existing pool driver"
            );
            if p["driver"] == "dir" {
                warnings.push(json!(
                    "Directory storage does not provide efficient copy-on-write forks"
                ));
            }
            if instance.is_none() {
                let sp = &o["pool_resources"]["space"];
                reject!(
                    number(&sp["total"]).saturating_sub(number(&sp["used"])) < disk_need,
                    "Storage pool lacks full root/state disk commitments and reserve"
                );
                reject!(
                    p["config"]["source"]
                        .as_str()
                        .is_some_and(|s| s.ends_with(".img"))
                        && number(&o["incus_disk_free"]) < disk_need,
                    "Loop backing filesystem lacks free space"
                );
            }
        }
    }
    match by_name(o, "networks", text(network, "name")?) {
        None => {
            if network["mode"] == "existing" {
                errors.push(json!("Existing network not found"));
            } else {
                let requested: ipnet::Ipv4Net = text(network, "subnet")?.parse()?;
                for route in o["routes"].as_array().into_iter().flatten() {
                    if let Some(dst) = route["dst"]
                        .as_str()
                        .and_then(|s| s.parse::<ipnet::Ipv4Net>().ok())
                    {
                        reject!(
                            requested.contains(&dst.network())
                                || dst.contains(&requested.network()),
                            "Managed subnet overlaps a host route"
                        );
                    }
                }
                for net in o["networks"].as_array().into_iter().flatten() {
                    if let Some(other) = net["config"]["ipv4.address"]
                        .as_str()
                        .and_then(|s| s.parse::<ipnet::Ipv4Net>().ok())
                    {
                        reject!(
                            requested.contains(&other.network())
                                || other.contains(&requested.network()),
                            "Managed subnet overlaps an Incus network"
                        );
                    }
                }
                actions.push("create_network");
            }
        }
        Some(n) => {
            reject!(
                n["managed"] != true || n["type"] != "bridge",
                "Selected network must be an Incus managed bridge"
            );
            if network["mode"] == "managed" {
                reject!(!owned(n, &c), "Existing managed network is not owned");
                reject!(
                    n["config"]["ipv4.address"] != network["subnet"],
                    "Managed subnet differs; migrate explicitly"
                );
            }
            reject!(
                n["config"]["ipv4.address"]
                    .as_str()
                    .is_none_or(|s| s == "none" || s == "auto")
                    || n["config"]["ipv4.dhcp"]
                        .as_str()
                        .is_some_and(|s| s != "true"),
                "Network needs concrete IPv4 and DHCP"
            );
        }
    }
    match by_name(o, "projects", text(&c["incus"], "project")?) {
        None => actions.push("create_project"),
        Some(p) => {
            reject!(!owned(p, &c), "Existing project lacks Engine Box ownership");
            reject!(
                p["config"]["features.networks"].as_str().unwrap_or("false") != "false"
                    || p["config"]["features.profiles"].as_str().unwrap_or("true") != "true",
                "Project requires global networks and private profiles"
            );
        }
    }
    let profile = profile_definition(&c);
    match by_name(o, "profiles", text(&c["incus"], "profile")?) {
        None => actions.push("create_profile"),
        Some(p) => {
            reject!(!owned(p, &c), "Existing profile lacks Engine Box ownership");
            reject!(
                p["config"] != profile["config"] || p["devices"] != profile["devices"],
                "Existing profile differs; use new profile or explicit migration"
            );
        }
    }
    match instance {
        None => actions.extend(["create_workspace", "prepare_workspace"]),
        Some(i) => {
            if let Err(error) = ensure_no_external_checkout(i, "bootstrap preparation") {
                errors.push(json!(error.to_string()));
            }
            reject!(
                !owned(i, &c),
                "Existing workspace lacks Engine Box ownership"
            );
            reject!(
                i["config"][RECIPE] != fingerprint(&c),
                "Workspace recipe changed; use new name or explicit migration"
            );
            reject!(
                i["profiles"] != json!([c["incus"]["profile"]]) || i["type"] != "virtual-machine",
                "Workspace profile/type changed"
            );
            reject!(
                i["devices"].as_object().is_some_and(|d| !d.is_empty()),
                "Workspace devices override managed profile"
            );
            if let Some(expanded) = i["expanded_config"].as_object() {
                for (k, v) in profile["config"].as_object().unwrap() {
                    reject!(
                        expanded.get(k).is_some_and(|actual| actual != v),
                        "Workspace overrides managed profile settings"
                    );
                }
            }
            if i["config"][PREPARED] != fingerprint(&c) {
                actions.push("prepare_workspace");
            }
        }
    }
    if actions
        .iter()
        .any(|a| ["create_workspace", "prepare_workspace"].contains(a))
    {
        errors.extend(admission_errors(&c, o)?);
    }
    warnings.push(json!(
        "Existing firewall, unrelated profiles and storage are preserved"
    ));
    Ok(
        json!({"schema_version":1,"config_fingerprint":fingerprint(&c),"ok":errors.is_empty(),"errors":errors,"warnings":warnings,"operations":actions.into_iter().map(|a|json!({"action":a})).collect::<Vec<_>>(),"runtime_environment":runtime_environment(&c)}),
    )
}
fn version_at_least(v: &str, min: (u64, u64)) -> bool {
    regex::Regex::new(r"(\d+)\.(\d+)")
        .unwrap()
        .captures(v)
        .and_then(|c| Some((c[1].parse::<u64>().ok()?, c[2].parse::<u64>().ok()?)))
        .is_some_and(|v| v >= min)
}
pub fn admission_errors(c: &Value, o: &Value) -> Result<Vec<Value>> {
    let p = &c["host"]["admission"];
    if p.as_object().is_none_or(|m| m.is_empty()) {
        return Ok(vec![]);
    }
    let Some(facts) = o.get("fleet_facts") else {
        return Ok(vec![json!(
            "Fleet admission requires complete allocation metadata across every project"
        )]);
    };
    let h = Host {
        id: "bootstrap".into(),
        target: "local".into(),
        project: text(&c["incus"], "project")?.into(),
        pool: text(&c["storage"], "pool")?.into(),
        reserve_cpu: number(&p["reserve_cpu"]),
        reserve_memory_gib: number(&p["reserve_memory_gib"]),
        reserve_disk_gib: number(&p["reserve_disk_gib"]),
        max_instances: number(&p["max_instances"]),
        contribution_cpu: p["contribution_cpu"].as_u64(),
        contribution_memory_gib: p["contribution_memory_gib"].as_u64(),
        cpu_overcommit: p["cpu_overcommit"].as_f64(),
        ..Default::default()
    };
    let room = crate::capacity::capacity(&h, facts)?;
    let existing = facts["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["project"] == h.project && i["name"] == c["workspace"]["name"]);
    let mut errors = Vec::new();
    if existing.is_none_or(|i| i["status"] == "Stopped")
        && (number(&room["cpu_available"]) < number(&c["workspace"]["cpu"])
            || number(&room["memory_budget_available_bytes"])
                < size(text(&c["workspace"], "memory")?)?)
    {
        errors.push(json!(
            "Fleet admission: CPU/RAM commitments exceed reserve or contribution ceiling"
        ));
    }
    if existing.is_none()
        && (number(&room["instance_slots_available"]) == 0
            || number(&room["disk_budget_available_bytes"])
                < size(text(&c["workspace"], "disk")?)?
                    + size(text(&c["workspace"], "state_disk")?)?)
    {
        errors.push(json!(
            "Fleet admission: disk commitments or instance count exceed budget"
        ));
    }
    Ok(errors)
}
fn local_box(c: &Value) -> Result<EngineBox> {
    EngineBox::new("local", text(&c["incus"], "project")?, guest(c)?)
}
fn install_packages() -> Result<()> {
    for args in [
        vec![
            "env",
            "DEBIAN_FRONTEND=noninteractive",
            "NEEDRESTART_MODE=l",
            "apt-get",
            "update",
        ],
        vec![
            "env",
            "DEBIAN_FRONTEND=noninteractive",
            "NEEDRESTART_MODE=l",
            "apt-get",
            "install",
            "-y",
            "--no-install-recommends",
            "incus",
            "qemu-system-x86",
            "qemu-system-modules-spice",
            "qemu-utils",
            "ovmf",
            "btrfs-progs",
        ],
    ] {
        if args.contains(&"install") {
            for (package, min) in [("incus", (6, 0)), ("qemu-system-x86", (8, 2))] {
                let policy = optional_probe(&["apt-cache", "policy", package])
                    .context("Cannot inspect apt candidate")?;
                let candidate = policy
                    .lines()
                    .find(|l| l.trim().starts_with("Candidate:"))
                    .unwrap_or("");
                ensure!(
                    version_at_least(candidate, min),
                    "APT candidate for {package} below supported minimum; no packages installed"
                );
            }
        }
        run_process(
            &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            None,
            Duration::from_secs(1800),
        )?;
    }
    run_process(
        &["systemctl".into(), "start".into(), "incus.socket".into()],
        None,
        Duration::from_secs(60),
    )?;
    Ok(())
}
fn wait_guest(c: &Value) -> Result<()> {
    let bx = local_box(c)?;
    let name = text(&c["workspace"], "name")?;
    let deadline =
        std::time::Instant::now() + Duration::from_secs(number(&c["workspace"]["ready_timeout"]));
    while std::time::Instant::now() < deadline {
        if bx
            .run(
                &["exec".into(), name.into(), "--".into(), "true".into()],
                Duration::from_secs(15),
            )
            .is_ok()
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    bail!("Guest unavailable; existing firewall/routing was not modified")
}
fn prepare_workspace(c: &Value, binary_dir: &Path) -> Result<()> {
    let bx = local_box(c)?;
    let name = text(&c["workspace"], "name")?;
    let vm = bx.instance(name)?;
    ensure_no_external_checkout(&vm, "bootstrap preparation")?;
    if vm["config"].get("user.enginebox.clean-base").is_some() {
        bx.call(&["config", "unset", name, "user.enginebox.clean-base"])?;
    }
    if vm["status"] == "Stopped" && vm["stateful"] != true {
        bx.call(&["start", name])?;
    } else {
        ensure!(
            vm["status"] == "Running",
            "Incomplete workspace frozen or hibernated; resume explicitly"
        );
    }
    wait_guest(c)?;
    // Only our native executables cross into the fresh guest; no operator home is bundled.
    for binary in ["enginebox-host", "enginebox-guest", "enginebox-mcp"] {
        let source = binary_dir.join(binary);
        ensure!(source.is_file(), "Missing provisioning binary {binary}");
        validate_guest_binary(&source)?;
        bx.call(&[
            "file",
            "push",
            source.to_str().context("Invalid binary path")?,
            &format!("{name}/usr/local/bin/{binary}"),
            "--mode=0755",
            "--uid=0",
            "--gid=0",
        ])?;
    }
    let command = vec![
        "exec".into(),
        name.into(),
        "--".into(),
        "enginebox-host".into(),
        "prepare-guest".into(),
    ];
    bx.run_input(&command, &serde_json::to_vec(c)?, Duration::from_secs(3600))?;
    bx.call(&[
        "exec",
        name,
        "--",
        "enginebox-host",
        "clean-base-check",
        "--home",
        text(&c["workspace"], "home")?,
    ])?;
    bx.call(&["exec", name, "--", "enginebox-host", "reset-base-identity"])?;
    bx.call(&["stop", name, "--timeout=120"])?;
    let snap = format!("prepared-{}", &fingerprint(c)[..12]);
    let snapshots = bx.query(&format!(
        "/1.0/instances/{name}/snapshots?project={}&recursion=1",
        bx.project
    ))?;
    if !snapshots
        .as_array()
        .context("Invalid snapshot response")?
        .iter()
        .any(|v| {
            v["name"]
                .as_str()
                .is_some_and(|s| s.rsplit('/').next() == Some(&snap))
        })
    {
        bx.call(&["config", "set", name, "user.enginebox.clean-base=true"])?;
        let result = bx.call(&["snapshot", "create", name, &snap]);
        let unset = bx.call(&["config", "unset", name, "user.enginebox.clean-base"]);
        result?;
        unset?;
    }
    bx.call(&["start", name])?;
    wait_guest(c)?;
    crate::fleet::wait_ready(&bx, name)?;
    bx.call(&[
        "config",
        "set",
        name,
        &format!("{PREPARED}={}", fingerprint(c)),
    ])?;
    Ok(())
}
fn execute(c: &Value, action: &str, binary_dir: &Path) -> Result<()> {
    let bx = local_box(c)?;
    let owner = format!("{OWNER}={}", text(&c["incus"], "project")?);
    let storage = &c["storage"];
    let network = &c["network"];
    let ws = &c["workspace"];
    // Global Incus operations still use explicit query/global argument forms.
    let global = |args: &[&str]| -> Result<()> {
        let mut argv = vec!["incus".into()];
        argv.extend(args.iter().map(|s| s.to_string()));
        run_process(&argv, None, Duration::from_secs(1800))?;
        Ok(())
    };
    match action {
        "install_packages" => install_packages()?,
        "create_pool" => {
            ensure!(
                fs2::available_space("/var/lib/incus")?
                    >= size(text(storage, "size")?)? + size(text(&c["host"], "disk_reserve")?)?,
                "Insufficient loop pool backing filesystem space"
            );
            global(&[
                "storage",
                "create",
                text(storage, "pool")?,
                "btrfs",
                &format!("size={}", text(storage, "size")?),
                &owner,
            ])?;
        }
        "create_network" => global(&[
            "network",
            "create",
            text(network, "name")?,
            &format!("ipv4.address={}", text(network, "subnet")?),
            "ipv4.nat=true",
            "ipv6.address=none",
            &owner,
        ])?,
        "create_project" => global(&[
            "project",
            "create",
            text(&c["incus"], "project")?,
            "-c",
            &owner,
            "-c",
            "features.images=false",
            "-c",
            "features.profiles=true",
            "-c",
            "features.storage.volumes=false",
            "-c",
            "features.networks=false",
        ])?,
        "create_profile" => {
            let mut body = profile_definition(c);
            body["name"] = c["incus"]["profile"].clone();
            bx.call(&[
                "query",
                "--request",
                "POST",
                &format!("/1.0/profiles?project={}", bx.project),
                "--data",
                &serde_json::to_string(&body)?,
            ])?;
        }
        "create_workspace" => {
            bx.call(&[
                "init",
                text(ws, "image")?,
                text(ws, "name")?,
                "--vm",
                "--profile",
                text(&c["incus"], "profile")?,
                "-c",
                &owner,
                "-c",
                &format!("{RECIPE}={}", fingerprint(c)),
            ])?;
        }
        "prepare_workspace" => prepare_workspace(c, binary_dir)?,
        _ => bail!("Unknown bootstrap operation"),
    };
    Ok(())
}
pub fn apply(c: &Value, binary_dir: Option<&Path>) -> Result<Value> {
    let c = normalize(c.clone())?;
    if c["host"]["transport"] == "ssh" {
        let mut argv = vec!["enginebox-host".into(), "bootstrap".into(), "apply".into()];
        if let Some(dir) = binary_dir {
            argv.extend([
                "--binary-dir".into(),
                dir.to_str().context("Invalid binary directory")?.into(),
            ]);
        }
        let bytes = host_run(
            &host(&c)?,
            &argv,
            Some(&serde_json::to_vec(&c)?),
            Duration::from_secs(7200),
        )?;
        return Ok(serde_json::from_slice(&bytes)?);
    }
    apply_local(&c, binary_dir)
}
pub fn apply_local(c: &Value, binary_dir: Option<&Path>) -> Result<Value> {
    ensure!(
        cfg!(target_os = "linux") && current_uid() == 0,
        "Host apply requires Linux root"
    );
    let c = normalize(c.clone())?;
    let executable = std::env::current_exe()?;
    let binary_dir = binary_dir.unwrap_or(executable.parent().context("Executable lacks parent")?);
    let _lock = state::root_host_lock()?;
    let dir = Path::new(text(&c["host"], "data_dir")?).join("bootstrap");
    state::private_dir(&dir)?;
    let journal_path = dir.join(format!("{}.json", text(&c["workspace"], "name")?));
    let mut journal = json!({"schema_version":1,"config_fingerprint":fingerprint(&c),"completed":[],"status":"running"});
    let result = (|| -> Result<Value> {
        for _ in 0..16 {
            let plan = build_plan(&c, &inspect_local(&c)?)?;
            ensure!(plan["ok"] == true, "Preflight failed: {}", plan["errors"]);
            let operations = plan["operations"].as_array().unwrap();
            if operations.is_empty() {
                journal["status"] = json!("complete");
                state::write_json(&journal_path, &journal)?;
                return Ok(
                    json!({"ok":true,"changed":!journal["completed"].as_array().unwrap().is_empty(),"completed":journal["completed"],"runtime_environment":runtime_environment(&c),"workspace":c["workspace"]["name"],"snapshot":format!("prepared-{}",&fingerprint(&c)[..12]),"warnings":plan["warnings"]}),
                );
            }
            let action = text(&operations[0], "action")?;
            journal["current"] = json!(action);
            state::write_json(&journal_path, &journal)?;
            eprintln!("Engine Box: {action}");
            execute(&c, action, binary_dir)?;
            journal["completed"]
                .as_array_mut()
                .unwrap()
                .push(json!(action));
            state::write_json(&journal_path, &journal)?;
        }
        bail!("Bootstrap did not converge after 16 operations")
    })();
    if let Err(e) = &result {
        journal["status"] = json!("failed");
        journal["error"] = json!(e.to_string().chars().take(2000).collect::<String>());
        state::write_json(&journal_path, &journal)?;
    }
    result
}
fn discover_network(h: &Host) -> Result<String> {
    if let Some(n) = &h.network {
        return Ok(n.clone());
    }
    let bx = h.engine_box();
    let mut networks = std::collections::BTreeSet::new();
    for t in &h.templates {
        let vm = bx.instance(&t.source)?;
        for d in vm["expanded_devices"]
            .as_object()
            .into_iter()
            .flat_map(|o| o.values())
        {
            if d["type"] == "nic"
                && let Some(n) = d["network"].as_str().or_else(|| {
                    if d["nictype"] == "bridged" {
                        d["parent"].as_str()
                    } else {
                        None
                    }
                })
            {
                networks.insert(n.to_owned());
            }
        }
    }
    ensure!(
        networks.len() == 1,
        "Set an explicit host network; existing bases lack one unambiguous bridge"
    );
    Ok(networks.into_iter().next().unwrap())
}
pub fn install_template(h: &Host, manifest: &Value) -> Result<Value> {
    let manifest = crate::templates::validate_manifest(manifest.clone())?;
    let network = discover_network(h)?;
    let generation = state::digest(
        &json!({"manifest":state::digest(&manifest),"host":h.binding(),"network":network}),
    );
    let base = format!("ebox-template-{}", &generation[..16]);
    let r = Resources::from_value(&manifest["recommended"], None)?;
    let mut admission = json!({"reserve_cpu":h.reserve_cpu,"reserve_memory_gib":h.reserve_memory_gib,"reserve_disk_gib":h.reserve_disk_gib,"max_instances":h.max_instances});
    if let (Some(cpu), Some(mem)) = (h.contribution_cpu, h.contribution_memory_gib) {
        admission["contribution_cpu"] = json!(cpu);
        admission["contribution_memory_gib"] = json!(mem);
    }
    if let Some(ratio) = h.cpu_overcommit {
        admission["cpu_overcommit"] = json!(ratio);
    }
    let c = normalize(
        json!({"host":{"transport":if h.target=="local"{"local"}else{"ssh"},"target":h.target,"install_packages":false,"disk_reserve":format!("{}GiB",h.reserve_disk_gib.max(1)),"memory_reserve":format!("{}GiB",h.reserve_memory_gib.max(1)),"admission":admission},"incus":{"project":h.project,"profile":base},"storage":{"mode":"existing","pool":h.pool},"network":{"mode":"existing","name":network},"workspace":{"name":base,"image":manifest["source_image"],"cpu":r.cpu,"memory":format!("{}GiB",r.memory_gib),"disk":format!("{}GiB",manifest["minimum"]["disk_gib"].as_u64().unwrap()),"state_disk":format!("{}GiB",r.memory_gib+2),"user":h.guest.user,"uid":h.guest.uid,"gid":h.guest.gid,"home":h.guest.home,"path":h.guest.workspace},"agents":{"install":manifest["native_clients"]},"modules":manifest["config"]}),
    )?;
    // The controller can share a machine with its node while remaining an
    // unprivileged private-state owner. Bootstrap always crosses the same
    // administrator-configured sudo helper boundary, including target=local.
    // Keep the original host/config binding; changing it to SSH would invalidate
    // existing deployment receipts and warm generations.
    let observed: Value = serde_json::from_slice(&host_run(
        h,
        &[
            "enginebox-host".into(),
            "bootstrap".into(),
            "inspect".into(),
        ],
        Some(&serde_json::to_vec(&c)?),
        Duration::from_secs(180),
    )?)?;
    let plan = build_plan(&c, &observed)?;
    ensure!(
        plan["ok"] == true,
        "Template preflight failed: {}",
        plan["errors"]
    );
    ensure!(
        plan["operations"].as_array().unwrap().iter().all(|o| [
            "create_profile",
            "create_workspace",
            "prepare_workspace"
        ]
        .contains(&o["action"].as_str().unwrap_or(""))),
        "Templates require an existing owned project, storage and bridge; bootstrap host first"
    );
    host_run(
        h,
        &["enginebox-host".into(), "bootstrap".into(), "apply".into()],
        Some(&serde_json::to_vec(&c)?),
        Duration::from_secs(7200),
    )?;
    let _lock = crate::operation_lock(h)?;
    let bx = h.engine_box();
    let vm = bx.instance(&base)?;
    ensure_no_external_checkout(&vm, "template publication")?;
    let fp = fingerprint(&c);
    let snapshot = format!("prepared-{}", &fp[..12]);
    ensure!(
        vm["config"][OWNER] == h.project
            && vm["config"][RECIPE] == fp
            && vm["config"][PREPARED] == fp
            && vm["profiles"] == json!([base])
            && vm["type"] == "virtual-machine",
        "Prepared template ownership/configuration mismatch"
    );
    let snap = bx.query(&format!(
        "/1.0/instances/{base}/snapshots/{snapshot}?project={}",
        h.project
    ))?;
    ensure_no_external_checkout(&snap, "template publication")?;
    ensure!(
        snap["config"]["user.enginebox.clean-base"] == "true" && snap["stateful"] != true,
        "Prepared snapshot is not clean disk-only base"
    );
    if vm["status"] == "Running" {
        bx.call(&["stop", &base, "--timeout=120"])?;
    } else {
        ensure!(
            vm["status"] == "Stopped" && vm["stateful"] != true,
            "Prepared template in unexpected state"
        );
    }
    Ok(
        json!({"source":base,"snapshot":snapshot,"snapshot_created_at":snap["created_at"],"network":network,"bootstrap_fingerprint":fp}),
    )
}
/// Host-side bundle selection must never copy a macOS/Windows controller executable into Linux guests.
pub fn validate_guest_binary(path: &Path) -> Result<()> {
    use std::io::Read;
    let mut header = [0u8; 20];
    std::fs::File::open(path)?.read_exact(&mut header)?;
    ensure!(
        &header[..4] == b"\x7fELF" && header[4] == 2 && header[5] == 1,
        "Guest binary must be a 64-bit little-endian Linux ELF"
    );
    ensure!(
        u16::from_le_bytes([header[18], header[19]]) == 62,
        "This Incus VM backend requires x86_64 Linux guest binaries"
    );
    Ok(())
}
