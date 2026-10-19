//! Explicit opt-in, disposable Linux qualification. Never run against an assigned
//! workspace: this test creates and removes only its own UUID-marked clone.
//! Required environment: ENGINEBOX_LIVE_QUALIFICATION=1, ENGINEBOX_TEST_FLEET,
//! ENGINEBOX_TEST_HOST, ENGINEBOX_TEST_RELEASE_BIN, ENGINEBOX_TEST_SOURCE
//! (ebox-base/snapshot). Optional ENGINEBOX_TEST_PREPARE=1 exercises preparation
//! on the existing-client clone; it does not claim a fresh Ubuntu installation.
//! ENGINEBOX_TEST_FRESH=1 uses the supported Ubuntu cloud image with no profiles,
//! the fleet's existing pool/network, and no source snapshot. It runs preparation
//! twice to check the real download path and repeatability without authentication.
use anyhow::{Context, Result, ensure};
use enginebox_core::{
    EngineBox, Host, Resources, bootstrap, capacity, fleet, guest, load_hosts,
    transport::{host_command, host_run},
};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{Receiver, channel},
    time::{Duration, Instant},
};
use wait_timeout::ChildExt;

struct Process {
    child: Child,
    input: Option<ChildStdin>,
    lines: Receiver<String>,
}
impl Process {
    fn start(argv: &[String]) -> Result<Self> {
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let input = child.stdin.take();
        let stdout = child.stdout.take().context("Missing child output")?;
        let (sender, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            input,
            lines,
        })
    }
    fn send(&mut self, value: &Value) -> Result<()> {
        let stdin = self.input.as_mut().context("Closed process")?;
        writeln!(stdin, "{value}")?;
        stdin.flush()?;
        Ok(())
    }
    fn response(&mut self, request: Value) -> Result<Value> {
        let id = request["id"].clone();
        self.send(&request)?;
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let line = self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))?;
            let response: Value = serde_json::from_str(&line)?;
            if response["id"] == id {
                ensure!(
                    response.get("error").is_none(),
                    "MCP protocol failure: {response}"
                );
                return Ok(response);
            }
        }
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.input.take();
        if self
            .child
            .wait_timeout(Duration::from_secs(3))
            .ok()
            .flatten()
            .is_none()
        {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
fn lease(host: &Host, binary: &str) -> Result<Process> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let guard = Process::start(&host_command(host, &[binary.into(), "lock".into()]))?;
        if guard.lines.recv_timeout(Duration::from_secs(15)).as_deref() == Ok("locked") {
            return Ok(guard);
        }
        drop(guard);
        ensure!(
            Instant::now() < deadline,
            "Host lease not acquired within 60 seconds"
        );
        std::thread::sleep(Duration::from_secs(1));
    }
}
fn host_exec(h: &Host, args: &[&str]) -> Result<Vec<u8>> {
    host_run(
        h,
        &args.iter().map(|v| (*v).into()).collect::<Vec<_>>(),
        None,
        Duration::from_secs(90),
    )
}
struct Disposable {
    host: Host,
    name: String,
    marker: String,
    directory: String,
    cleaned: bool,
}
impl Disposable {
    fn cleanup(&mut self) -> Result<()> {
        let bx = self.host.engine_box();
        let items = bx.query(&format!(
            "/1.0/instances?project={}&recursion=1",
            self.host.project
        ))?;
        if let Some(vm) = items
            .as_array()
            .context("Invalid inventory")?
            .iter()
            .find(|v| v["name"] == self.name)
        {
            ensure!(
                vm["config"]["user.enginebox.validation"] == self.marker,
                "Refusing cleanup of foreign workspace"
            );
            ensure!(
                vm["config"]["user.enginebox.owner"] == self.host.project,
                "Workspace ownership changed"
            );
            bx.call(&["delete", &self.name, "--force"])?;
        }
        let marker = host_exec(&self.host, &["cat", &format!("{}/marker", self.directory)])?;
        ensure!(
            marker == self.marker.as_bytes(),
            "Refusing cleanup of foreign staging directory"
        );
        host_exec(
            &self.host,
            &["rm", "--recursive", "--force", "--", &self.directory],
        )?;
        self.cleaned = true;
        Ok(())
    }
}
impl Drop for Disposable {
    fn drop(&mut self) {
        if !self.cleaned
            && let Err(error) = self.cleanup()
        {
            eprintln!(
                "Disposable qualification cleanup requires inspection for {}: {error:#}",
                self.name
            );
        }
    }
}
fn ready(bx: &EngineBox, name: &str) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut last = String::new();
    while Instant::now() < deadline {
        let result = bx.execute_with_input(
            name,
            &["enginebox-host".into(), "readiness".into()],
            &serde_json::to_vec(&bx.guest)?,
            Duration::from_secs(15),
        );
        match result.and_then(|bytes| Ok(serde_json::from_slice::<Value>(&bytes)?)) {
            Ok(value) if value["ready"] == true => return Ok(value),
            Ok(value) => last = value.to_string(),
            Err(error) => last = format!("{error:#}"),
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    anyhow::bail!("Guest readiness timed out: {last}")
}
fn mcp(bx: &EngineBox, name: &str, browser: bool) -> Result<Value> {
    let args = if browser {
        vec![
            "enginebox-guest",
            "browser",
            "--client",
            "codex",
            "--profile",
            "rust-qualification",
        ]
    } else {
        vec!["env", "ENGINEBOX_GUEST=1", "enginebox-mcp"]
    };
    let args = bx.exec_args(
        name,
        &args.into_iter().map(str::to_owned).collect::<Vec<_>>(),
    )?;
    let mut process = Process::start(&bx.command(&args))?;
    let init = process.response(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"enginebox-disposable-qualification","version":env!("CARGO_PKG_VERSION")}}}))?;
    process.send(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))?;
    let list =
        process.response(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}))?;
    let names = list["result"]["tools"]
        .as_array()
        .context("Missing MCP tools")?
        .iter()
        .map(|v| v["name"].clone())
        .collect::<Vec<_>>();
    if browser {
        ensure!(
            names.iter().any(|v| v == "browser_navigate"),
            "Browser navigation tool missing"
        );
        let navigation = process.response(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"browser_navigate","arguments":{"url":"about:blank"}}}))?;
        ensure!(
            navigation["result"]["isError"] != true,
            "Browser launch failed: {navigation}"
        );
    } else {
        ensure!(
            names.len() == 4
                && names
                    .iter()
                    .all(|v| v.as_str().is_some_and(|s| s.starts_with("desktop_"))),
            "Guest MCP unexpectedly exposes host controls"
        );
    }
    Ok(json!({"server":init["result"]["serverInfo"],"tools":names,"headless_navigation":browser}))
}

#[test]
#[ignore = "explicitly authorized disposable Linux VM; see module environment contract"]
fn disposable_linux_guest_qualification() -> Result<()> {
    ensure!(
        std::env::var("ENGINEBOX_LIVE_QUALIFICATION").as_deref() == Ok("1"),
        "Explicit live-test opt-in required"
    );
    let config = std::env::var("ENGINEBOX_TEST_FLEET")?;
    let id = std::env::var("ENGINEBOX_TEST_HOST")?;
    let host = load_hosts(Path::new(&config))?
        .into_iter()
        .find(|h| h.id == id)
        .context("Unknown qualification host")?;
    let release = std::env::var("ENGINEBOX_TEST_RELEASE_BIN")?;
    ensure!(
        release.starts_with('/')
            && !release.contains("..")
            && !release.chars().any(char::is_control),
        "Invalid candidate binary directory"
    );
    let fresh = std::env::var("ENGINEBOX_TEST_FRESH").as_deref() == Ok("1");
    let source = if fresh {
        "images:ubuntu/24.04/cloud".to_owned()
    } else {
        std::env::var("ENGINEBOX_TEST_SOURCE")?
    };
    let bx = host.engine_box();
    let (template, mut source_before) = if fresh {
        let network = host
            .network
            .as_ref()
            .context("Fresh test requires an explicit existing fleet network")?;
        let observed = bx.query(&format!("/1.0/networks/{network}?project={}", host.project))?;
        ensure!(
            observed["managed"] == true
                && observed["type"] == "bridge"
                && observed["status"] == "Created",
            "Fresh test requires an existing managed bridge; it creates no host infrastructure"
        );
        (Value::Null, Value::Null)
    } else {
        let (base, snapshot) = source
            .split_once('/')
            .context("Source must be BASE/SNAPSHOT")?;
        enginebox_core::identifier(base, true)?;
        enginebox_core::identifier(snapshot, false)?;
        let template = json!({"source":base,"snapshot":snapshot});
        let observed = fleet::template_snapshot(&host, &template)?;
        ensure!(
            observed["config"]["user.enginebox.owner"] == host.project,
            "Clean snapshot owner mismatch"
        );
        (template, observed)
    };
    let resources = Resources {
        cpu: 2,
        memory_gib: 2,
        disk_gib: 30,
    };
    if !fresh {
        let (_, disk) = fleet::root_device(&bx, &source_before)?;
        ensure!(
            disk["pool"] == host.pool
                && capacity::resource_bytes(&disk["size"], u64::MAX)
                    <= resources.disk_gib * enginebox_core::GIB,
            "Source disk cannot fit qualification allocation"
        );
    }
    let candidate = format!("{release}/enginebox-host");
    let _lease = lease(&host, &candidate)?;
    let facts: Value = serde_json::from_slice(&host_exec(
        &host,
        &[
            &candidate,
            "inventory",
            "--pool",
            &host.pool,
            "--project",
            &host.project,
        ],
    )?)?;
    ensure!(
        capacity::resource_fits(&host, &facts, resources)?,
        "Disposable clone does not fit admitted fleet capacity"
    );
    let marker = uuid::Uuid::new_v4().simple().to_string();
    let name = format!("ebox-rust-qual-{}", &marker[..12]);
    let directory = format!("/var/tmp/enginebox-qualification-{marker}");
    host_exec(&host, &["mkdir", "--mode=0755", "--", &directory])?;
    host_run(
        &host,
        &["tee".into(), format!("{directory}/marker")],
        Some(marker.as_bytes()),
        Duration::from_secs(10),
    )?;
    let mut disposable = Disposable {
        host: host.clone(),
        name: name.clone(),
        marker: marker.clone(),
        directory: directory.clone(),
        cleaned: false,
    };
    eprintln!("Qualifying disposable {name} after fleet capacity admission");
    let mut hashes = serde_json::Map::new();
    for binary in ["enginebox-host", "enginebox-guest", "enginebox-mcp"] {
        let from = format!("{release}/{binary}");
        let to = format!("{directory}/{binary}");
        host_exec(
            &host,
            &[
                "install", "-o", "root", "-g", "root", "-m0755", "--", &from, &to,
            ],
        )?;
        let result = String::from_utf8(host_exec(&host, &["sha256sum", "--", &to])?)?;
        hashes.insert(
            binary.into(),
            json!(
                result
                    .split_whitespace()
                    .next()
                    .context("Missing source hash")?
            ),
        );
    }
    let started = Instant::now();
    let mut create = if fresh {
        vec![
            "init",
            &source,
            &name,
            "--vm",
            "--no-profiles",
            "--storage",
            &host.pool,
            "--network",
            host.network.as_deref().unwrap(),
        ]
    } else {
        vec![
            "copy",
            &source,
            &name,
            "--stateless",
            "--storage",
            &host.pool,
        ]
    };
    let owner = format!("user.enginebox.owner={}", host.project);
    let validation = format!("user.enginebox.validation={marker}");
    create.extend([
        "-c",
        &owner,
        "-c",
        "migration.stateful=true",
        "-c",
        "limits.cpu=2",
        "-c",
        "limits.memory=2GiB",
        "-c",
        "user.enginebox.fleet=true",
        "-c",
        "user.enginebox.template=rust-qualification",
        "-c",
        "user.enginebox.pool.state=preparing",
        "-c",
        "user.enginebox.clean-base=false",
        "-c",
        "boot.autostart=false",
        "-c",
        &validation,
    ]);
    if fresh {
        create.extend(["-c","cloud-init.user-data=#cloud-config\npreserve_hostname: true\n","-c","cloud-init.network-config=version: 2\nethernets:\n  enginebox:\n    match:\n      name: 'en*'\n    dhcp4: true\n    dhcp-identifier: mac\n"]);
    }
    bx.run(
        &create.into_iter().map(str::to_owned).collect::<Vec<_>>(),
        Duration::from_secs(900),
    )?;
    let vm = bx.instance(&name)?;
    let (device, disk) = fleet::root_device(&bx, &vm)?;
    ensure!(
        disk["pool"] == host.pool,
        "Copied workspace changed storage pool"
    );
    let mode = if vm["devices"].get(&device).is_some() {
        "set"
    } else {
        "override"
    };
    bx.call(&[
        "config",
        "device",
        mode,
        &name,
        &device,
        "size=30GiB",
        "size.state=4GiB",
    ])?;
    bx.call(&["start", &name])?;
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if bx.call(&["exec", &name, "--", "true"]).is_ok() {
            break;
        }
        ensure!(Instant::now() < deadline, "Guest agent did not start");
        std::thread::sleep(Duration::from_secs(1));
    }
    let migration: Value = serde_json::from_slice(&host_exec(
        &host,
        &[
            &candidate,
            "sync-guest-tools",
            "--project",
            &host.project,
            "--name",
            &name,
            "--binary-dir",
            &directory,
            "--already-locked",
        ],
    )?)?;
    let mut readiness = if fresh {
        Value::Null
    } else {
        ready(&bx, &name)?
    };
    let migration_ready_seconds = started.elapsed().as_secs_f64();
    for binary in ["enginebox-host", "enginebox-guest", "enginebox-mcp"] {
        let version = bx.call(&["exec", &name, "--", binary, "--version"])?;
        ensure!(
            String::from_utf8(version)?.trim() == format!("{binary} {}", env!("CARGO_PKG_VERSION")),
            "Helper version mismatch"
        );
        let hash = String::from_utf8(bx.call(&[
            "exec",
            &name,
            "--",
            "sha256sum",
            "--",
            &format!("/usr/local/bin/{binary}"),
        ])?)?;
        ensure!(
            hash.split_whitespace().next() == hashes[binary].as_str(),
            "Installed helper hash mismatch"
        );
    }
    if !fresh {
        guest::assert_remote_idle(&host, &name)?;
        bx.call(&[
            "exec",
            &name,
            "--",
            "enginebox-host",
            "clean-base-check",
            "--home",
            &host.guest.home,
        ])?;
    }
    let mut preparation = Value::Null;
    if fresh || std::env::var("ENGINEBOX_TEST_PREPARE").as_deref() == Ok("1") {
        let mut c = bootstrap::defaults();
        c["host"]["transport"] = json!("local");
        c["storage"]["pool"] = json!(host.pool);
        c["network"]["name"] = json!(host.network.as_deref().unwrap_or("eboxbr0"));
        c["incus"]["project"] = json!(host.project);
        c["workspace"]["name"] = json!(name);
        c["workspace"]["cpu"] = json!(2);
        c["workspace"]["memory"] = json!("2GiB");
        c["workspace"]["state_disk"] = json!("4GiB");
        c["workspace"]["user"] = json!(host.guest.user);
        c["workspace"]["uid"] = json!(host.guest.uid);
        c["workspace"]["gid"] = json!(host.guest.gid);
        c["workspace"]["home"] = json!(host.guest.home);
        c["workspace"]["path"] = json!(host.guest.workspace);
        let c = bootstrap::normalize(c)?;
        eprintln!("Preparing {name}; fresh Ubuntu image: {fresh}");
        let before = Instant::now();
        let output = bx.run_input(
            &[
                "exec".into(),
                name.clone(),
                "--".into(),
                "enginebox-host".into(),
                "prepare-guest".into(),
            ],
            &serde_json::to_vec(&c)?,
            Duration::from_secs(1800),
        )?;
        let details: Value = serde_json::from_slice(&output)?;
        preparation = json!({"basis":if fresh { "fresh images:ubuntu/24.04/cloud VM; no inherited profile or native client installation" } else { "existing native clients in clean clone; not a fresh Ubuntu installation" },"seconds":before.elapsed().as_secs_f64(),"result":details});
        bx.call(&[
            "exec",
            &name,
            "--",
            "test",
            "-f",
            "/etc/cloud/cloud-init.disabled",
        ])?;
        bx.call(&["restart", &name, "--force"])?;
        readiness = ready(&bx, &name)?;
        if fresh {
            eprintln!("Rechecking preparation idempotence in {name}");
            let before = Instant::now();
            let output = bx.run_input(
                &[
                    "exec".into(),
                    name.clone(),
                    "--".into(),
                    "enginebox-host".into(),
                    "prepare-guest".into(),
                ],
                &serde_json::to_vec(&c)?,
                Duration::from_secs(1800),
            )?;
            preparation["repeat"] = json!({"seconds":before.elapsed().as_secs_f64(),"result":serde_json::from_slice::<Value>(&output)?});
            ready(&bx, &name)?;
        }
        bx.call(&[
            "exec",
            &name,
            "--",
            "enginebox-host",
            "clean-base-check",
            "--home",
            &host.guest.home,
        ])?;
    }
    let installed_versions: Value = serde_json::from_slice(&bx.call(&[
        "exec",
        &name,
        "--",
        "cat",
        "/opt/enginebox-runtime/installed-versions.json",
    ])?)?;
    let computer = mcp(&bx, &name, false)?;
    let browser = mcp(&bx, &name, true)?;
    guest::assert_remote_idle(&host, &name)?;
    let pause = Instant::now();
    ensure!(
        bx.lifecycle(&name, "pause")?["status"] == "Frozen",
        "Pause failed"
    );
    let pause_seconds = pause.elapsed().as_secs_f64();
    let resume = Instant::now();
    ensure!(
        bx.lifecycle(&name, "resume")?["status"] == "Running",
        "Resume failed"
    );
    ready(&bx, &name)?;
    let resume_seconds = resume.elapsed().as_secs_f64();
    let mut source_after = if fresh {
        Value::Null
    } else {
        fleet::template_snapshot(&host, &template)?
    };
    // Incus observation fields may change when a CoW snapshot is copied:
    // `size` includes backend physical/shared extent accounting.
    if !fresh {
        for key in ["last_used_at", "size"] {
            source_before.as_object_mut().unwrap().remove(key);
            source_after.as_object_mut().unwrap().remove(key);
        }
        ensure!(
            source_before == source_after,
            "Original clean snapshot changed"
        );
    }
    disposable.cleanup()?;
    let report = json!({"passed":true,"name":name,"version":env!("CARGO_PKG_VERSION"),"allocation":resources,"migration":migration,"helper_sha256":hashes,"readiness":readiness,"migration_ready_seconds":migration_ready_seconds,"preparation":preparation,"installed_versions":installed_versions,"computer_mcp":computer,"browser_mcp":browser,"pause_seconds":pause_seconds,"resume_ready_seconds":resume_seconds,"source_snapshot_unchanged":if fresh {Value::Null} else {json!(true)},"cleanup":true,"inference_tested":false});
    if let Ok(path) = std::env::var("ENGINEBOX_TEST_REPORT") {
        enginebox_core::state::write_json(Path::new(&path), &report)?;
    }
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
