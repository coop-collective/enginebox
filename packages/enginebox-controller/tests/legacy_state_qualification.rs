//! Read-only source qualification. Only this ignored test accepts live paths;
//! all controller writes go into a newly created private temporary directory.
#![cfg(unix)]

use enginebox_controller::Controller;
use enginebox_core::{state, templates};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
};

type Checked<T> = Result<T, &'static str>;
const MAX_FILE: usize = 4 * 1024 * 1024;
const MAX_TOTAL: usize = 64 * 1024 * 1024;
const MAX_FILES: usize = 10_000;

fn phase<T, E>(result: Result<T, E>, code: &'static str) -> Checked<T> {
    result.map_err(|_| code)
}
fn require(condition: bool, code: &'static str) -> Checked<()> {
    if condition { Ok(()) } else { Err(code) }
}

// Never derive Debug for data-bearing structs: failed assertions must not print
// native prompts/output, private host configuration, or record contents.
#[derive(PartialEq, Eq)]
struct Fingerprint {
    device: u64,
    inode: u64,
    uid: u32,
    mode: u32,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}
fn fingerprint(metadata: &fs::Metadata) -> Fingerprint {
    Fingerprint {
        device: metadata.dev(),
        inode: metadata.ino(),
        uid: metadata.uid(),
        mode: metadata.mode(),
        size: metadata.len(),
        modified: (metadata.mtime(), metadata.mtime_nsec()),
        changed: (metadata.ctime(), metadata.ctime_nsec()),
    }
}
#[derive(PartialEq, Eq)]
struct Record {
    bytes: Vec<u8>,
    identity: Fingerprint,
}
#[derive(Default, PartialEq, Eq)]
struct Snapshot {
    records: BTreeMap<PathBuf, Record>,
    directories: BTreeSet<PathBuf>,
}

fn reject_links(path: &Path) -> Checked<()> {
    require(
        path.is_absolute()
            && !path
                .components()
                .any(|part| matches!(part, Component::ParentDir)),
        "source-path",
    )?;
    for ancestor in path.ancestors() {
        let metadata = phase(fs::symlink_metadata(ancestor), "source-path-unavailable")?;
        require(!metadata.file_type().is_symlink(), "source-symlink")?;
    }
    Ok(())
}
fn read_record(path: &Path) -> Checked<Record> {
    reject_links(path)?;
    let before = phase(fs::symlink_metadata(path), "source-file-unavailable")?;
    require(
        before.is_file() && before.nlink() == 1 && before.len() <= MAX_FILE as u64,
        "source-file-shape",
    )?;
    let file = phase(
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path),
        "source-file-open",
    )?;
    let opened = phase(file.metadata(), "source-file-metadata")?;
    require(
        fingerprint(&before) == fingerprint(&opened),
        "source-raced-before-read",
    )?;
    let mut bytes = Vec::new();
    phase(
        file.take((MAX_FILE + 1) as u64).read_to_end(&mut bytes),
        "source-file-read",
    )?;
    require(bytes.len() <= MAX_FILE, "source-file-bound")?;
    let after = phase(fs::symlink_metadata(path), "source-file-disappeared")?;
    require(
        fingerprint(&before) == fingerprint(&after),
        "source-raced-during-read",
    )?;
    Ok(Record {
        bytes,
        identity: fingerprint(&after),
    })
}
fn stable_terminal(category: &str, relative: &Path, bytes: &[u8]) -> Checked<()> {
    if relative.extension().and_then(|s| s.to_str()) == Some("lock") {
        require(
            category == "templates" && relative.starts_with("templates/locks") && bytes.is_empty(),
            "unexpected-lock-file",
        )?;
        return Ok(());
    }
    require(
        relative.extension().and_then(|s| s.to_str()) == Some("json"),
        "unexpected-source-file",
    )?;
    let value: Value = phase(serde_json::from_slice(bytes), "source-json")?;
    require(value.is_object(), "source-record-shape")?;
    if matches!(category, "jobs" | "agent-runs") {
        require(
            matches!(value["status"].as_str(), Some("succeeded" | "failed")),
            "active-or-incomplete-record",
        )?;
        require(
            value["execution_uncertain"] != true,
            "uncertain-agent-record",
        )?;
    } else if let Some(status) = value.get("status") {
        require(
            matches!(status.as_str(), Some("ready" | "failed")),
            "active-or-incomplete-template",
        )?;
    }
    Ok(())
}
fn collect(root: &Path) -> Checked<Snapshot> {
    reject_links(root)?;
    let owner = phase(fs::symlink_metadata(root), "source-root")?.uid();
    let mut snapshot = Snapshot::default();
    let mut total = 0usize;
    for category in ["jobs", "agent-runs", "templates"] {
        let mut queue = vec![PathBuf::from(category)];
        while let Some(relative) = queue.pop() {
            let directory = root.join(&relative);
            reject_links(&directory)?;
            let metadata = phase(
                fs::symlink_metadata(&directory),
                "source-directory-unavailable",
            )?;
            require(
                metadata.is_dir() && metadata.uid() == owner && metadata.mode() & 0o077 == 0,
                "source-directory-shape",
            )?;
            snapshot.directories.insert(relative.clone());
            for item in phase(fs::read_dir(&directory), "source-directory-read")? {
                let item = phase(item, "source-directory-entry")?;
                let child = relative.join(item.file_name());
                let metadata = phase(
                    fs::symlink_metadata(item.path()),
                    "source-entry-unavailable",
                )?;
                require(!metadata.file_type().is_symlink(), "source-symlink")?;
                if metadata.is_dir() {
                    require(
                        category == "templates"
                            && relative == Path::new("templates")
                            && ["manifests", "history", "deployments", "sync", "locks"]
                                .contains(&item.file_name().to_str().unwrap_or("")),
                        "unexpected-source-directory",
                    )?;
                    queue.push(child);
                } else {
                    let record = read_record(&item.path())?;
                    require(
                        record.identity.uid == owner && record.identity.mode & 0o077 == 0,
                        "source-record-permissions",
                    )?;
                    stable_terminal(category, &child, &record.bytes)?;
                    total += record.bytes.len();
                    require(
                        total <= MAX_TOTAL && snapshot.records.len() < MAX_FILES,
                        "source-size-bound",
                    )?;
                    snapshot.records.insert(child, record);
                }
            }
        }
    }
    Ok(snapshot)
}
fn private_write(path: &Path, bytes: &[u8]) -> Checked<()> {
    phase(
        state::private_dir(path.parent().ok_or("destination-parent")?),
        "destination-directory",
    )?;
    let mut file = phase(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path),
        "destination-file",
    )?;
    phase(file.write_all(bytes), "destination-write")?;
    phase(file.sync_all(), "destination-sync")
}
fn copy_records(snapshot: &Snapshot, destination: &Path) -> Checked<()> {
    for directory in &snapshot.directories {
        phase(
            state::private_dir(&destination.join(directory)),
            "destination-directory",
        )?;
    }
    for (path, record) in &snapshot.records {
        // Lock files are observed for source stability but never transferred.
        if path.extension().and_then(|s| s.to_str()) != Some("lock") {
            private_write(&destination.join(path), &record.bytes)?;
        }
    }
    Ok(())
}
fn count(snapshot: &Snapshot, category: &str) -> usize {
    snapshot
        .records
        .keys()
        .filter(|p| {
            p.starts_with(category) && p.extension().and_then(|s| s.to_str()) == Some("json")
        })
        .count()
}
fn files_absent(root: &Path) -> Checked<()> {
    let mut queue = vec![root.to_path_buf()];
    while let Some(directory) = queue.pop() {
        for entry in phase(fs::read_dir(directory), "private-home-inspection")? {
            let entry = phase(entry, "private-home-entry")?;
            require(
                phase(entry.file_type(), "private-home-type")?.is_dir(),
                "unexpected-provider-state",
            )?;
            queue.push(entry.path());
        }
    }
    Ok(())
}

fn qualify(source: &Path, config: &Path) -> Checked<Value> {
    let config_before = read_record(config)?;
    let before = collect(source)?;
    let temporary = phase(
        tempfile::Builder::new()
            .prefix("enginebox-state-qualification-")
            .tempdir(),
        "temporary-state",
    )?;
    let destination = phase(temporary.path().canonicalize(), "temporary-path")?;
    phase(
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o700)),
        "temporary-permissions",
    )?;
    copy_records(&before, &destination)?;
    let config_copy = destination.join("qualification-fleet.toml");
    private_write(&config_copy, &config_before.bytes)?;
    require(
        collect(source)? == before && read_record(config)? == config_before,
        "source-changed-during-copy",
    )?;
    require(
        !destination.join("agents").exists(),
        "provider-homes-copied",
    )?;
    let result = (|| {
        // No runtime, listener, router, background loop or API request is started.
        let controller = phase(
            Controller::new(
                &config_copy,
                &destination,
                uuid::Uuid::new_v4().simple().to_string(),
            ),
            "controller-new",
        )?;
        let registry = phase(templates::Registry::new(&destination), "template-registry")?;
        let catalog = phase(registry.list(), "template-decode")?;
        // Historical manifests also need the same parser; Registry::list only
        // loads the current manifests and their deployment receipts.
        for (path, record) in &before.records {
            if path.starts_with("templates/history") {
                let value = phase(
                    templates::decode_manifest_json(&record.bytes),
                    "template-history-decode",
                )?;
                phase(
                    templates::validate_manifest(value),
                    "template-history-validate",
                )?;
            }
        }
        let activated = phase(controller.activate(), "controller-activate")?;
        require(activated["active"] == true, "controller-activation-result")?;
        let drained = controller.drain();
        require(
            drained["drained"] == true
                && drained["active_jobs"] == 0
                && drained["active_requests"] == 0
                && drained["background_tasks"] == 0,
            "controller-drain",
        )?;
        drop(controller);
        // Activation sets an unreadable run record aside instead of refusing:
        // qualification still reports it.
        require(
            !destination.join("agent-runs-quarantine").exists(),
            "run-record-set-aside",
        )?;
        files_absent(&destination.join("agents"))?;
        for (path, record) in &before.records {
            if path.extension().and_then(|s| s.to_str()) != Some("lock") {
                require(
                    phase(fs::read(destination.join(path)), "copy-readback")? == record.bytes,
                    "completed-record-rewritten",
                )?;
            }
        }
        Ok(
            json!({"passed":true,"jobs":count(&before,"jobs"),"agent_runs":count(&before,"agent-runs"),
            "template_records":count(&before,"templates"),"current_templates":catalog.len(),
            "source_unchanged":true,"provider_state_copied":false,"background_started":false,
            "host_or_provider_api_calls":0}),
        )
    })();
    // Check source stability even when the copied controller rejects a record.
    require(
        collect(source)? == before && read_record(config)? == config_before,
        "source-changed-during-qualification",
    )?;
    phase(temporary.close(), "temporary-cleanup")?;
    result
}

/// Confine the opt-in Linux test thread. A future constructor accidentally
/// attempting a process launch or connection must fail instead of contacting
/// live hosts/providers. No sandbox is installed in the live service process.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn deny_external_effects() -> Checked<()> {
    let deny = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;
    let stmt = |code: u16, k: u32| libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    };
    let jump = |k: u32| libc::sock_filter {
        code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
        jt: 0,
        jf: 1,
        k,
    };
    let mut filter = vec![
        stmt((libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16, 4),
        libc::sock_filter {
            code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
            jt: 1,
            jf: 0,
            k: 0xc000003e,
        }, // AUDIT_ARCH_X86_64
        stmt((libc::BPF_RET | libc::BPF_K) as u16, deny),
    ];
    filter.push(stmt((libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16, 0));
    filter.push(libc::sock_filter {
        code: (libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K) as u16,
        jt: 0,
        jf: 1,
        k: 0x40000000,
    });
    filter.push(stmt((libc::BPF_RET | libc::BPF_K) as u16, deny));
    for syscall in [
        libc::SYS_socket,
        libc::SYS_socketpair,
        libc::SYS_connect,
        libc::SYS_execve,
        libc::SYS_execveat,
        libc::SYS_fork,
        libc::SYS_vfork,
        libc::SYS_clone,
        libc::SYS_clone3,
    ] {
        filter.push(jump(syscall as u32));
        filter.push(stmt((libc::BPF_RET | libc::BPF_K) as u16, deny));
    }
    filter.push(stmt(
        (libc::BPF_RET | libc::BPF_K) as u16,
        libc::SECCOMP_RET_ALLOW,
    ));
    let program = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_mut_ptr(),
    };
    // This thread makes no further child/thread/network operations. All pointers
    // refer to owned, live filter storage while the kernel copies the program.
    unsafe {
        require(
            libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0,
            "qualification-sandbox",
        )?;
        require(
            libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &program) == 0,
            "qualification-sandbox",
        )?;
    }
    Ok(())
}
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn deny_external_effects() -> Checked<()> {
    Err("live-qualification-requires-linux-x86-64")
}

#[test]
#[ignore = "explicit opt-in: reads completed legacy metadata only, never provider homes"]
fn qualify_actual_legacy_state_without_external_calls() {
    let run = || -> Checked<Value> {
        require(
            std::env::var("ENGINEBOX_QUALIFY_LEGACY_STATE")
                .ok()
                .as_deref()
                == Some("1"),
            "qualification-opt-in",
        )?;
        let source = PathBuf::from(
            std::env::var_os("ENGINEBOX_LEGACY_STATE_SOURCE").ok_or("source-required")?,
        );
        let config = PathBuf::from(
            std::env::var_os("ENGINEBOX_LEGACY_FLEET_CONFIG").ok_or("config-required")?,
        );
        deny_external_effects()?;
        qualify(&source, &config)
    };
    match run() {
        Ok(report) => println!("{report}"),
        Err(code) => {
            panic!("legacy metadata qualification failed: {code}; private contents suppressed")
        }
    }
}

#[test]
fn copies_only_completed_metadata_and_refuses_links_or_incomplete_records() {
    let run = || -> Checked<()> {
        let source = phase(tempfile::tempdir(), "fixture")?;
        let root = phase(source.path().canonicalize(), "fixture")?;
        for directory in [
            "jobs",
            "agent-runs",
            "templates",
            "templates/locks",
            "agents/homes",
        ] {
            phase(state::private_dir(&root.join(directory)), "fixture")?;
        }
        private_write(
            &root.join("agents/homes/private-token"),
            b"must never be copied",
        )?;
        let path = root.join("jobs/fixture.json");
        private_write(&path, br#"{"status":"succeeded"}"#)?;
        private_write(&root.join("templates/locks/registry.lock"), b"")?;
        let snapshot = collect(&root)?;
        let destination = phase(tempfile::tempdir(), "fixture")?;
        copy_records(&snapshot, destination.path())?;
        require(
            !destination.path().join("agents").exists(),
            "fixture-secret-copied",
        )?;
        require(
            !destination
                .path()
                .join("templates/locks/registry.lock")
                .exists(),
            "fixture-lock-copied",
        )?;
        for record in [
            br#"{"status":"running"}"#.as_slice(),
            br#"{"status":"failed","execution_uncertain":true}"#.as_slice(),
        ] {
            phase(fs::write(&path, record), "fixture")?;
            require(collect(&root).is_err(), "fixture-active-accepted")?;
        }
        phase(fs::remove_file(&path), "fixture")?;
        phase(
            std::os::unix::fs::symlink(root.join("agents/homes/private-token"), &path),
            "fixture",
        )?;
        require(collect(&root).is_err(), "fixture-link-accepted")?;
        Ok(())
    };
    assert!(
        run().is_ok(),
        "copy safety fixture failed; private contents suppressed"
    );
}

#[test]
fn disposable_controller_accepts_completed_records_and_rejects_invalid_runs() {
    let run = || -> Checked<()> {
        let source = phase(tempfile::tempdir(), "fixture")?;
        let root = phase(source.path().canonicalize(), "fixture")?;
        phase(
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)),
            "fixture",
        )?;
        for directory in ["jobs", "agent-runs", "templates"] {
            phase(state::private_dir(&root.join(directory)), "fixture")?;
        }
        let config = root.join("fleet.toml");
        private_write(
            &config,
            b"schema_version=1\n[[hosts]]\nid='worker'\ntarget='unused-fixture-host'\n",
        )?;
        let job_id = "11111111111111111111111111111111";
        private_write(
            &root.join(format!("jobs/{job_id}.json")),
            &phase(
                serde_json::to_vec(&json!({"id":job_id,"status":"succeeded"})),
                "fixture",
            )?,
        )?;
        let run_id = "22222222222222222222222222222222";
        let run_path = root.join(format!("agent-runs/{run_id}.json"));
        let mut record = json!({"id":run_id,"host_id":"worker","name":"ebox-fixture",
            "provider":"claude","scope":"controller-owner","state_scope":"private-workspace",
            "status":"failed","workdir":".","resume_id":null,
            "created_at":"2026-10-20T00:00:00Z","error":"fixture"});
        private_write(&run_path, &phase(serde_json::to_vec(&record), "fixture")?)?;
        let report = qualify(&root, &config)?;
        require(
            report["passed"] == true
                && report["jobs"] == 1
                && report["agent_runs"] == 1
                && report["source_unchanged"] == true,
            "fixture-qualification-result",
        )?;
        // A small scalar field from another release is tolerated.
        record["unknown_legacy_field"] = json!(true);
        phase(
            fs::write(&run_path, phase(serde_json::to_vec(&record), "fixture")?),
            "fixture",
        )?;
        require(
            qualify(&root, &config).is_ok_and(|r| r["passed"] == true),
            "fixture-tolerated-field-refused",
        )?;
        // A structured unknown field is not: the record is set aside, never
        // loaded, and qualification reports it without refusing activation.
        record["unknown_legacy_field"] = json!({"nested":true});
        phase(
            fs::write(&run_path, phase(serde_json::to_vec(&record), "fixture")?),
            "fixture",
        )?;
        let before = collect(&root)?;
        require(
            qualify(&root, &config) == Err("run-record-set-aside"),
            "fixture-invalid-run-accepted",
        )?;
        require(collect(&root)? == before, "fixture-source-modified")?;
        Ok(())
    };
    if let Err(code) = run() {
        panic!("disposable controller fixture failed: {code}; private contents suppressed");
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn qualification_sandbox_blocks_connections_and_process_launches() {
    assert!(deny_external_effects().is_ok());
    // Creating an unbound local socket does not contact another process, even
    // if a filter regression were to allow it. Close it before failing then.
    let socket = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    let error = std::io::Error::last_os_error().raw_os_error();
    if socket >= 0 {
        unsafe { libc::close(socket) };
    }
    assert!(socket < 0 && error == Some(libc::EPERM));
    // This path cannot be an executable; a missing filter would yield ENOENT.
    let launched = std::process::Command::new("/proc/self/enginebox-no-such-executable").spawn();
    assert!(launched.is_err_and(|error| error.raw_os_error() == Some(libc::EPERM)));
}
