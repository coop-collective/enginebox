//! Explicit opt-in, host-local release benchmark. No existing workspace is mutated.
//! Run as root with ENGINEBOX_BOOT_BENCHMARK=1, ENGINEBOX_TEST_FLEET,
//! ENGINEBOX_TEST_HOST, ENGINEBOX_TEST_SOURCE=BASE/SNAPSHOT,
//! ENGINEBOX_TEST_RELEASE_BIN, ENGINEBOX_BENCH_SOURCE_SHA, ENGINEBOX_BENCH_REPORT.
//! Optional ENGINEBOX_BENCH_GATE waits up to 180s for a coordinator-created file.
//! All samples use a credential-free UUID fixture marked preparing, never assigned.
#![cfg(target_os = "linux")]
use anyhow::{Context, Result, ensure};
use enginebox_core::{
    EngineBox, Host, Resources, bootstrap, capacity, fleet, guest, load_hosts, state,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn lease() -> Result<(File, f64)> {
    let start = Instant::now();
    loop {
        match state::root_host_lock() {
            Ok(lock) => return Ok((lock, start.elapsed().as_secs_f64())),
            Err(error) if start.elapsed() >= Duration::from_secs(60) => return Err(error),
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}
struct Fixture {
    host: Host,
    name: String,
    marker: String,
    staging: PathBuf,
    cleaned: bool,
    setup_lock: Option<File>,
}
impl Fixture {
    fn cleanup(&mut self) -> Result<()> {
        let (_lock, _) = lease()?;
        let bx = self.host.engine_box();
        let all = bx.query(&format!(
            "/1.0/instances?project={}&recursion=1",
            self.host.project
        ))?;
        if let Some(vm) = all
            .as_array()
            .context("Invalid inventory")?
            .iter()
            .find(|v| v["name"] == self.name)
        {
            ensure!(
                vm["config"]["user.enginebox.validation"] == self.marker
                    && vm["config"]["user.enginebox.owner"] == self.host.project,
                "Refusing foreign fixture cleanup"
            );
            bx.call(&["delete", &self.name, "--force"])?;
        }
        ensure!(
            fs::read_to_string(self.staging.join("marker"))? == self.marker,
            "Foreign staging directory"
        );
        fs::remove_dir_all(&self.staging)?;
        self.cleaned = true;
        Ok(())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.setup_lock.take();
        if !self.cleaned
            && let Err(e) = self.cleanup()
        {
            eprintln!("Cleanup requires inspection: {}: {e:#}", self.name);
        }
    }
}
fn text(bx: &EngineBox, name: &str, argv: &[&str]) -> Result<String> {
    Ok(String::from_utf8(bx.execute(
        name,
        &argv.iter().map(|v| (*v).into()).collect::<Vec<_>>(),
        Duration::from_secs(30),
    )?)?
    .trim()
    .into())
}
fn pressure() -> Result<Value> {
    let (total, available) = capacity::memory()?;
    Ok(
        json!({"loadavg":fs::read_to_string("/proc/loadavg")?.trim(),"memory_total_bytes":total,"memory_available_bytes":available}),
    )
}
fn verify_source_devices(bx: &EngineBox, host: &Host, source: &Value) -> Result<()> {
    let mut devices = serde_json::Map::new();
    let mut config = serde_json::Map::new();
    for profile in source["profiles"]
        .as_array()
        .context("Missing source profiles")?
    {
        let profile = profile.as_str().context("Invalid source profile")?;
        enginebox_core::identifier(profile, false)?;
        let value = bx.query(&format!("/1.0/profiles/{profile}?project={}", host.project))?;
        devices.extend(
            value["devices"]
                .as_object()
                .context("Invalid profile devices")?
                .clone(),
        );
        config.extend(
            value["config"]
                .as_object()
                .context("Invalid profile configuration")?
                .clone(),
        );
    }
    devices.extend(
        source["devices"]
            .as_object()
            .context("Invalid source devices")?
            .clone(),
    );
    config.extend(
        source["config"]
            .as_object()
            .context("Invalid source configuration")?
            .clone(),
    );
    ensure!(
        !config.keys().any(|key| key.starts_with("raw.")),
        "Refusing custom raw VM configuration"
    );
    ensure!(
        devices.len() == 2,
        "Benchmark source must contain only root disk and known bridge NIC"
    );
    for device in devices.values() {
        let root = device["type"] == "disk"
            && device["path"] == "/"
            && device["pool"] == host.pool
            && device.get("source").is_none();
        let nic = device["type"] == "nic"
            && device["network"].as_str() == host.network.as_deref()
            && host.network.is_some()
            && device.get("parent").is_none();
        ensure!(
            root || nic,
            "Refusing source with external or passthrough attachments"
        );
    }
    Ok(())
}
fn marker(bx: &EngineBox, name: &str) -> Result<Value> {
    Ok(
        json!({"boot_id":text(bx,name,&["cat","/proc/sys/kernel/random/boot_id"])?,"process":text(bx,name,&["sh","-c","p=$(systemctl show enginebox-benchmark-marker --property MainPID --value); test \"$p\" -gt 1 && cat /proc/\"$p\"/stat"]) ?}),
    )
}
fn start_marker(bx: &EngineBox, name: &str) -> Result<()> {
    bx.call(&[
        "exec",
        name,
        "--",
        "systemd-run",
        "--unit=enginebox-benchmark-marker",
        "--property=Type=exec",
        "/usr/bin/sleep",
        "3600",
    ])?;
    Ok(())
}
fn stable_marker(v: &Value) -> (Value, Vec<String>) {
    // PID and start-time survive pause/hibernate; CPU counters in /proc/PID/stat do not.
    let fields = v["process"]
        .as_str()
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>();
    (
        v["boot_id"].clone(),
        [0, 21]
            .into_iter()
            .filter_map(|i| fields.get(i).map(|v| (*v).into()))
            .collect(),
    )
}
fn components(bx: &EngineBox, name: &str) -> Result<Value> {
    let start = Instant::now();
    guest::ensure_remote_guest_tools(bx, name, true)?;
    let helpers = start.elapsed().as_secs_f64();
    let start = Instant::now();
    let readiness: Value = serde_json::from_slice(&bx.execute_with_input(
        name,
        &["enginebox-host".into(), "readiness".into()],
        &serde_json::to_vec(&bx.guest)?,
        Duration::from_secs(15),
    )?)?;
    ensure!(readiness["ready"] == true, "Direct readiness failed");
    Ok(
        json!({"helper_validation_seconds":helpers,"readiness_probe_seconds":start.elapsed().as_secs_f64()}),
    )
}
fn stats(samples: &[Value], key: &str) -> Value {
    let mut values = samples
        .iter()
        .filter_map(|v| v[key].as_f64())
        .collect::<Vec<_>>();
    values.sort_by(f64::total_cmp);
    let n = values.len();
    let median = if n % 2 == 0 {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    } else {
        values[n / 2]
    };
    json!({"n":n,"median":median,"p95_nearest_rank":values[(n*95).div_ceil(100)-1],"min":values[0],"max":values[n-1]})
}
fn sample_phase(
    f: &Fixture,
    action: &str,
    count: usize,
    report: &mut Value,
    path: &Path,
) -> Result<()> {
    let bx = f.host.engine_box();
    let mut samples = Vec::new();
    for index in 0..count {
        let (lock, lock_wait) = lease()?;
        guest::assert_remote_idle(&f.host, &f.name)?;
        let before_marker = marker(&bx, &f.name)?;
        let pressure_before = pressure()?;
        let start = Instant::now();
        let prepared = bx.lifecycle(&f.name, action)?;
        let prepare_seconds = start.elapsed().as_secs_f64();
        let expected = if action == "pause" {
            "Frozen"
        } else {
            "Stopped"
        };
        ensure!(
            prepared["status"] == expected && prepared["stateful"] == (action == "hibernate"),
            "Unexpected prestart state: {prepared}"
        );
        let started = Instant::now();
        bx.lifecycle(&f.name, "resume")?;
        let lifecycle_seconds = started.elapsed().as_secs_f64();
        let probe_start = Instant::now();
        let ready = fleet::wait_ready(&bx, &f.name)?;
        let readiness_seconds = probe_start.elapsed().as_secs_f64();
        let to_ready_seconds = started.elapsed().as_secs_f64();
        let mut clients = serde_json::Map::new();
        let clients_start = Instant::now();
        for cli in ["codex", "claude", "agent"] {
            let start = Instant::now();
            let version = text(&bx, &f.name, &[cli, "--version"])?;
            clients.insert(
                cli.into(),
                json!({"version":version,"seconds":start.elapsed().as_secs_f64()}),
            );
        }
        let native_cli_seconds = clients_start.elapsed().as_secs_f64();
        let to_native_cli_seconds = started.elapsed().as_secs_f64();
        if action == "stop" {
            let after_boot = text(&bx, &f.name, &["cat", "/proc/sys/kernel/random/boot_id"])?;
            ensure!(
                before_marker["boot_id"] != after_boot,
                "Cold start reused boot ID"
            );
            start_marker(&bx, &f.name)?;
        } else {
            ensure!(
                stable_marker(&before_marker) == stable_marker(&marker(&bx, &f.name)?),
                "Resume did not preserve process and boot ID"
            );
        }
        let parts = components(&bx, &f.name)?;
        let row = json!({"sample":index+1,"lock_wait_seconds":lock_wait,"prepare_seconds":prepare_seconds,"lifecycle_seconds":lifecycle_seconds,"readiness_seconds":readiness_seconds,"to_ready_seconds":to_ready_seconds,"native_cli_seconds":native_cli_seconds,"to_native_cli_seconds":to_native_cli_seconds,"clients":clients,"components_when_ready":parts,"pressure_before":pressure_before,"pressure_after":pressure()?,"ready":ready["ready"],"process_identity_preserved":action!="stop"});
        eprintln!(
            "{action} {}/{}: lifecycle={lifecycle_seconds:.4}s ready={to_ready_seconds:.4}s native={to_native_cli_seconds:.4}s",
            index + 1,
            count
        );
        samples.push(row);
        report["phases"][action] = json!({"samples":samples});
        state::write_json(path, report)?;
        drop(lock);
    }
    let summaries = [
        "prepare_seconds",
        "lifecycle_seconds",
        "readiness_seconds",
        "to_ready_seconds",
        "native_cli_seconds",
        "to_native_cli_seconds",
    ]
    .into_iter()
    .map(|k| (k.into(), stats(&samples, k)))
    .collect::<serde_json::Map<_, _>>();
    report["phases"][action]["summary"] = json!(summaries);
    state::write_json(path, report)?;
    Ok(())
}

#[test]
#[ignore = "requires explicit root opt-in and admitted disposable Linux VM"]
fn host_local_boot_benchmark() -> Result<()> {
    ensure!(
        std::env::var("ENGINEBOX_BOOT_BENCHMARK").as_deref() == Ok("1"),
        "Explicit benchmark opt-in required"
    );
    ensure!(
        !cfg!(debug_assertions),
        "Use cargo test --release for timings"
    );
    ensure!(
        unsafe { libc::geteuid() } == 0,
        "Host-local benchmark requires root"
    );
    let config = std::env::var("ENGINEBOX_TEST_FLEET")?;
    let id = std::env::var("ENGINEBOX_TEST_HOST")?;
    let host = load_hosts(Path::new(&config))?
        .into_iter()
        .find(|h| h.id == id)
        .context("Unknown host")?;
    ensure!(
        host.target == "local",
        "Benchmark excludes SSH transport; host target must be local"
    );
    let source = std::env::var("ENGINEBOX_TEST_SOURCE")?;
    let (base, snapshot) = source.split_once('/').context("Expected BASE/SNAPSHOT")?;
    enginebox_core::identifier(base, true)?;
    enginebox_core::identifier(snapshot, false)?;
    let source_sha = std::env::var("ENGINEBOX_BENCH_SOURCE_SHA")?;
    ensure!(
        source_sha.len() == 40 && source_sha.bytes().all(|v| v.is_ascii_hexdigit()),
        "Expected source commit SHA"
    );
    let report_path = PathBuf::from(std::env::var("ENGINEBOX_BENCH_REPORT")?);
    ensure!(
        report_path.is_absolute(),
        "Use an absolute private report path"
    );
    let release = PathBuf::from(std::env::var("ENGINEBOX_TEST_RELEASE_BIN")?);
    ensure!(
        release.is_absolute(),
        "Release binary directory must be absolute"
    );
    let bx = host.engine_box();
    let (lock, _) = lease()?;
    let facts = capacity::local_inventory(&host.pool, &host.project)?;
    let resources = Resources {
        cpu: 2,
        memory_gib: 2,
        disk_gib: 30,
    };
    ensure!(
        capacity::resource_fits(&host, &facts, resources)?,
        "Disposable benchmark exceeds admission"
    );
    let template = json!({"source":base,"snapshot":snapshot});
    let source_before = fleet::template_snapshot(&host, &template)?;
    verify_source_devices(&bx, &host, &source_before)?;
    ensure!(
        source_before["config"]["user.enginebox.owner"] == host.project,
        "Foreign clean snapshot"
    );
    let (_, disk) = fleet::root_device(&bx, &source_before)?;
    ensure!(
        disk["pool"] == host.pool
            && capacity::resource_bytes(&disk["size"], u64::MAX) <= 30 * enginebox_core::GIB,
        "Source disk exceeds budget"
    );
    let uuid = uuid::Uuid::new_v4().simple().to_string();
    let name = format!("ebox-rust-bench-{}", &uuid[..12]);
    let staging = PathBuf::from(format!("/var/tmp/enginebox-benchmark-{uuid}"));
    fs::create_dir(&staging)?;
    fs::write(staging.join("marker"), &uuid)?;
    let mut fixture = Fixture {
        host: host.clone(),
        name: name.clone(),
        marker: uuid.clone(),
        staging: staging.clone(),
        cleaned: false,
        setup_lock: Some(lock),
    };
    let mut hashes = serde_json::Map::new();
    use std::os::unix::fs::PermissionsExt;
    for binary in ["enginebox-host", "enginebox-guest", "enginebox-mcp"] {
        let bytes = fs::read(release.join(binary))?;
        let target = staging.join(binary);
        fs::write(&target, &bytes)?;
        fs::set_permissions(target, fs::Permissions::from_mode(0o755))?;
        hashes.insert(binary.into(), json!(format!("{:x}", Sha256::digest(bytes))));
    }
    let copy_start = Instant::now();
    bx.call(&[
        "copy",
        &source,
        &name,
        "--stateless",
        "--storage",
        &host.pool,
        "-c",
        &format!("user.enginebox.owner={}", host.project),
        "-c",
        "migration.stateful=true",
        "-c",
        "limits.cpu=2",
        "-c",
        "limits.memory=2GiB",
        "-c",
        "user.enginebox.fleet=true",
        "-c",
        "user.enginebox.template=rust-benchmark",
        "-c",
        "user.enginebox.pool.state=preparing",
        "-c",
        "user.enginebox.clean-base=false",
        "-c",
        "boot.autostart=false",
        "-c",
        &format!("user.enginebox.validation={uuid}"),
    ])?;
    let copy_seconds = copy_start.elapsed().as_secs_f64();
    let vm = bx.instance(&name)?;
    let (device, _) = fleet::root_device(&bx, &vm)?;
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
    while bx.call(&["exec", &name, "--", "true"]).is_err() {
        ensure!(Instant::now() < deadline, "Guest agent did not start");
        std::thread::sleep(Duration::from_secs(1));
    }
    guest::sync_guest_tools(&host.project, &name, Some(&staging))?;
    let mut c = bootstrap::defaults();
    c["host"]["transport"] = json!("local");
    c["storage"]["pool"] = json!(host.pool);
    c["network"]["name"] = json!(
        host.network
            .as_deref()
            .context("Explicit existing network required")?
    );
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
    bx.run_input(
        &[
            "exec".into(),
            name.clone(),
            "--".into(),
            "enginebox-host".into(),
            "prepare-guest".into(),
        ],
        &serde_json::to_vec(&c)?,
        Duration::from_secs(900),
    )?;
    bx.call(&["restart", &name, "--force"])?;
    fleet::wait_ready(&bx, &name)?;
    bx.call(&[
        "exec",
        &name,
        "--",
        "enginebox-host",
        "clean-base-check",
        "--home",
        &host.guest.home,
    ])?;
    start_marker(&bx, &name)?;
    let mut report = json!({"schema_version":1,"source_sha":source_sha,"crate_version":env!("CARGO_PKG_VERSION"),"benchmark_source_sha256":format!("{:x}",Sha256::digest(include_bytes!("boot_benchmarks.rs"))),"started_at":chrono::Utc::now(),"resource_budget":{"cpu":2,"memory_gib":2,"root_gib":30,"state_gib":4},"fixture":name,"helper_sha256":hashes,"capacity_before":capacity::capacity(&host,&facts)?,"fixture_copy_seconds":copy_seconds,"host_before":pressure()?,"phases":{},"cleaned":false});
    state::write_json(&report_path, &report)?;
    fixture.setup_lock.take();
    eprintln!("Fixture prepared; waiting for optional timing gate");
    if let Ok(gate) = std::env::var("ENGINEBOX_BENCH_GATE") {
        let deadline = Instant::now() + Duration::from_secs(180);
        while !Path::new(&gate).exists() {
            ensure!(Instant::now() < deadline, "Timing gate timeout");
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    sample_phase(&fixture, "stop", 10, &mut report, &report_path)?;
    sample_phase(&fixture, "pause", 20, &mut report, &report_path)?;
    sample_phase(&fixture, "hibernate", 5, &mut report, &report_path)?;
    let (lock, _) = lease()?;
    let source_after = fleet::template_snapshot(&host, &template)?;
    for field in ["config", "devices", "profiles", "stateful", "name"] {
        ensure!(
            source_before[field] == source_after[field],
            "Source snapshot changed: {field}"
        );
    }
    report["host_after"] = pressure()?;
    report["completed_at"] = json!(chrono::Utc::now());
    drop(lock);
    fixture.cleanup()?;
    report["cleaned"] = json!(true);
    state::write_json(&report_path, &report)?;
    Ok(())
}
