use crate::{GIB, transport::run_process};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
fn probe(argv: &[&str]) -> Option<String> {
    run_process(
        &argv.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        None,
        Duration::from_secs(15),
    )
    .ok()
    .map(|s| String::from_utf8_lossy(&s).trim().to_owned())
}
fn json_probe(argv: &[&str]) -> Option<Value> {
    probe(argv).and_then(|s| serde_json::from_str(&s).ok())
}
fn label(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(160).collect()
}
pub fn allocation(
    cpus: Option<u64>,
    memory: Option<u64>,
    disk: Option<u64>,
    requested_cpu: Option<u64>,
    requested_memory: Option<f64>,
    requested_disk: Option<f64>,
) -> Value {
    let (Some(c), Some(m), Some(d)) = (cpus, memory, disk) else {
        return json!({"status":"unknown","reason":"CPU, RAM and disk observations are required"});
    };
    if c == 0 || m == 0 {
        return json!({"status":"unknown","reason":"CPU and memory observations must be positive"});
    }
    let maximum = json!({"cpus":(c/2).min(c.saturating_sub(2)),"memory_bytes":(m/2).min(m.saturating_sub(4*GIB)),"disk_bytes":(d/2).min(d.saturating_sub(20*GIB))});
    let bytes = |v: Option<f64>, default: &Value| {
        v.map(|n| {
            if n.is_finite() && n > 0.0 && n <= 1_000_000.0 {
                (n * GIB as f64) as u64
            } else {
                0
            }
        })
        .unwrap_or_else(|| default.as_u64().unwrap())
    };
    let requested = json!({"cpus":requested_cpu.unwrap_or_else(||maximum["cpus"].as_u64().unwrap()),"memory_bytes":bytes(requested_memory,&maximum["memory_bytes"]),"disk_bytes":bytes(requested_disk,&maximum["disk_bytes"])});
    let errors = ["cpus", "memory_bytes", "disk_bytes"]
        .iter()
        .filter(|k| requested[**k] == 0 || requested[**k].as_u64() > maximum[**k].as_u64())
        .map(|k| format!("{k} exceeds personal-machine budget or is zero"))
        .collect::<Vec<_>>();
    json!({"status":if errors.is_empty(){"suggested"}else{"rejected"},"maximum":maximum,"requested":requested,"errors":errors,"enforced":false,"scope":"Local report only; Docker VM and current workloads must also be bounded"})
}
pub fn docker_inventory(enabled: bool) -> Value {
    if !enabled {
        return json!({"status":"not_requested"});
    }
    let endpoint = if std::env::var_os("DOCKER_CONTEXT").is_none() {
        std::env::var("DOCKER_HOST")
            .ok()
            .map(Value::String)
            .or_else(|| {
                json_probe(&[
                    "docker",
                    "context",
                    "inspect",
                    "--format",
                    "{{json .Endpoints.docker.Host}}",
                ])
            })
    } else {
        json_probe(&[
            "docker",
            "context",
            "inspect",
            "--format",
            "{{json .Endpoints.docker.Host}}",
        ])
    };
    let Some(endpoint) = endpoint.and_then(|v| v.as_str().map(str::to_owned)) else {
        return json!({"status":"unavailable","reason":"Docker CLI/context unavailable"});
    };
    if !endpoint.starts_with("unix:///") && !endpoint.starts_with("npipe:////./pipe/") {
        return json!({"status":"remote_context_refused","reason":"Choose a local Docker context"});
    }
    let format = r#"{"cpus":{{.NCPU}},"memory_bytes":{{.MemTotal}},"os":{{json .OSType}},"architecture":{{json .Architecture}},"server_version":{{json .ServerVersion}},"security_options":{{json .SecurityOptions}}}"#;
    let Some(d) = json_probe(&["docker", "info", "--format", format]) else {
        return json!({"status":"unavailable","reason":"Local Docker engine did not return inventory"});
    };
    json!({"status":"observed","cpus":d["cpus"],"memory_bytes":d["memory_bytes"],"os":label(d["os"].as_str().unwrap_or("unknown")),"architecture":label(d["architecture"].as_str().unwrap_or("unknown")),"server_version":label(d["server_version"].as_str().unwrap_or("unknown")),"rootless":d["security_options"].as_array().is_some_and(|a|a.iter().any(|v|v.as_str().is_some_and(|s|s.contains("rootless"))))})
}
pub fn graphics() -> Value {
    let mut devices = Vec::new();
    let mut available = false;
    if cfg!(target_os = "macos") {
        if let Some(data) = json_probe(&["system_profiler", "SPDisplaysDataType", "-json"]) {
            available = true;
            for d in data["SPDisplaysDataType"].as_array().into_iter().flatten() {
                devices.push(json!({"name":label(d["sppci_model"].as_str().unwrap_or("Unknown GPU")),"source":"system_profiler","memory_bytes":null}));
            }
        }
    } else if cfg!(target_os = "windows") {
        if let Some(data) = json_probe(&[
            "powershell.exe",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "@(Get-CimInstance Win32_VideoController | Select-Object Name) | ConvertTo-Json -Compress",
        ]) {
            available = true;
            let items = if data.is_array() {
                data.as_array().unwrap().clone()
            } else {
                vec![data]
            };
            for d in items {
                devices.push(json!({"name":label(d["Name"].as_str().unwrap_or("Unknown GPU")),"source":"Win32_VideoController","memory_bytes":null}));
            }
        }
    } else {
        if let Some(data) = probe(&[
            "nvidia-smi",
            "--query-gpu=index,name,memory.total",
            "--format=csv,noheader,nounits",
        ]) {
            available = true;
            for line in data.lines() {
                let parts = line.split(',').map(str::trim).collect::<Vec<_>>();
                if parts.len() == 3 {
                    devices.push(json!({"name":label(parts[1]),"source":"nvidia-smi","memory_bytes":parts[2].parse::<u64>().ok().map(|v|v*1024*1024)}));
                }
            }
        }
        let nvidia = !devices.is_empty();
        if let Ok(entries) = std::fs::read_dir("/sys/bus/pci/devices") {
            available = true;
            for e in entries.flatten() {
                let p = e.path();
                if !std::fs::read_to_string(p.join("class")).is_ok_and(|s| s.starts_with("0x03")) {
                    continue;
                }
                let vendor = std::fs::read_to_string(p.join("vendor")).unwrap_or_default();
                let name = match vendor.trim() {
                    "0x10de" if nvidia => continue,
                    "0x10de" => "NVIDIA",
                    "0x1002" => "AMD",
                    "0x8086" => "Intel",
                    _ => "PCI",
                };
                devices.push(json!({"name":format!("{name} display controller"),"source":"pci-sysfs","memory_bytes":null}));
            }
        }
    }
    json!({"status":if available{"observed"}else{"unknown"},"devices":devices,"passthrough_tested":false,"schedulable":false})
}
pub fn inspect(
    path: &Path,
    docker: bool,
    cpus: Option<u64>,
    memory_gib: Option<f64>,
    disk_gib: Option<f64>,
) -> Result<Value> {
    for n in [memory_gib, disk_gib].into_iter().flatten() {
        ensure!(
            n.is_finite() && n > 0.0 && n <= 1_000_000.0,
            "Resource GiB must be positive and finite"
        );
    }
    ensure!(cpus.is_none_or(|n| n > 0), "CPU count must be positive");
    let total_cpu = std::thread::available_parallelism()
        .ok()
        .map(|n| n.get() as u64);
    let memory = if cfg!(target_os = "linux") {
        crate::capacity::memory().ok().map(|v| v.0)
    } else if cfg!(target_os = "macos") {
        probe(&["sysctl", "-n", "hw.memsize"]).and_then(|s| s.parse().ok())
    } else if cfg!(target_os = "windows") {
        probe(&[
            "powershell.exe",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "(Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory",
        ])
        .and_then(|s| s.parse().ok())
    } else {
        None
    };
    let disk = fs2::available_space(path).ok();
    let kvm = Path::new("/dev/kvm").exists();
    let incus = probe(&["incus", "--version"]).is_some();
    let eligible = cfg!(target_os = "linux") && std::env::consts::ARCH == "x86_64" && kvm && incus;
    let mut docker_report = docker_inventory(docker);
    if docker_report["status"] == "observed" {
        docker_report["allocation"] = allocation(
            total_cpu
                .zip(docker_report["cpus"].as_u64())
                .map(|(a, b)| a.min(b)),
            memory
                .zip(docker_report["memory_bytes"].as_u64())
                .map(|(a, b)| a.min(b)),
            disk,
            cpus,
            memory_gib,
            disk_gib,
        );
    }
    Ok(
        json!({"schema_version":1,"read_only":true,"machine":{"os":if cfg!(target_os="macos"){"Darwin"}else if cfg!(target_os="windows"){"Windows"}else{"Linux"},"architecture":std::env::consts::ARCH,"cpus":total_cpu,"memory_bytes":memory,"disk_free_bytes":disk,"kvm_device_present":kvm,"incus_cli_present":incus},"docker":docker_report,"graphics":graphics(),"allocation":allocation(total_cpu,memory,disk,cpus,memory_gib,disk_gib),"backends":{"incus_linux_vm":{"status":if eligible{"requires_doctor"}else{"not_eligible"},"next_step":"Run ebox doctor with reviewed Linux host configuration"},"docker_workspace":{"status":"planned","enrollment_enabled":false},"native_windows":{"status":"planned","enrollment_enabled":false},"native_macos":{"status":"planned","enrollment_enabled":false}},"notes":["Inventory does not enroll a node or change resource limits","Docker totals are distinct from physical totals","GPU visibility is not passthrough or workload validation"]}),
    )
}
