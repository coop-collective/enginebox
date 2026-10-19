//! Explicit owner-authorized inference qualification. Creates its own Incus
//! project and VM, uses access-only credentials, and never refreshes live homes.
//! Run as the account that owns the source controller homes on a qualified Linux
//! node with the candidate enginebox-host visible in sudo's PATH. See the report
//! documentation for the private mount-namespace staging procedure.
use anyhow::{Context, Result, ensure};
use enginebox_agents::{AgentService, delegation, security};
use enginebox_core::{Fleet, Host, Resources, capacity, fleet, guest, transport::host_run};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn run(h: &Host, argv: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
    host_run(
        h,
        &argv.iter().map(|s| (*s).into()).collect::<Vec<_>>(),
        input,
        Duration::from_secs(180),
    )
}
fn lease(h: &Host) -> Result<enginebox_core::HostOperationLock> {
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        match enginebox_core::operation_lock(h) {
            Ok(lock) => return Ok(lock),
            Err(error) if Instant::now() >= until => return Err(error),
            Err(_) => std::thread::sleep(Duration::from_secs(1)),
        }
    }
}
struct Fixture {
    host: Host,
    name: String,
    marker: String,
    created: bool,
    cleaned: bool,
}
impl Fixture {
    fn cleanup(&mut self) -> Result<()> {
        if !self.created || self.cleaned {
            return Ok(());
        }
        let _lock = lease(&self.host)?;
        let bx = self.host.engine_box();
        let project = bx.query(&format!("/1.0/projects/{}", self.host.project))?;
        ensure!(
            project["config"]["user.enginebox.validation"] == self.marker,
            "Foreign test project; cleanup refused"
        );
        let instances = bx.query(&format!(
            "/1.0/instances?project={}&recursion=1",
            self.host.project
        ))?;
        for vm in instances.as_array().context("Invalid project inventory")? {
            ensure!(
                vm["name"] == self.name && vm["config"]["user.enginebox.validation"] == self.marker,
                "Foreign instance; cleanup refused"
            );
            bx.call(&["delete", &self.name, "--force"])?;
        }
        let profiles = bx.query(&format!(
            "/1.0/profiles?project={}&recursion=1",
            self.host.project
        ))?;
        if profiles
            .as_array()
            .context("Invalid profile inventory")?
            .iter()
            .any(|p| p["name"] == "qualification")
        {
            bx.call(&["profile", "delete", "qualification"])?;
        }
        run(
            &self.host,
            &["incus", "project", "delete", &self.host.project],
            None,
        )?;
        self.cleaned = true;
        Ok(())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            eprintln!(
                "Fixture cleanup requires inspection: {}: {error:#}",
                self.host.project
            );
        }
    }
}
fn copy_access(source: &Path, state_root: &Path, providers: &[&str]) -> Result<Value> {
    let mut report = serde_json::Map::new();
    for &provider in providers {
        let home = source.join(provider);
        let _lock = security::Lease::acquire(
            &home,
            &format!(".local/state/enginebox/auth-{provider}.lock"),
        )?;
        let raw = security::read_json(&home, delegation::native_file(provider))?
            .context("Native connection missing")?;
        let material = delegation::material(provider, &raw)?;
        let expiry = material["expires_at"]
            .as_f64()
            .context("Access expiry unknown; do not guess or refresh")?;
        let left = expiry - chrono::Utc::now().timestamp() as f64;
        ensure!(
            left > 1800.,
            "{provider} access expires within 30 minutes; let the existing owner renew before this test"
        );
        let target = state_root.join("agents/homes").join(provider);
        security::private_dir(&target)?;
        if provider == "claude" {
            security::write_json(
                &target,
                delegation::TOKEN_FILE,
                &json!({"kind":"qualification_access_only","value":material["content"]["CLAUDE_CODE_OAUTH_TOKEN"],"expires_at":expiry}),
            )?;
        } else {
            security::write_json(
                &target,
                delegation::native_file(provider),
                &material["content"],
            )?;
        }
        report.insert(
            provider.into(),
            json!({"access_only":true,"expires_in_seconds":left as u64,"live_home_written":false}),
        );
    }
    Ok(Value::Object(report))
}
fn run_files(root: &Path) -> Result<std::collections::BTreeSet<std::ffi::OsString>> {
    std::fs::read_dir(root.join("agent-runs"))?
        .filter_map(|entry| match entry {
            Ok(entry) if entry.path().extension().is_some_and(|ext| ext == "json") => {
                Some(Ok(entry.file_name()))
            }
            Ok(_) => None,
            Err(error) => Some(Err(error.into())),
        })
        .collect()
}
fn wait_run(
    service: &AgentService,
    root: &Path,
    host: &Host,
    name: &str,
    request: Value,
) -> Result<Value> {
    let admitted_before = run_files(root)?;
    let admission_deadline = Instant::now() + Duration::from_secs(60);
    let queued = loop {
        match service.launch(&host.id, name, request.clone()) {
            Ok(queued) => break queued,
            // This exact local guard runs before attach/save/spawn in launch.
            // Never retry an admitted run, provider rejection or transport error.
            Err(error)
                if error.to_string()
                    == "Cannot acquire host operation lock; another controller or installer may be active"
                    && service.pending() == 0
                    && Instant::now() < admission_deadline =>
            {
                ensure!(
                    run_files(root)? == admitted_before,
                    "Admission error left a run record; never replay the request"
                );
                std::thread::sleep(Duration::from_secs(1));
            }
            Err(error) => return Err(error),
        }
    };
    let id = queued["id"].as_str().context("Run ID missing")?;
    let until = Instant::now() + Duration::from_secs(210);
    loop {
        let record = service.get_run(&host.id, name, id)?;
        if !["queued", "running"].contains(&record["status"].as_str().unwrap_or("")) {
            return Ok(record);
        }
        ensure!(
            Instant::now() < until,
            "Native run exceeded the qualification deadline; disposable fixture will be removed"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}
fn summarized(record: &Value, seconds: f64) -> Value {
    let output = record["result"]["output"]
        .as_str()
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut summary = json!({"run_id":record["id"],"status":record["status"],"exit_code":record["result"]["exit_code"],"timed_out":record["result"]["timed_out"],"output_truncated":record["result"]["output_truncated"],"execution_uncertain":record["execution_uncertain"],"native_session_id":record["native_session_id"],"seconds":seconds,"quota_blocked":output.contains("usage limit") || output.contains("usage-limit") || output.contains("quota exceeded") || output.contains("insufficient credits")});
    for event in record["result"]["output"]
        .as_str()
        .unwrap_or("")
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
    {
        if event["type"] == "result" && event["total_cost_usd"].is_number() {
            // Native resumed-session totals are cumulative, not an account
            // balance or an incremental cost to sum across these records.
            summary["native_reported_session_cost_usd"] = event["total_cost_usd"].clone();
        }
    }
    summary
}
fn guest_json(h: &Host, name: &str, script: &str) -> Result<Value> {
    let raw = h.engine_box().execute(
        name,
        &["python3".into(), "-I".into(), "-c".into(), script.into()],
        Duration::from_secs(30),
    )?;
    Ok(serde_json::from_slice(&raw)?)
}
fn browser_calls(record: &Value) -> usize {
    record["result"]["output"]
        .as_str()
        .unwrap_or("")
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| {
            event["item"]["type"] == "mcp_tool_call"
                && event["item"]["server"] == "enginebox-browser"
                || event["message"]["content"]
                    .as_array()
                    .is_some_and(|content| {
                        content.iter().any(|item| {
                            item["type"] == "tool_use"
                                && item["name"].as_str().is_some_and(|name| {
                                    name.starts_with("mcp__enginebox-browser__")
                                })
                        })
                    })
        })
        .count()
}

#[test]
#[ignore = "explicit owner-authorized subscription inference; disposable Linux VM and fresh private state required"]
fn real_native_launch_and_exact_resume() -> Result<()> {
    ensure!(cfg!(target_os = "linux"), "Linux qualification only");
    ensure!(
        std::env::var("ENGINEBOX_NATIVE_RUN_QUALIFICATION").as_deref() == Ok("1"),
        "Explicit inference opt-in required"
    );
    let config = std::env::var("ENGINEBOX_TEST_FLEET")?;
    let selected = std::env::var("ENGINEBOX_TEST_HOST")?;
    let source_host = enginebox_core::load_hosts(Path::new(&config))?
        .into_iter()
        .find(|h| h.id == selected)
        .context("Unknown source host")?;
    ensure!(
        source_host.target == "local",
        "Run locally on the qualified node"
    );
    let source = std::env::var("ENGINEBOX_TEST_SOURCE")?;
    let (base, snapshot) = source.split_once('/').context("Expected BASE/SNAPSHOT")?;
    enginebox_core::identifier(base, true)?;
    enginebox_core::identifier(snapshot, false)?;
    let release = PathBuf::from(std::env::var("ENGINEBOX_TEST_RELEASE_BIN")?);
    ensure!(release.is_absolute(), "Absolute candidate path required");
    let root = PathBuf::from(std::env::var("ENGINEBOX_NATIVE_TEST_STATE")?);
    ensure!(
        root.is_absolute() && !root.exists(),
        "Fresh absolute private state root required"
    );
    security::private_dir(&root)?;
    security::write_json(
        &root,
        "qualification.json",
        &json!({"kind":"native-run-qualification","version":env!("CARGO_PKG_VERSION")}),
    )?;
    let source_homes = PathBuf::from(std::env::var("ENGINEBOX_NATIVE_SOURCE_HOMES")?);
    let providers_text = std::env::var("ENGINEBOX_NATIVE_TEST_PROVIDERS")
        .unwrap_or_else(|_| "codex,claude,cursor".into());
    let providers = providers_text.split(',').collect::<Vec<_>>();
    ensure!(
        !providers.is_empty()
            && providers
                .iter()
                .all(|p| ["codex", "claude", "cursor"].contains(p))
            && providers
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                == providers.len(),
        "Select distinct supported providers"
    );
    let browser_qualification =
        std::env::var("ENGINEBOX_NATIVE_BROWSER_QUALIFICATION").as_deref() == Ok("1");
    let material = copy_access(&source_homes, &root, &providers)?;
    eprintln!(
        "Access-only credentials derived under original provider leases; live homes were not modified"
    );
    let marker = uuid::Uuid::new_v4().simple().to_string();
    let mut host = source_host.clone();
    host.id = "native-qualification".into();
    host.project = format!("ebox-native-qual-{}", &marker[..12]);
    host.templates.clear();
    host.warm_pools.clear();
    let name = format!("ebox-native-qual-{}", &marker[..12]);
    let mut fixture = Fixture {
        host: host.clone(),
        name: name.clone(),
        marker: marker.clone(),
        created: false,
        cleaned: false,
    };
    let mut report = json!({"version":env!("CARGO_PKG_VERSION"),"project":host.project,"name":name,"credential_derivation":material,"selected_providers":providers,"providers":{},"layerfs":false,"cleanup":false});
    let outcome = (|| -> Result<()> {
        {
            let _lock = lease(&source_host)?;
            let facts = capacity::inventory(&source_host)?;
            let resources = Resources {
                cpu: 2,
                memory_gib: 2,
                disk_gib: 30,
            };
            ensure!(
                capacity::resource_fits(&source_host, &facts, resources)?,
                "Fixture exceeds capacity/reserve admission"
            );
            report["allocation"] = serde_json::to_value(resources)?;
            let source_snapshot = fleet::template_snapshot(
                &source_host,
                &json!({"source":base,"snapshot":snapshot}),
            )?;
            ensure!(
                source_snapshot["config"]["user.enginebox.owner"] == source_host.project,
                "Source owner mismatch"
            );
            let devices = source_snapshot["expanded_devices"]
                .as_object()
                .context("Source devices missing")?;
            ensure!(
                devices.len() == 2
                    && devices
                        .values()
                        .all(|d| d["type"] == "nic" || d["type"] == "disk" && d["path"] == "/"),
                "Source contains external devices"
            );
            let root_device = devices
                .values()
                .find(|d| d["path"] == "/")
                .context("Root device missing")?;
            ensure!(
                root_device["pool"] == host.pool
                    && capacity::resource_bytes(&root_device["size"], u64::MAX)
                        <= 30 * enginebox_core::GIB,
                "Source disk exceeds fixture budget"
            );
            run(
                &host,
                &[
                    "incus",
                    "project",
                    "create",
                    &host.project,
                    "-c",
                    "features.images=false",
                    "-c",
                    "features.networks=false",
                    "-c",
                    "features.profiles=true",
                    "-c",
                    "features.storage.volumes=false",
                    "-c",
                    &format!("user.enginebox.validation={marker}"),
                ],
                None,
            )?;
            fixture.created = true;
            let bx = host.engine_box();
            bx.call(&["profile", "create", "qualification"])?;
            let profile = json!({"config":{"limits.cpu":"2","limits.memory":"2GiB","migration.stateful":"true","boot.autostart":"false","user.enginebox.owner":host.project},"devices":{"root":{"type":"disk","path":"/","pool":host.pool,"size":"30GiB","size.state":"4GiB"},"eth0":{"type":"nic","name":"eth0","network":host.network.as_ref().context("Explicit network required")?}}});
            bx.run_input(
                &["profile".into(), "edit".into(), "qualification".into()],
                &serde_json::to_vec(&profile)?,
                Duration::from_secs(30),
            )?;
            source_host.engine_box().call(&[
                "copy",
                &source,
                &name,
                "--target-project",
                &host.project,
                "--stateless",
                "--storage",
                &host.pool,
                "--profile",
                "qualification",
                "-c",
                &format!("user.enginebox.owner={}", host.project),
                "-c",
                &format!("user.enginebox.validation={marker}"),
                "-c",
                "user.enginebox.fleet=true",
                "-c",
                "user.enginebox.pool.state=claimed",
                "-c",
                "user.enginebox.clean-base=false",
            ])?;
            bx.call(&["start", &name])?;
            let deadline = Instant::now() + Duration::from_secs(180);
            while bx.call(&["exec", &name, "--", "true"]).is_err() {
                ensure!(Instant::now() < deadline, "Guest agent timeout");
                std::thread::sleep(Duration::from_secs(1));
            }
            let binary_dir = release.to_str().context("Invalid binary path")?;
            let result = run(
                &host,
                &[
                    &format!("{binary_dir}/enginebox-host"),
                    "sync-guest-tools",
                    "--project",
                    &host.project,
                    "--name",
                    &name,
                    "--binary-dir",
                    binary_dir,
                    "--already-locked",
                ],
                None,
            )?;
            report["helper_install"] = serde_json::from_slice(&result)?;
            bx.call(&[
                "exec",
                &name,
                "--",
                "enginebox-host",
                "clean-base-check",
                "--home",
                &host.guest.home,
            ])?;
            report["credential_free_source_verified"] = json!(true);
            if std::env::var("ENGINEBOX_NATIVE_PREPARE_GUEST").as_deref() == Ok("1") {
                let mut config = enginebox_core::bootstrap::defaults();
                config["host"]["transport"] = json!("local");
                config["storage"]["pool"] = json!(host.pool);
                config["network"]["name"] = json!(host.network);
                config["incus"]["project"] = json!(host.project);
                config["incus"]["profile"] = json!("qualification");
                config["workspace"]["name"] = json!(name);
                config["modules"]["browser"] = json!(browser_qualification);
                config["modules"]["operator_mcp"] = json!(false);
                let started = Instant::now();
                bx.run_input(
                    &[
                        "exec".into(),
                        name.clone(),
                        "--".into(),
                        "enginebox-host".into(),
                        "prepare-guest".into(),
                    ],
                    &serde_json::to_vec(&config)?,
                    Duration::from_secs(600),
                )?;
                report["rust_preparation_seconds"] = json!(started.elapsed().as_secs_f64());
                bx.call(&["stop", &name, "--timeout", "30"])?;
                bx.call(&["start", &name])?;
                fleet::wait_ready(&bx, &name)?;
                report["prepared_guest_rebooted"] = json!(true);
            }
            for binary in ["enginebox-host", "enginebox-guest", "enginebox-mcp"] {
                use sha2::{Digest, Sha256};
                let expected =
                    format!("{:x}", Sha256::digest(std::fs::read(release.join(binary))?));
                let observed = String::from_utf8(bx.call(&[
                    "exec",
                    &name,
                    "--",
                    "sha256sum",
                    &format!("/usr/local/bin/{binary}"),
                ])?)?;
                ensure!(
                    observed.split_whitespace().next() == Some(expected.as_str()),
                    "Guest helper digest differs from candidate"
                );
                report["helper_sha256"][binary] = json!(expected);
                let version =
                    String::from_utf8(bx.call(&["exec", &name, "--", binary, "--version"])?)?;
                ensure!(
                    version.trim() == format!("{binary} {}", env!("CARGO_PKG_VERSION")),
                    "Wrong guest helper version"
                );
            }
            // Preparation may migrate a legacy MCP entry and retain a private
            // config backup. This assigned fixture is never published as a base.
            fleet::wait_ready(&bx, &name)?;
            guest::assert_remote_idle(&host, &name)?;
            report["instance_uuid"] = bx.instance(&name)?["config"]["volatile.uuid"].clone();
            for (provider, binary) in [
                ("codex", "codex"),
                ("claude", "claude"),
                ("cursor", "agent"),
            ] {
                let version = bx.execute(
                    &name,
                    &[binary.into(), "--version".into()],
                    Duration::from_secs(30),
                )?;
                report["client_versions"][provider] = json!(String::from_utf8(version)?.trim());
            }
        }
        let fleet = Fleet::from_hosts(vec![host.clone()], &root)?;
        if browser_qualification {
            report["browser_fixture"] = guest_json(
                &host,
                &name,
                &format!(
                    "import json,pathlib,subprocess; p=pathlib.Path({:?}); p.mkdir(); (p/'index.html').write_text(\"<!doctype html><title>Enginebox qualification</title><label>Name <input id='name'></label><button onclick=\\\"document.querySelector('output').textContent='Hello '+document.querySelector('input').value\\\">Greet</button><output></output>\"); s=subprocess.Popen(['python3','-I','-m','http.server','8765','--bind','127.0.0.1','--directory',str(p)],stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True); print(json.dumps({{'pid':s.pid,'loopback_only':True}}))",
                    format!("{}/browser-fixture", host.guest.workspace)
                ),
            )?;
        }
        let binaries = BTreeMap::from([
            (
                "codex".into(),
                source_homes
                    .parent()
                    .context("Missing agent root")?
                    .join("bin/codex")
                    .to_string_lossy()
                    .into_owned(),
            ),
            (
                "claude".into(),
                source_homes
                    .parent()
                    .unwrap()
                    .join("bin/claude")
                    .to_string_lossy()
                    .into_owned(),
            ),
            (
                "cursor".into(),
                source_homes
                    .parent()
                    .unwrap()
                    .join("bin/agent")
                    .to_string_lossy()
                    .into_owned(),
            ),
        ]);
        let mut service = AgentService::with_binaries(&root, fleet.clone(), binaries.clone())?;
        for &provider in &providers {
            let workdir = format!("qualification-{provider}");
            host.engine_box().execute(
                &name,
                &["mkdir".into(), workdir.clone()],
                Duration::from_secs(15),
            )?;
            let preflight = guest_json(
                &host,
                &name,
                &format!(
                    "import json,pathlib,subprocess; p=pathlib.Path({:?}); f=p/'write-probe'; f.write_text('ok'); f.unlink(); r=subprocess.run(['codex','-c','sandbox_mode=\"workspace-write\"','sandbox','--','python3','-I','-c',\"from pathlib import Path; p=Path('sandbox-probe'); p.write_text('ok'); p.unlink()\"],cwd=p,capture_output=True,text=True); print(json.dumps({{'direct_write':True,'sandbox_exit':r.returncode,'sandbox_output':r.stdout+r.stderr}}))",
                    format!("{}/{workdir}", host.guest.workspace)
                ),
            )?;
            report["preflights"][provider] = preflight;
            security::write_json(&root, "report.json", &report)?;
            if report["preflights"][provider]["sandbox_exit"] != 0 {
                let diagnostic = host.engine_box().call(&[
                    "exec", &name, "--", "sh", "-c",
                    "ls -l /etc/apparmor.d/bwrap-userns-restrict /usr/bin/bwrap 2>&1; cat /sys/kernel/security/apparmor/profiles | grep -E 'bwrap|unprivileged'; systemctl is-enabled apparmor; systemctl is-active apparmor; journalctl -k -b --no-pager | grep -E 'DENIED|bwrap' | tail -15; true",
                ])?;
                report["apparmor_diagnostic"] = json!(String::from_utf8(diagnostic)?);
            }
            ensure!(
                report["preflights"][provider]["sandbox_exit"] == 0,
                "Codex model-free sandbox write preflight failed"
            );
            let canary = uuid::Uuid::new_v4().simple().to_string();
            let prompt = format!(
                "This is an authorized small coding test in this disposable directory. Use the terminal tool to run Python that creates calculator.py defining add(a,b) returning a+b, then run Python to assert add(2,3)==5. If any tool fails, include its exact error in your answer. Remember this conversation-only token: {canary}. Do not write the token to any file or repeat it in your answer. End by saying READY only after the file and assertion succeed. Work only in the current directory."
            );
            let mut request = json!({"provider":provider,"prompt":prompt,"workdir":workdir,"timeout_seconds":180});
            if provider == "claude" {
                request["model"] = json!("haiku");
            }
            let started = Instant::now();
            let first = wait_run(&service, &root, &host, &name, request)?;
            report["providers"][provider] =
                json!({"first":summarized(&first,started.elapsed().as_secs_f64())});
            security::write_json(&root, "report.json", &report)?;
            eprintln!("{provider} first run: {}", first["status"]);
            if first["status"] != "succeeded" {
                continue;
            }
            let session = first["native_session_id"]
                .as_str()
                .context("Successful run missing exact native session ID")?;
            let first_proof = guest_json(
                &host,
                &name,
                &format!(
                    "import json,pathlib,runpy; p=pathlib.Path({:?}); m=runpy.run_path(str(p/'calculator.py')); assert m['add'](2,3)==5; assert all({:?}.encode() not in f.read_bytes() for f in p.rglob('*') if f.is_file()), 'Conversation token was written into project'; print(json.dumps({{'add':True,'project_has_no_canary':True}}))",
                    format!("{}/{workdir}", host.guest.workspace),
                    canary
                ),
            )?;
            report["providers"][provider]["first_file_proof"] = first_proof;
            // Construct a new service to prove durable run lookup plus exact
            // native history resume without process-local conversation state.
            service.close();
            let deadline = Instant::now() + Duration::from_secs(10);
            while service.pending() > 0 {
                ensure!(
                    Instant::now() < deadline,
                    "Native run has not released its lease"
                );
                std::thread::sleep(Duration::from_millis(100));
            }
            drop(service);
            service = AgentService::with_binaries(&root, fleet.clone(), binaries.clone())?;
            ensure!(
                service.get_run(&host.id, &name, first["id"].as_str().unwrap())?["native_session_id"]
                    == session,
                "Restart lost exact session record"
            );
            let prompt = "Resume our exact conversation. Without searching any files, recall the conversation-only token I gave you earlier. Add multiply(a,b) returning a*b to calculator.py and run Python assertions for add(2,3)==5 and multiply(6,7)==42. End your answer with TOKEN= followed by the remembered token. Work only in this directory.";
            let mut request = json!({"provider":provider,"prompt":prompt,"workdir":workdir,"resume_id":session,"timeout_seconds":180});
            if provider == "claude" {
                request["model"] = json!("haiku");
            }
            let started = Instant::now();
            let resumed = wait_run(&service, &root, &host, &name, request)?;
            let recalled = resumed["result"]["output"]
                .as_str()
                .is_some_and(|output| output.contains(&canary));
            report["providers"][provider]["resumed"] =
                summarized(&resumed, started.elapsed().as_secs_f64());
            report["providers"][provider]["canary_recalled"] = json!(recalled);
            report["providers"][provider]["same_session_id"] =
                json!(resumed["native_session_id"] == session);
            if resumed["status"] == "succeeded" {
                report["providers"][provider]["resumed_file_proof"] = guest_json(
                    &host,
                    &name,
                    &format!(
                        "import json,runpy; m=runpy.run_path({:?}); assert m['add'](2,3)==5; assert m['multiply'](6,7)==42; print(json.dumps({{'add':True,'multiply':True}}))",
                        format!("{}/{workdir}/calculator.py", host.guest.workspace)
                    ),
                )?;
            }
            security::write_json(&root, "report.json", &report)?;
            eprintln!(
                "{provider} resumed: {}, exact canary: {recalled}",
                resumed["status"]
            );
            if browser_qualification && resumed["status"] == "succeeded" {
                let browser_name = format!("enginebox-{}", &canary[..8]);
                let screenshot = format!(
                    "{}/.local/share/enginebox/browser/{provider}/output/qualification-{provider}.png",
                    host.guest.home
                );
                let prompt = format!(
                    "Use only the enginebox-browser MCP tools for this browser task, without shell, Python, file edits, or other browser libraries. Navigate to http://127.0.0.1:8765/index.html, enter {browser_name} in the Name field, click Greet, read the displayed greeting, then save a PNG screenshot with the exact absolute filename {screenshot} in the MCP output directory. Include the observed greeting in your final answer."
                );
                let mut request = json!({"provider":provider,"prompt":prompt,"workdir":workdir,"resume_id":session,"timeout_seconds":180});
                if provider == "claude" {
                    request["model"] = json!("haiku");
                }
                let started = Instant::now();
                let browser = wait_run(&service, &root, &host, &name, request)?;
                let mut proof = summarized(&browser, started.elapsed().as_secs_f64());
                proof["mcp_tool_events"] = json!(browser_calls(&browser));
                proof["greeting_observed"] = json!(
                    browser["result"]["output"]
                        .as_str()
                        .is_some_and(|output| output.contains(&format!("Hello {browser_name}")))
                );
                proof["same_session_id"] = json!(browser["native_session_id"] == session);
                report["providers"][provider]["browser"] = proof;
                security::write_json(&root, "report.json", &report)?;
                if browser["status"] == "succeeded" {
                    report["providers"][provider]["browser_png"] = guest_json(
                        &host,
                        &name,
                        &format!(
                            "import json,pathlib; p=pathlib.Path({:?}); b=p.read_bytes(); assert b.startswith(b'\\x89PNG\\r\\n\\x1a\\n') and len(b)>1000; print(json.dumps({{'valid_png':True,'bytes':len(b)}}))",
                            screenshot
                        ),
                    )?;
                }
                security::write_json(&root, "report.json", &report)?;
                eprintln!("{provider} browser: {}", browser["status"]);
            }
        }
        service.close();
        let deadline = Instant::now() + Duration::from_secs(10);
        while service.pending() > 0 {
            ensure!(Instant::now() < deadline, "Native service did not drain");
            std::thread::sleep(Duration::from_millis(100));
        }
        guest::assert_remote_idle(&host, &name)?;
        report["guest_leases_released"] = json!(true);
        if std::env::var("ENGINEBOX_NATIVE_BOOT_SAMPLES").as_deref() == Ok("1") {
            let bx = host.engine_box();
            report["boot_readiness"]["boundary"] = json!(
                "host-local Rust lifecycle resume through helper, identity, network and DNS readiness; excludes lock wait, preparation, HTTP, credential delivery and model inference"
            );
            report["boot_readiness"]["host_load_before"] =
                json!(std::fs::read_to_string("/proc/loadavg")?.trim());
            for action in ["stop", "pause"] {
                let mut samples = Vec::new();
                for _ in 0..3 {
                    let _lock = lease(&host)?;
                    guest::assert_remote_idle(&host, &name)?;
                    let before = bx.call(&[
                        "exec",
                        &name,
                        "--",
                        "cat",
                        "/proc/sys/kernel/random/boot_id",
                    ])?;
                    bx.lifecycle(&name, action)?;
                    let started = Instant::now();
                    bx.lifecycle(&name, "resume")?;
                    let lifecycle = started.elapsed().as_secs_f64();
                    fleet::wait_ready(&bx, &name)?;
                    let to_ready = started.elapsed().as_secs_f64();
                    let after = bx.call(&[
                        "exec",
                        &name,
                        "--",
                        "cat",
                        "/proc/sys/kernel/random/boot_id",
                    ])?;
                    ensure!(
                        (before == after) == (action == "pause"),
                        "Unexpected guest boot identity after lifecycle action"
                    );
                    samples.push(json!({"lifecycle_seconds":lifecycle,"to_ready_seconds":to_ready,"boot_id_preserved":before==after}));
                }
                report["boot_readiness"][action] = json!(samples);
                security::write_json(&root, "report.json", &report)?;
            }
            report["boot_readiness"]["host_load_after"] =
                json!(std::fs::read_to_string("/proc/loadavg")?.trim());
        }
        Ok(())
    })();
    if let Err(error) = &outcome {
        report["error"] = json!(format!("{error:#}"));
    }
    let cleanup = fixture.cleanup();
    report["cleanup"] = json!(cleanup.is_ok() && fixture.cleaned);
    security::write_json(&root, "report.json", &report)?;
    // Access credentials and native logs remain private until the operator exports
    // the report. Never print model output or credential material to test stdout.
    println!("{}", serde_json::to_string_pretty(&report)?);
    cleanup?;
    outcome?;
    for provider in providers {
        let p = &report["providers"][provider];
        if p["first"]["quota_blocked"] == true {
            continue;
        }
        ensure!(
            p["first"]["status"] == "succeeded"
                && p["resumed"]["status"] == "succeeded"
                && p["canary_recalled"] == true
                && p["same_session_id"] == true,
            "{provider} did not pass native launch and exact resume; inspect private report"
        );
        if browser_qualification {
            ensure!(
                p["browser"]["status"] == "succeeded"
                    && p["browser"]["mcp_tool_events"]
                        .as_u64()
                        .is_some_and(|n| n > 0)
                    && p["browser"]["greeting_observed"] == true
                    && p["browser"]["same_session_id"] == true
                    && p["browser_png"]["valid_png"] == true,
                "{provider} did not pass native browser MCP qualification"
            );
        }
    }
    Ok(())
}
