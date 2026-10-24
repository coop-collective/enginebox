//! Native contributor identity, budgets and outbound work execution.
use crate::{
    native_node,
    release::{atomic_json, private_dir, reject_symlink_components},
};
use anyhow::{Context, Result, ensure};
use enginebox_node_protocol::{Budget, Enrollment, HEARTBEAT_SECONDS, Workloads};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema: u32,
    pub controller: String,
    pub id: String,
    pub token: String,
    pub enrolled: bool,
    pub budget: Budget,
    pub workloads: Workloads,
}
fn hash(value: &str) -> String {
    hex::encode(Sha256::digest(serde_json::to_vec(value).unwrap()))
}
fn endpoint(action: &str, test: bool) -> String {
    format!("/{}/node/{action}", if test { "v1" } else { "api" })
}
async fn post(c: &Config, action: &str, body: Value, test: bool) -> Result<Value> {
    native_node::post(
        &native_node::client()?,
        &native_node::controller_origin(&c.controller, test)?,
        &endpoint(action, test),
        &c.token,
        None,
        body,
    )
    .await
}
pub async fn register(
    path: &Path,
    controller: &str,
    code: &str,
    budget: Budget,
    test: bool,
) -> Result<Value> {
    budget.validate()?;
    let origin = native_node::controller_origin(controller, test)?;
    let (id, secret) = code.split_once('.').context("Invalid registration code")?;
    ensure!(
        enginebox_node_protocol::id(id) && enginebox_node_protocol::hash(secret),
        "Invalid registration code"
    );
    let parent = path.parent().context("Config parent required")?;
    private_dir(parent)?;
    reject_symlink_components(path)?;
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(parent.join("register.lock"))?;
    lock.try_lock_exclusive()?;
    let mut c = if path.exists() {
        let c: Config = serde_json::from_slice(&std::fs::read(path)?)?;
        ensure!(
            c.id == id && c.controller == origin.as_str() && c.budget == budget,
            "This installation belongs to a different registration or budget"
        );
        c
    } else {
        Config {
            schema: 1,
            controller: origin.to_string(),
            id: id.to_owned(),
            token: format!(
                "{id}.{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            ),
            enrolled: false,
            budget,
            workloads: Workloads {
                ci: false,
                agents: false,
            },
        }
    };
    if c.enrolled {
        return Ok(json!({"id":c.id,"enrolled":true,"existing":true}));
    }
    atomic_json(path, &c)?;
    let enrollment = Enrollment {
        secret_hash: hash(&c.token),
        budget: c.budget.clone(),
        os: std::env::consts::OS.into(),
        architecture: std::env::consts::ARCH.into(),
    };
    let response = native_node::post(
        &native_node::client()?,
        &origin,
        &endpoint("enroll", test),
        code,
        None,
        serde_json::to_value(enrollment)?,
    )
    .await?;
    ensure!(
        response["id"] == id && response["enrolled"] == true,
        "Unexpected enrollment response"
    );
    c.workloads = serde_json::from_value(response["workloads"].clone())?;
    c.enrolled = true;
    atomic_json(path, &c)?;
    Ok(json!({"id":c.id,"enrolled":true,"budget":c.budget,"workloads":c.workloads}))
}
fn executable(name: &str) -> Option<PathBuf> {
    let file = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    };
    let mut paths =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect::<Vec<_>>();
    if cfg!(windows) {
        if let Some(p) = std::env::var_os("ProgramFiles") {
            paths.extend([
                PathBuf::from(&p).join("PowerShell/7"),
                PathBuf::from(p).join("Git/cmd"),
            ]);
        }
        if let Some(p) = std::env::var_os("LOCALAPPDATA") {
            paths.push(PathBuf::from(p).join("Microsoft/WinGet/Links"));
        }
    }
    paths
        .into_iter()
        .map(|p| p.join(&file))
        .find(|p| p.is_absolute() && p.is_file())
}
fn bytes_used(path: &Path, remaining: &mut usize) -> Result<u64> {
    ensure!(*remaining > 0, "Workspace file inventory limit reached");
    *remaining -= 1;
    let m = std::fs::symlink_metadata(path)?;
    if m.file_type().is_symlink() {
        return Ok(m.len());
    }
    let mut bytes = m.len();
    if m.is_dir() {
        for e in std::fs::read_dir(path)? {
            bytes = bytes
                .checked_add(bytes_used(&e?.path(), remaining)?)
                .context("Workspace size overflow")?;
        }
    }
    Ok(bytes)
}
fn check_disk(root: &Path, budget: &Budget) -> Result<()> {
    let used = bytes_used(root, &mut 250_000)?;
    ensure!(
        used <= u64::from(budget.disk_gib) * 1024 * 1024 * 1024,
        "Workspace disk allowance exceeded"
    );
    ensure!(
        fs2::available_space(root)? >= 1024 * 1024 * 1024,
        "Less than 1 GiB free on the workspace volume"
    );
    Ok(())
}

#[cfg(windows)]
fn enforce_budget(b: &Budget) -> Result<()> {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::{JobObjects::*, Threading::GetCurrentProcess},
    };
    let cores = std::thread::available_parallelism()?.get();
    ensure!(
        b.cpu as usize <= cores,
        "CPU allowance exceeds this machine"
    );
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        ensure!(!job.is_null(), "Cannot create resource job");
        let mut memory: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        memory.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_JOB_MEMORY;
        memory.JobMemoryLimit = (u64::from(b.memory_gib) * 1024 * 1024 * 1024) as usize;
        let mut cpu: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION = std::mem::zeroed();
        cpu.ControlFlags =
            JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
        cpu.Anonymous.CpuRate = ((b.cpu as usize * 10000) / cores).clamp(1, 10000) as u32;
        let success = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&memory as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of_val(&memory) as u32,
        ) != 0
            && SetInformationJobObject(
                job,
                JobObjectCpuRateControlInformation,
                (&cpu as *const JOBOBJECT_CPU_RATE_CONTROL_INFORMATION).cast(),
                std::mem::size_of_val(&cpu) as u32,
            ) != 0
            && AssignProcessToJobObject(job, GetCurrentProcess()) != 0;
        if !success {
            CloseHandle(job);
            anyhow::bail!("Cannot enforce CPU/memory/process-tree limits; no workload started");
        }
        // The non-inherited handle stays in the supervisor. OS closure kills the tree.
    }
    Ok(())
}
#[cfg(not(windows))]
fn enforce_budget(b: &Budget) -> Result<()> {
    crate::wsl::enforce(b)
}

fn worker_environment(command: &mut Command) -> Result<()> {
    command.env_clear();
    for key in [
        "PATH",
        "SystemRoot",
        "WINDIR",
        "COMSPEC",
        "PATHEXT",
        "USERPROFILE",
        "LOCALAPPDATA",
        "APPDATA",
        "SystemDrive",
        "TEMP",
        "TMP",
        "LOCALAPPDATA",
        "ProgramFiles",
        "USERPROFILE",
        "JAVA_HOME",
        "ANDROID_HOME",
        "ANDROID_SDK_ROOT",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    let mut paths =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect::<Vec<_>>();
    for name in ["pwsh", "git"] {
        if let Some(exe) = executable(name)
            && let Some(parent) = exe.parent()
        {
            paths.push(parent.to_owned());
        }
    }
    command.env("PATH", std::env::join_paths(paths)?);
    Ok(())
}
async fn tool_ready(name: &str, args: &[&str]) -> bool {
    let Some(exe) = executable(name) else {
        return false;
    };
    tokio::time::timeout(
        Duration::from_secs(3),
        Command::new(exe)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await
    .is_ok_and(|r| r.is_ok_and(|s| s.success()))
}
async fn tools(_root: &Path, c: &Config, running: bool) -> Value {
    if cfg!(target_os = "linux") {
        return crate::wsl::inventory(running);
    }
    let ci = c.workloads.ci
        && tool_ready(
            "pwsh",
            &[
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "if ($PSVersionTable.PSVersion -lt [version]'7.3') { exit 1 }",
            ],
        )
        .await
        && tool_ready("git", &["--version"]).await;
    json!({"ci":ci,
        "codex":false,"workspace":false,"backend":"legacy-windows-ci","worker_running":running,
        "cpu_memory_limits":"windows-job-object","disk_limit":"workspace-watchdog","streaming_qualified":false})
}
pub async fn run(path: &Path, test: bool) -> Result<()> {
    reject_symlink_components(path)?;
    let c: Config = serde_json::from_slice(&std::fs::read(path)?)?;
    ensure!(c.enrolled && c.schema == 1, "Machine is not registered");
    c.budget.validate()?;
    enforce_budget(&c.budget)?;
    let private = path.parent().context("Missing config directory")?;
    private_dir(private)?;
    let root = private
        .parent()
        .context("Missing installation root")?
        .join("work");
    private_dir(&root)?;
    // Provider state belongs to individual template sandboxes.
    private_dir(&private.join("receipts"))?;
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(private.join("worker.lock"))?;
    lock.try_lock_exclusive()?;
    let session = uuid::Uuid::new_v4().to_string();
    let hello = post(&c, "hello", json!({"session":session}), test).await?;
    ensure!(
        hello["id"] == c.id
            && hello["session"] == session
            && hello["budget"] == serde_json::to_value(&c.budget)?,
        "Controller returned a different machine budget or identity"
    );
    let epoch = hello["epoch"].as_u64().context("Missing epoch")?;
    let result: Result<()>=async {
        loop {
            let inventory=tools(&root,&c,false).await;
            let heartbeat=post(&c,"heartbeat",json!({"session":session,"epoch":epoch,"tools":inventory}),test).await?;
            atomic_json(&private.join("status.json"),&json!({"online":true,"id":c.id,"controller":c.controller,"budget":c.budget,"tools":inventory,"blocked":heartbeat["blocked"]}))?;
            if heartbeat["blocked"]!=true {
                check_disk(&root,&c.budget)?;
                let poll=post(&c,"poll",json!({"session":session,"epoch":epoch}),test).await?;
                if poll["job"].is_object() {
                    let job=&poll["job"];let id=job["id"].as_str().context("Missing job identity")?;
                    ensure!(enginebox_node_protocol::id(id) && job["node"]==c.id,"Unexpected assignment");
                    let receipt=private.join("receipts").join(format!("{id}.json"));
                    ensure!(!receipt.exists(),"A prior execution needs reconciliation; never replay it");
                    atomic_json(&receipt,&json!({"state":"started","id":id}))?;
                    let input=json!({"root":root,"node":c.id,"run":job["run"]});
                    let mut command=Command::new(std::env::current_exe()?);
                    worker_environment(&mut command)?;
                    let mut child=command.arg("execute-worker")
                        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).spawn()?;
                    child.stdin.take().context("Missing worker input")?.write_all(&serde_json::to_vec(&input)?).await?;
                    let stdout=child.stdout.take().context("Missing worker output")?;
                    let reader=tokio::spawn(async move {let mut out=Vec::new();stdout.take(320_001).read_to_end(&mut out).await?;Ok::<_,std::io::Error>(out)});
                    let mut running_tools=inventory.clone();running_tools["worker_running"]=json!(true);
                    let outcome: Result<Value>=async {
                        loop {
                            if let Some(status)=child.try_wait()? {
                                ensure!(status.success(),"Worker process failed");
                                let bytes=reader.await??;ensure!(bytes.len()<=320_000,"Worker result exceeds limit");
                                return Ok(serde_json::from_slice(&bytes)?);
                            }
                            check_disk(&root,&c.budget)?;
                            let heartbeat=post(&c,"heartbeat",json!({"session":session,"epoch":epoch,"tools":running_tools}),test).await?;
                            ensure!(heartbeat["blocked"]!=true,"Controller fenced the worker");
                            ensure!(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs()<job["deadline"].as_u64().unwrap_or(0),"Assignment expired");
                            tokio::time::sleep(Duration::from_secs(HEARTBEAT_SECONDS)).await;
                        }
                    }.await;
                    if outcome.is_err() {
                        let _=child.kill().await;let _=child.wait().await;
                        // Exit the supervisor too: its Windows Job Object owns descendants.
                        return outcome.map(|_|());
                    }
                    let output=outcome?;atomic_json(&receipt,&json!({"state":"finished","result":output}))?;
                    post(&c,"result",json!({"session":session,"epoch":epoch,"job":id,"result":output}),test).await?;
                    atomic_json(&receipt,&json!({"state":"acknowledged"}))?;
                }
            }
            tokio::time::sleep(Duration::from_secs(HEARTBEAT_SECONDS)).await;
        }
    }.await;
    if cfg!(target_os = "linux") {
        crate::wsl::stop_runtime()?;
    }
    atomic_json(
        &private.join("status.json"),
        &json!({"online":false,"id":c.id,"error":"Worker stopped; check controller status and local resource availability"}),
    )?;
    result
}

pub fn execute(input: Value) -> Result<Value> {
    if cfg!(target_os = "linux") {
        return crate::wsl::execute(input);
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::{JobObjects::*, Threading::GetCurrentProcess},
        };
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            ensure!(!job.is_null(), "Cannot contain worker descendants");
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            ) == 0
                || AssignProcessToJobObject(job, GetCurrentProcess()) == 0
            {
                CloseHandle(job);
                anyhow::bail!("Worker containment failed");
            }
            // This subprocess owns the handle until it exits, including on failure.
        }
    }
    let root = PathBuf::from(input["root"].as_str().context("Worker root required")?);
    reject_symlink_components(&root)?;
    ensure!(root.is_absolute() && root.is_dir(), "Invalid worker root");
    let id = input["node"].as_str().context("Node required")?;
    ensure!(enginebox_node_protocol::id(id), "Invalid node id");
    let run: enginebox_node_protocol::Run = serde_json::from_value(input["run"].clone())?;
    if run.kind == "ci" {
        let request: enginebox_ci::Request = serde_json::from_value(run.request)?;
        ensure!(
            request.request_id == run.request_id,
            "CI request identities differ"
        );
        let node = enginebox_ci::Node {
            schema_version: 1,
            id: id.to_owned(),
            target: "local".into(),
            os: enginebox_ci::model::Os::current()?,
            architecture: enginebox_ci::model::architecture()?.into(),
            image: "windows-native-v1".into(),
            capabilities: vec!["pwsh".into()],
            disabled: false,
        };
        return enginebox_ci::executor::execute_at(&node, &request, &root.join("ci"));
    }
    anyhow::bail!("Host agent execution is disabled. Use a WSL template workspace.")
}
