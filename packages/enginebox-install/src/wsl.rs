//! Trusted Linux runtime inside the dedicated WSL distribution. Workload argv
//! is handed only to LayerFS's unprivileged template sandbox, never a host shell.
use anyhow::{Context, Result, ensure};
use enginebox_node_protocol::{Budget, WorkspaceRequest};
use serde_json::{Value, json};
use std::{path::Path, process::Command, time::Duration};

const CONFIG: &str = "/etc/enginebox/layerfs.json";
const EBOX: &str = "/usr/local/bin/ebox";

pub fn enforce(budget: &Budget) -> Result<()> {
    ensure!(cfg!(target_os = "linux"), "WSL workloads require Linux");
    ensure!(
        Path::new("/etc/enginebox/wsl-runtime").is_file(),
        "Not a provisioned Enginebox WSL runtime"
    );
    let settings: Value = serde_json::from_slice(&std::fs::read("/etc/enginebox/settings.json")?)?;
    ensure!(
        settings["cpu"] == budget.cpu
            && settings["memory_gib"] == budget.memory_gib
            && settings["disk_gib"] == budget.disk_gib,
        "Runtime resource settings differ from enrollment"
    );
    let membership = std::fs::read_to_string("/proc/self/cgroup")?;
    ensure!(
        membership
            .lines()
            .any(|l| l.starts_with("0::/enginebox.slice/")),
        "Worker is outside its resource slice"
    );
    let memory = std::fs::read_to_string("/sys/fs/cgroup/enginebox.slice/memory.max")?
        .trim()
        .parse::<u64>()?;
    ensure!(
        memory <= u64::from(budget.memory_gib) * 1024 * 1024 * 1024,
        "Memory cgroup exceeds enrollment"
    );
    let cpu = std::fs::read_to_string("/sys/fs/cgroup/enginebox.slice/cpu.max")?;
    let limits = cpu
        .split_whitespace()
        .map(str::parse::<u64>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(
        limits.len() == 2 && limits[1] > 0 && limits[0] <= u64::from(budget.cpu) * limits[1],
        "CPU cgroup exceeds enrollment"
    );
    Ok(())
}

pub fn inventory(running: bool) -> Value {
    let ready = std::fs::read("/var/lib/enginebox/qualification.json")
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .is_some_and(|proof| proof["layerfs"] == true && proof["isolation"] == true);
    json!({"backend":"wsl2-layerfs", "workspace":ready,
        "templates":if ready { vec!["native-dev"] } else {vec![]},
        "worker_running":running,"ci":false,"codex":false,
        "cpu_memory_limits":"linux-cgroup-v2","disk_limit":"wsl-vhd-maximum"})
}

pub fn command(request: &WorkspaceRequest) -> Result<Vec<String>> {
    request.validate()?;
    let mut argv = vec![
        EBOX.into(),
        "storage".into(),
        "layerfs".into(),
        "--config".into(),
        CONFIG.into(),
        request.operation.clone(),
    ];
    if request.operation == "fork" {
        argv.push(request.project.clone());
    }
    argv.push(request.workspace.clone());
    if request.operation == "exec" {
        argv.extend([
            "--timeout".into(),
            request.timeout_seconds.to_string(),
            "--".into(),
        ]);
        argv.extend(request.argv.clone());
    }
    Ok(argv)
}

pub fn execute(input: Value) -> Result<Value> {
    ensure!(
        input["run"]["kind"] == "workspace",
        "WSL accepts template workspace operations only; host agent/CI execution is disabled"
    );
    let request: WorkspaceRequest = serde_json::from_value(input["run"]["request"].clone())?;
    let argv = command(&request)?;
    let output = enginebox_agents::process::bounded(
        &argv,
        None,
        Some(Path::new("/")),
        &[],
        Duration::from_secs(request.timeout_seconds + 150),
        250_000,
        None,
    )?;
    ensure!(
        !output.timed_out,
        "Workspace operation timed out; reconcile before retrying"
    );
    ensure!(
        output.exit_code == 0,
        "Workspace operation failed; inspect its retained receipt before retrying"
    );
    let result: Value =
        serde_json::from_str(output.output.trim()).context("Invalid LayerFS result")?;
    Ok(json!({"ok":result["ok"] != false,"backend":"wsl2-layerfs","result":result}))
}

// The daemon belongs exclusively to this distro and has a private socket and
// data root. Stopping it kills sandbox processes on lost leases/revocation,
// including detached children; no other Docker installation is addressed.
pub fn stop_runtime() -> Result<()> {
    ensure!(
        Path::new("/etc/enginebox/wsl-runtime").is_file(),
        "Not an owned runtime"
    );
    let status = Command::new("systemctl")
        .args(["stop", "enginebox-docker.service"])
        .status()?;
    ensure!(
        status.success(),
        "Could not stop the owned workspace runtime"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn commands_stay_inside_the_workspace_and_do_not_accept_host_paths() {
        let mut r = WorkspaceRequest {
            operation: "exec".into(),
            workspace: "example".into(),
            project: "welcome".into(),
            argv: vec!["bash".into(), "-c".into(), "echo 'literal; $HOME'".into()],
            timeout_seconds: 30,
        };
        let args = command(&r).unwrap();
        assert_eq!(&args[args.len() - 3..], r.argv);
        assert_eq!(args[0], EBOX);
        assert!(args.windows(2).any(|v| v == ["--config", CONFIG]));
        r.workspace = "../../etc".into();
        assert!(command(&r).is_err());
        r.workspace = "example".into();
        r.operation = "sh".into();
        assert!(command(&r).is_err());
        r.operation = "fork".into();
        assert!(command(&r).is_err());
        assert!(execute(json!({"run":{"kind":"agent"}})).is_err());
    }
}
