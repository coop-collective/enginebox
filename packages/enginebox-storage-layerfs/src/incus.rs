//! Attach only a private checkout to an owned Incus VM. Store and provider homes
//! never cross this boundary. Callers hold the workspace lease before entering.
use crate::config::{digest, identifier, real_path};
use anyhow::{Context, Result, ensure};
use enginebox_core::{state, transport::run_process};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

const DEVICE: &str = "ebox-layerfs";
const MARKER: &str = "user.enginebox.layerfs";
const WORKSPACE_MARKER: &str = "user.enginebox.layerfs.workspace";
const MAPPED: &str = "/enginebox-incus";
const STATEFUL: &str = "migration.stateful";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub project: String,
    pub instance: String,
    pub owner: String,
    pub uid: u32,
    pub gid: u32,
}
impl Target {
    fn validate(&self) -> Result<()> {
        for value in [&self.project, &self.instance, &self.owner] {
            identifier(value)?;
        }
        ensure!(
            self.instance.starts_with("ebox-"),
            "Only Engine Box instances may receive checkout exports"
        );
        ensure!(
            self.instance != "ebox-base" && !self.instance.starts_with("ebox-template-"),
            "Reusable base instances cannot receive private checkout exports"
        );
        ensure!(
            (1..=65535).contains(&self.uid) && (1..=65535).contains(&self.gid),
            "Guest checkout identity must be an unprivileged UID/GID in 1–65535"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Attachment {
    schema: u32,
    workspace: String,
    id: String,
    target: Target,
    instance_uuid: String,
    prior_stateful: Option<String>,
    container: String,
    container_pid: u64,
    host_boot_id: String,
    container_started_at: String,
    export: PathBuf,
    mount_device: Option<u64>,
    mount_inode: Option<u64>,
    state: String,
    last_error: Option<String>,
}

fn receipt(root: &Path, name: &str) -> PathBuf {
    root.join("attachments").join(format!("{name}.json"))
}
fn save(root: &Path, a: &Attachment) -> Result<()> {
    state::write_json(&receipt(root, &a.workspace), &serde_json::to_value(a)?)
}
fn load(root: &Path, name: &str) -> Result<Attachment> {
    identifier(name)?;
    let a: Attachment = serde_json::from_value(state::read_json(&receipt(root, name))?)?;
    a.target.validate()?;
    ensure!(
        a.schema == 2
            && a.workspace == name
            && a.export == root.join("exports").join(name)
            && digest(&a.container),
        "Invalid Incus attachment receipt"
    );
    ensure!(
        uuid::Uuid::parse_str(&a.id).is_ok() && uuid::Uuid::parse_str(&a.instance_uuid).is_ok(),
        "Invalid attachment identity"
    );
    ensure!(
        matches!(a.prior_stateful.as_deref(), None | Some("true" | "false")),
        "Invalid saved VM stateful configuration"
    );
    Ok(a)
}

/// Reject all command/checkpoint/remount/close/recovery activity until an exact
/// attachment has been detached. An uncertain receipt remains an active lease.
pub fn ensure_detached(root: &Path, name: &str) -> Result<()> {
    identifier(name)?;
    if receipt(root, name).exists() {
        ensure!(
            load(root, name)?.state == "detached",
            "Workspace is attached to Incus or its attachment is uncertain; stop and detach its VM before storage operations"
        );
    }
    Ok(())
}

fn command(args: Vec<String>, seconds: u64) -> Result<Vec<u8>> {
    run_process(&args, None, Duration::from_secs(seconds))
}
fn docker(docker_socket: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut argv = vec![
        "docker".into(),
        "--host".into(),
        format!("unix://{}", docker_socket.display()),
    ];
    argv.extend(args.iter().map(|v| (*v).into()));
    command(argv, 30)
}
fn incus(target: &Target, args: &[&str]) -> Result<Vec<u8>> {
    let mut argv = vec!["incus".into(), "--project".into(), target.project.clone()];
    argv.extend(args.iter().map(|v| (*v).into()));
    command(argv, 45)
}
fn instance(target: &Target) -> Result<Value> {
    Ok(serde_json::from_slice(&command(
        vec![
            "incus".into(),
            "query".into(),
            format!(
                "/1.0/instances/{}?project={}",
                target.instance, target.project
            ),
        ],
        30,
    )?)?)
}
fn expanded<'a>(v: &'a Value, key: &str) -> &'a Value {
    v.get("expanded_config")
        .and_then(|c| c.get(key))
        .unwrap_or(&v["config"][key])
}

/// A pure preflight shared by execution and contract tests. Read ownership from
/// expanded configuration because managed profiles may supply it.
pub fn validate_instance(v: &Value, target: &Target, stopped: bool) -> Result<String> {
    target.validate()?;
    ensure!(
        v["name"] == target.instance && v["type"] == "virtual-machine",
        "Target must be the exact requested VM"
    );
    ensure!(
        expanded(v, "user.enginebox.owner").as_str() == Some(target.owner.as_str()),
        "Refusing VM with a different Engine Box owner"
    );
    ensure!(
        expanded(v, "user.enginebox.clean-base") != "true"
            && !matches!(
                expanded(v, "user.enginebox.pool.state").as_str(),
                Some("ready" | "preparing" | "quarantined")
            ),
        "Cannot attach a private checkout to a reusable base or unclaimed warm VM"
    );
    ensure!(
        expanded(v, "raw.idmap").as_str().is_none_or(str::is_empty),
        "Additional VM identity mapping is not qualified for checkout exports"
    );
    ensure!(
        v["stateful"] == false,
        "Memory state is incompatible with checkout attach/detach"
    );
    if stopped {
        ensure!(
            v["status"] == "Stopped",
            "Incus checkout attachment requires a cleanly stopped VM"
        );
    }
    let id = expanded(v, "volatile.uuid")
        .as_str()
        .context("VM UUID missing")?;
    uuid::Uuid::parse_str(id).context("VM UUID is invalid")?;
    Ok(id.into())
}
fn expected_device(a: &Attachment) -> Value {
    json!({"type":"disk","source":a.export,"path":"/project","io.bus":"virtiofs","io.cache":"none","required":"true"})
}
fn ensure_unattached(v: &Value) -> Result<()> {
    ensure!(
        [MARKER, WORKSPACE_MARKER]
            .iter()
            .all(|key| v["config"].get(*key).is_none() && v["expanded_config"].get(*key).is_none()),
        "VM already carries an attachment marker"
    );
    Ok(())
}
fn prior_stateful(v: &Value) -> Result<Option<String>> {
    match v["config"].get(STATEFUL) {
        None => Ok(None),
        Some(Value::String(value)) if matches!(value.as_str(), "true" | "false") => {
            Ok(Some(value.clone()))
        }
        _ => anyhow::bail!("Unsupported local VM stateful setting; refusing to overwrite it"),
    }
}
fn check_custody(a: &Attachment, stopped: bool) -> Result<Value> {
    let v = instance(&a.target)?;
    ensure!(
        validate_instance(&v, &a.target, stopped)? == a.instance_uuid,
        "VM was replaced; refusing its old attachment receipt"
    );
    ensure!(
        v["config"][MARKER] == a.id && v["config"][WORKSPACE_MARKER] == a.workspace,
        "VM attachment ownership changed"
    );
    ensure!(
        v["config"][STATEFUL] == "false",
        "VM stateful override changed; preserve for inspection"
    );
    Ok(v)
}
fn check_instance(a: &Attachment, stopped: bool) -> Result<Value> {
    let v = check_custody(a, stopped)?;
    ensure!(
        v["devices"][DEVICE] == expected_device(a),
        "VM checkout device changed; preserve for inspection"
    );
    Ok(v)
}
fn container_pid(docker_socket: &Path, id: &str) -> Result<u64> {
    ensure!(digest(id), "Invalid Docker container ID");
    let v: Value = serde_json::from_slice(&docker(docker_socket, &["inspect", id])?)?;
    ensure!(
        v[0]["Id"] == id && v[0]["State"]["Running"] == true,
        "Checkout container is not running"
    );
    let pid = v[0]["State"]["Pid"]
        .as_u64()
        .context("Container PID missing")?;
    ensure!(pid > 1, "Invalid container PID");
    Ok(pid)
}
fn container_started_at(docker_socket: &Path, id: &str) -> Result<String> {
    let v: Value = serde_json::from_slice(&docker(docker_socket, &["inspect", id])?)?;
    Ok(v[0]["State"]["StartedAt"]
        .as_str()
        .context("Container start time missing")?
        .into())
}
fn host_boot_id() -> Result<String> {
    Ok(fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .into())
}
fn mount_identity(path: &Path) -> Result<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = fs::metadata(path)?;
        ensure!(m.is_dir(), "Checkout export must be a directory");
        Ok((m.dev(), m.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        anyhow::bail!("Incus checkout exports require Linux")
    }
}
/// Clone only the mapped mount in its namespace, then attach the detached tree
/// in the host namespace. Legacy bind mounts cannot cross mount namespaces.
#[cfg(target_os = "linux")]
fn export_mount(pid: u64, target: &Path) -> Result<()> {
    use std::{
        os::{
            fd::AsRawFd,
            unix::{fs::OpenOptionsExt, process::CommandExt},
        },
        process::{Command, Stdio},
    };
    let host_ns = fs::File::open("/proc/self/ns/mnt")?;
    let source_ns = fs::File::open(format!("/proc/{pid}/ns/mnt"))?;
    let source = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
        .open(format!("/proc/{pid}/root{MAPPED}"))?;
    let destination = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
        .open(target)?;
    let (host_fd, namespace_fd, source_fd, target_fd) = (
        host_ns.as_raw_fd(),
        source_ns.as_raw_fd(),
        source.as_raw_fd(),
        destination.as_raw_fd(),
    );
    let mut child = Command::new("/usr/bin/true");
    child
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Only this short-lived child changes namespaces. No allocation, locks,
    // formatting or path lookup in the post-fork callback; descriptors remain
    // alive through spawn. All operations below are Linux syscalls.
    unsafe {
        child.pre_exec(move || {
            if libc::setns(namespace_fd, libc::CLONE_NEWNS) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mount_fd = libc::syscall(
                libc::SYS_open_tree,
                source_fd,
                c"".as_ptr(),
                1u32 | libc::O_CLOEXEC as u32 | libc::AT_EMPTY_PATH as u32,
            ) as i32;
            if mount_fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::setns(host_fd, libc::CLONE_NEWNS) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // MOVE_MOUNT_F_EMPTY_PATH | MOVE_MOUNT_T_EMPTY_PATH.
            if libc::syscall(
                libc::SYS_move_mount,
                mount_fd,
                c"".as_ptr(),
                target_fd,
                c"".as_ptr(),
                0x44u32,
            ) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            libc::close(mount_fd);
            Ok(())
        });
    }
    ensure!(child.status().context("Cannot import mapped checkout mount from its Docker namespace (Linux open_tree/move_mount required)")?.success(), "Mount export helper failed");
    Ok(())
}
#[cfg(not(target_os = "linux"))]
fn export_mount(_: u64, _: &Path) -> Result<()> {
    anyhow::bail!("Incus checkout exports require Linux open_tree/move_mount")
}

fn check_export(docker_socket: &Path, a: &Attachment) -> Result<()> {
    ensure!(
        host_boot_id()? == a.host_boot_id
            && container_started_at(docker_socket, &a.container)? == a.container_started_at,
        "Host or checkout container restarted; preserve the external mount for recovery inspection"
    );
    ensure!(
        container_pid(docker_socket, &a.container)? == a.container_pid,
        "Checkout container restarted; export identity is uncertain"
    );
    real_path(&a.export)?;
    let m: Value = serde_json::from_slice(&command(
        vec![
            "findmnt".into(),
            "-J".into(),
            "-M".into(),
            a.export.to_string_lossy().into_owned(),
            "-o".into(),
            "TARGET,FSTYPE,SOURCE".into(),
        ],
        10,
    )?)?;
    ensure!(
        m["filesystems"][0]["fstype"] == "fuse.bindfs"
            && m["filesystems"][0]["source"] == "/project",
        "Mapped checkout export is no longer mounted"
    );
    ensure!(
        mount_identity(&a.export)?
            == (
                a.mount_device.context("Export device missing")?,
                a.mount_inode.context("Export inode missing")?
            ),
        "Checkout export mount changed"
    );
    ensure!(
        mount_identity(Path::new(&format!(
            "/proc/{}/root{MAPPED}",
            a.container_pid
        )))? == mount_identity(&a.export)?,
        "Sandbox mapped mount no longer matches its host export"
    );
    Ok(())
}
fn finish(root: &Path, a: &mut Attachment, result: Result<Value>, success: &str) -> Result<Value> {
    match result {
        Ok(v) => {
            a.state = success.into();
            a.last_error = None;
            save(root, a)?;
            Ok(v)
        }
        Err(e) => {
            a.state = "uncertain".into();
            a.last_error = Some(format!("{e:#}"));
            save(root, a)?;
            Err(e)
        }
    }
}

/// Caller verifies the ready workspace's container ownership and holds its lease.
/// On any partial failure no automatic rollback/retry discards the exported state.
pub fn attach(
    root: &Path,
    name: &str,
    container: &str,
    target: Target,
    docker_socket: &Path,
) -> Result<Value> {
    ensure!(cfg!(target_os = "linux"), "Incus attachment requires Linux");
    identifier(name)?;
    target.validate()?;
    real_path(root)?;
    let _host = state::root_host_lock()?;
    ensure_detached(root, name)?;
    let v = instance(&target)?;
    let instance_uuid = validate_instance(&v, &target, true)?;
    ensure_unattached(&v)?;
    ensure!(
        v["expanded_devices"]
            .as_object()
            .context("VM devices missing")?
            .values()
            .all(|d| d["path"] != "/project")
            && v["devices"].get(DEVICE).is_none(),
        "VM already has a project device"
    );
    let pid = container_pid(docker_socket, container)?;
    // bindfs stays within the existing workspace's resource budget. It maps
    // projected identities without a tree walk, chmod, or recursive chown.
    let version = String::from_utf8(docker(
        docker_socket,
        &["exec", "--user=0", container, "bindfs", "--version"],
    )?)?;
    ensure!(
        version.trim() == "bindfs 1.14.7",
        "Incus ownership mapping requires qualified bindfs 1.14.7"
    );
    docker(
        docker_socket,
        &[
            "exec",
            "--user=0",
            container,
            "sh",
            "-c",
            "test ! -e /enginebox-incus && test ! -L /enginebox-incus && mountpoint -q /project",
        ],
    )?;
    state::private_dir(&root.join("attachments"))?;
    state::private_dir(&root.join("exports"))?;
    let export = root.join("exports").join(name);
    ensure!(!export.exists(), "Export path already exists");
    let mut a = Attachment {
        schema: 2,
        workspace: name.into(),
        id: uuid::Uuid::new_v4().to_string(),
        target,
        instance_uuid,
        prior_stateful: prior_stateful(&v)?,
        container: container.into(),
        container_pid: pid,
        host_boot_id: host_boot_id()?,
        container_started_at: container_started_at(docker_socket, container)?,
        export,
        mount_device: None,
        mount_inode: None,
        state: "attaching".into(),
        last_error: None,
    };
    if receipt(root, name).exists() {
        let prior = load(root, name)?;
        ensure!(
            prior.state == "detached",
            "Prior attachment is not cleanly detached"
        );
        let history = root.join("attachments/history");
        state::private_dir(&history)?;
        state::write_json(
            &history.join(format!("{name}-{}.json", prior.id)),
            &serde_json::to_value(prior)?,
        )?;
    }
    save(root, &a)?;
    let result = (|| {
        incus(
            &a.target,
            &[
                "config",
                "set",
                &a.target.instance,
                &format!("{MARKER}={}", a.id),
                &format!("{WORKSPACE_MARKER}={name}"),
                "migration.stateful=false",
            ],
        )?;
        docker(
            docker_socket,
            &["exec", "--user=0", container, "mkdir", "-m", "0700", MAPPED],
        )?;
        docker(
            docker_socket,
            &[
                "exec",
                "--user=0",
                container,
                "bindfs",
                &format!("--map=501/{}:@20/@{}", a.target.uid, a.target.gid),
                "--disable-lock-forwarding",
                "-o",
                "subtype=bindfs",
                "/project",
                MAPPED,
            ],
        )?;
        state::private_dir(&a.export)?;
        export_mount(pid, &a.export)?;
        let (dev, ino) = mount_identity(&a.export)?;
        a.mount_device = Some(dev);
        a.mount_inode = Some(ino);
        save(root, &a)?;
        check_export(docker_socket, &a)?;
        incus(
            &a.target,
            &[
                "config",
                "device",
                "add",
                &a.target.instance,
                DEVICE,
                "disk",
                &format!("source={}", a.export.display()),
                "path=/project",
                "io.bus=virtiofs",
                "io.cache=none",
                "required=true",
            ],
        )?;
        check_instance(&a, true)?;
        Ok(
            json!({"ok":true,"workspace":name,"attachment":a.id,"project":a.target.project,"instance":a.target.instance,"path":"/project","guest_uid":a.target.uid,"guest_gid":a.target.gid,"status":"attached","ready":false,"memory_checkpoint_supported":false}),
        )
    })();
    finish(root, &mut a, result, "attached")
}

/// Verify actual guest mount and unprivileged permissions after normal VM boot.
/// This never starts, resumes, or changes a guest on the caller's behalf.
pub fn verify(root: &Path, name: &str, docker_socket: &Path) -> Result<Value> {
    let _host = state::root_host_lock()?;
    let a = load(root, name)?;
    ensure!(
        a.state == "attached",
        "Attachment is {}; no automatic repair is allowed",
        a.state
    );
    let v = check_instance(&a, false)?;
    ensure!(
        v["status"] == "Running",
        "Guest readiness requires a running VM"
    );
    check_export(docker_socket, &a)?;
    let mount: Value = serde_json::from_slice(&incus(
        &a.target,
        &[
            "exec",
            &a.target.instance,
            &format!("--user={}", a.target.uid),
            &format!("--group={}", a.target.gid),
            "--",
            "findmnt",
            "-J",
            "-M",
            "/project",
            "-o",
            "FSTYPE,TARGET",
        ],
    )?)?;
    ensure!(
        mount["filesystems"][0]["fstype"] == "virtiofs"
            && mount["filesystems"][0]["target"] == "/project",
        "Guest project path is not the expected virtiofs mount"
    );
    incus(
        &a.target,
        &[
            "exec",
            &a.target.instance,
            &format!("--user={}", a.target.uid),
            &format!("--group={}", a.target.gid),
            "--",
            "sh",
            "-c",
            "test -r /project/.git/HEAD && test -w /project && test ! -e /layerfs-store && test ! -S /var/run/docker.sock",
        ],
    )?;
    Ok(
        json!({"ok":true,"workspace":name,"ready":true,"path":"/project","guest_uid":a.target.uid,"guest_gid":a.target.gid,"memory_checkpoint_supported":false}),
    )
}

/// Stop the VM explicitly before detaching. Guest shutdown precedes releasing
/// the export; only then may Runtime checkpoint/remount/close the LayerFS branch.
pub fn detach(root: &Path, name: &str, docker_socket: &Path) -> Result<Value> {
    let _host = state::root_host_lock()?;
    let mut a = load(root, name)?;
    ensure!(
        a.state == "attached",
        "Attachment is {}; preserve it for inspection",
        a.state
    );
    check_instance(&a, true)?;
    check_export(docker_socket, &a)?;
    a.state = "detaching".into();
    save(root, &a)?;
    let result = (|| {
        incus(
            &a.target,
            &["config", "device", "remove", &a.target.instance, DEVICE],
        )?;
        command(
            vec!["umount".into(), a.export.to_string_lossy().into_owned()],
            15,
        )?;
        docker(
            docker_socket,
            &["exec", "--user=0", &a.container, "umount", MAPPED],
        )?;
        docker(
            docker_socket,
            &["exec", "--user=0", &a.container, "rmdir", MAPPED],
        )?;
        fs::remove_dir(&a.export)?;
        check_custody(&a, true)?;
        if let Some(value) = &a.prior_stateful {
            incus(
                &a.target,
                &[
                    "config",
                    "set",
                    &a.target.instance,
                    &format!("{STATEFUL}={value}"),
                ],
            )?;
        } else {
            incus(
                &a.target,
                &["config", "unset", &a.target.instance, STATEFUL],
            )?;
        }
        incus(&a.target, &["config", "unset", &a.target.instance, MARKER])?;
        incus(
            &a.target,
            &["config", "unset", &a.target.instance, WORKSPACE_MARKER],
        )?;
        Ok(json!({"ok":true,"workspace":name,"status":"detached","checkpoint_required":true}))
    })();
    finish(root, &mut a, result, "detached")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn target() -> Target {
        Target {
            project: "enginebox".into(),
            instance: "ebox-private".into(),
            owner: "enginebox".into(),
            uid: 1001,
            gid: 1001,
        }
    }
    fn instance() -> Value {
        json!({"name":"ebox-private","type":"virtual-machine","status":"Stopped","stateful":false,"config":{},"expanded_config":{"user.enginebox.owner":"enginebox","volatile.uuid":"c3f7a3ae-164d-45d5-b79d-96e0d6f54a8c"}})
    }
    #[test]
    fn rejects_unowned_running_stateful_and_reusable_guests() {
        assert!(validate_instance(&instance(), &target(), true).is_ok());
        for (section, key, value) in [
            ("", "status", json!("Running")),
            ("", "stateful", json!(true)),
            ("", "type", json!("container")),
            ("expanded_config", "user.enginebox.owner", json!("another")),
            ("expanded_config", "raw.idmap", json!("both 1001 501")),
            (
                "expanded_config",
                "user.enginebox.clean-base",
                json!("true"),
            ),
            (
                "expanded_config",
                "user.enginebox.pool.state",
                json!("ready"),
            ),
            (
                "expanded_config",
                "user.enginebox.pool.state",
                json!("preparing"),
            ),
            (
                "expanded_config",
                "user.enginebox.pool.state",
                json!("quarantined"),
            ),
        ] {
            let mut v = instance();
            if section.is_empty() {
                v[key] = value;
            } else {
                v[section][key] = value;
            }
            assert!(
                validate_instance(&v, &target(), true).is_err(),
                "accepted {key}"
            );
        }
    }
    #[test]
    fn stateful_setting_preserves_local_inheritance_boundary() -> Result<()> {
        let mut v = instance();
        v["expanded_config"][STATEFUL] = json!("true");
        assert_eq!(prior_stateful(&v)?, None);
        for value in ["true", "false"] {
            v["config"][STATEFUL] = json!(value);
            assert_eq!(prior_stateful(&v)?, Some(value.into()));
        }
        for value in [json!("unknown"), json!(true), Value::Null] {
            v["config"][STATEFUL] = value;
            assert!(prior_stateful(&v).is_err());
        }
        Ok(())
    }
    #[test]
    fn partial_attachment_markers_cannot_be_overwritten() {
        for section in ["config", "expanded_config"] {
            for marker in [MARKER, WORKSPACE_MARKER] {
                let mut value = instance();
                value[section][marker] = json!("");
                assert!(ensure_unattached(&value).is_err());
            }
        }
    }
    #[test]
    fn refuses_unsafe_targets_and_root_guest_identity() {
        for project in ["remote:project", "../private", "x?project=y"] {
            let mut t = target();
            t.project = project.into();
            assert!(validate_instance(&instance(), &t, true).is_err());
        }
        let mut t = target();
        t.uid = 0;
        assert!(validate_instance(&instance(), &t, true).is_err());
        for name in ["ebox-base", "ebox-template-example"] {
            let mut t = target();
            t.instance = name.into();
            let mut value = instance();
            value["name"] = json!(name);
            assert!(validate_instance(&value, &t, true).is_err());
        }
    }
    #[test]
    fn uncertain_and_in_progress_receipts_block_storage_operations() -> Result<()> {
        let dir = tempfile::tempdir()?;
        state::private_dir(&dir.path().join("attachments"))?;
        ensure_detached(dir.path(), "checkout")?;
        let mut a = Attachment {
            schema: 2,
            workspace: "checkout".into(),
            id: uuid::Uuid::new_v4().to_string(),
            target: target(),
            instance_uuid: uuid::Uuid::new_v4().to_string(),
            prior_stateful: None,
            container: "a".repeat(64),
            container_pid: 100,
            host_boot_id: "test-boot".into(),
            container_started_at: "test-time".into(),
            export: dir.path().join("exports/checkout"),
            mount_device: None,
            mount_inode: None,
            state: String::new(),
            last_error: None,
        };
        for status in ["attaching", "attached", "detaching", "uncertain"] {
            a.state = status.into();
            save(dir.path(), &a)?;
            assert!(ensure_detached(dir.path(), "checkout").is_err());
        }
        a.state = "detached".into();
        save(dir.path(), &a)?;
        ensure_detached(dir.path(), "checkout")?;
        a.export = PathBuf::from("/unrelated");
        save(dir.path(), &a)?;
        assert!(ensure_detached(dir.path(), "checkout").is_err());
        Ok(())
    }
}
