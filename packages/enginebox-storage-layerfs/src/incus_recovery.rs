//! Reconcile completed external effects, never replay attachment commands.
//! An interrupted or restarted mount remains in custody until its exact state
//! is proved. This module changes only private operation metadata.
use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct Check<T> {
    value: Option<T>,
    error: Option<String>,
}
impl<T> Check<T> {
    fn from_result(result: Result<T>) -> Self {
        match result {
            Ok(value) => Self {
                value: Some(value),
                error: None,
            },
            Err(error) => Self {
                value: None,
                error: Some(format!("{error:#}")),
            },
        }
    }
    fn unavailable(message: &str) -> Self {
        Self {
            value: None,
            error: Some(message.into()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Presence {
    Exact,
    Absent,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct Vm {
    uuid: String,
    stopped: bool,
    markers: Presence,
    device: Presence,
    local_stateful: Option<String>,
}

fn vm_view(a: &Attachment, value: &Value) -> Result<Vm> {
    let uuid = validate_instance(value, &a.target, false)?;
    ensure!(
        uuid == a.instance_uuid,
        "VM UUID changed; preserve the old attachment"
    );
    let config = value["config"].as_object().context("VM config missing")?;
    let expanded_config = value["expanded_config"]
        .as_object()
        .context("Expanded VM config missing")?;
    let marker_pairs = [
        (MARKER, a.id.as_str()),
        (WORKSPACE_MARKER, a.workspace.as_str()),
    ];
    let markers = if marker_pairs.iter().all(|(key, expected)| {
        config.get(*key).and_then(Value::as_str) == Some(*expected)
            && expanded_config.get(*key).and_then(Value::as_str) == Some(*expected)
    }) {
        Presence::Exact
    } else if marker_pairs
        .iter()
        .all(|(key, _)| !config.contains_key(*key) && !expanded_config.contains_key(*key))
    {
        Presence::Absent
    } else {
        Presence::Other
    };
    let devices = value["devices"]
        .as_object()
        .context("Local VM devices missing")?;
    let expanded_devices = value["expanded_devices"]
        .as_object()
        .context("Expanded VM devices missing")?;
    let expected = expected_device(a);
    let other_project_device = expanded_devices
        .iter()
        .any(|(name, device)| name != DEVICE && device["path"] == "/project");
    let device = if !other_project_device
        && devices.get(DEVICE) == Some(&expected)
        && expanded_devices.get(DEVICE) == Some(&expected)
    {
        Presence::Exact
    } else if !other_project_device
        && !devices.contains_key(DEVICE)
        && !expanded_devices.contains_key(DEVICE)
    {
        Presence::Absent
    } else {
        Presence::Other
    };
    Ok(Vm {
        uuid,
        stopped: value["status"] == "Stopped",
        markers,
        device,
        local_stateful: prior_stateful(value)?,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct Container {
    id: String,
    running: bool,
    restarting: bool,
    dead: bool,
    pid: u64,
    started_at: String,
}
fn container_view(value: &Value) -> Result<Container> {
    let value = value
        .as_array()
        .filter(|items| items.len() == 1)
        .context("Expected one Docker container")?
        .first()
        .unwrap();
    Ok(Container {
        id: value["Id"].as_str().context("Container ID missing")?.into(),
        running: value["State"]["Running"]
            .as_bool()
            .context("Container running state missing")?,
        restarting: value["State"]["Restarting"]
            .as_bool()
            .context("Container restart state missing")?,
        dead: value["State"]["Dead"]
            .as_bool()
            .context("Container dead state missing")?,
        pid: value["State"]["Pid"]
            .as_u64()
            .context("Container PID missing")?,
        started_at: value["State"]["StartedAt"]
            .as_str()
            .context("Container start time missing")?
            .into(),
    })
}
fn original_container(a: &Attachment, container: &Container) -> bool {
    container.id == a.container
        && container.running
        && !container.restarting
        && !container.dead
        && container.pid > 1
        && container.pid == a.container_pid
        && container.started_at == a.container_started_at
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum PathView {
    Absent,
    UnmountedDirectory,
    Mounted {
        fs_type: String,
        source: String,
        root: String,
        device: u64,
        inode: u64,
    },
}

fn unescape_mount_field(field: &str) -> Result<String> {
    let mut result = Vec::new();
    let bytes = field.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            let escape = bytes.get(i..i + 4).context("Truncated mountinfo escape")?;
            result.push(match escape {
                b"\\040" => b' ',
                b"\\011" => b'\t',
                b"\\012" => b'\n',
                b"\\134" => b'\\',
                _ => anyhow::bail!("Unknown mountinfo escape"),
            });
            i += 4;
        } else {
            result.push(bytes[i]);
            i += 1;
        }
    }
    Ok(String::from_utf8(result)?)
}

/// Read the kernel mount table first: an absent mount must not be mistaken for
/// the empty directory underneath it. Reject stacked mounts at this target.
fn mount_at(table: &str, target: &str) -> Result<Option<(String, String, String)>> {
    let mut found = None;
    for line in table.lines() {
        let (before, after) = line.split_once(" - ").context("Invalid mountinfo record")?;
        let fields = before.split_whitespace().collect::<Vec<_>>();
        let details = after.split_whitespace().collect::<Vec<_>>();
        ensure!(
            fields.len() >= 6 && details.len() >= 3,
            "Incomplete mountinfo record"
        );
        if unescape_mount_field(fields[4])? == target {
            ensure!(
                found.is_none(),
                "Stacked export mounts require manual inspection"
            );
            found = Some((
                details[0].into(),
                unescape_mount_field(details[1])?,
                unescape_mount_field(fields[3])?,
            ));
        }
    }
    Ok(found)
}
fn path_view(table: &str, target: &str, path: &Path) -> Result<PathView> {
    if let Some((fs_type, source, root)) = mount_at(table, target)? {
        // Bound stat in a child; don't issue a potentially stuck FUSE lookup in
        // the reconciler itself. No checkout files are opened or changed.
        let output = command(
            vec![
                "stat".into(),
                "-L".into(),
                "--format=%d:%i:%f".into(),
                "--".into(),
                path.to_string_lossy().into_owned(),
            ],
            10,
        )?;
        let text = std::str::from_utf8(&output)?.trim();
        let fields = text.split(':').collect::<Vec<_>>();
        ensure!(
            fields.len() == 3 && u32::from_str_radix(fields[2], 16)? & 0o170000 == 0o040000,
            "Export is not a directory"
        );
        Ok(PathView::Mounted {
            fs_type,
            source,
            root,
            device: fields[0].parse()?,
            inode: fields[1].parse()?,
        })
    } else {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                ensure!(
                    metadata.is_dir() && !metadata.file_type().is_symlink(),
                    "Unexpected file or symlink at export path"
                );
                Ok(PathView::UnmountedDirectory)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(PathView::Absent),
            Err(error) => Err(error.into()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct Observation {
    boot_id: Check<String>,
    vm: Check<Vm>,
    container: Check<Container>,
    export: Check<PathView>,
    mapped: Check<PathView>,
}
fn observe(a: &Attachment, docker_socket: &Path) -> Observation {
    let boot_id = Check::from_result(host_boot_id());
    let vm = Check::from_result(instance(&a.target).and_then(|value| vm_view(a, &value)));
    // Socket checks are separate from artifacts: inspection remains useful if
    // the daemon or SDK files are unavailable after a host restart.
    let container = Check::from_result((|| {
        crate::runtime::verify_socket(docker_socket)?;
        container_view(&serde_json::from_slice(&docker(
            docker_socket,
            &["inspect", &a.container],
        )?)?)
    })());
    let (export, mapped) = if boot_id.value.as_deref() == Some(a.host_boot_id.as_str())
        && container
            .value
            .as_ref()
            .is_some_and(|c| original_container(a, c))
    {
        let export = Check::from_result((|| {
            real_path(a.export.parent().context("Export parent missing")?)?;
            path_view(
                &fs::read_to_string("/proc/self/mountinfo")?,
                a.export.to_str().context("Export path is not UTF-8")?,
                &a.export,
            )
        })());
        let mapped = Check::from_result((|| {
            path_view(
                &fs::read_to_string(format!("/proc/{}/mountinfo", a.container_pid))?,
                MAPPED,
                Path::new(&format!("/proc/{}/root{MAPPED}", a.container_pid)),
            )
        })());
        (export, mapped)
    } else {
        (
            Check::unavailable(
                "Old mount namespace not accessed without original host/container identity",
            ),
            Check::unavailable(
                "Old mount namespace not accessed without original host/container identity",
            ),
        )
    };
    Observation {
        boot_id,
        vm,
        container,
        export,
        mapped,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct Decision {
    target_state: Option<&'static str>,
    reason: &'static str,
    next_action: &'static str,
}
fn blocked(reason: &'static str, next_action: &'static str) -> Decision {
    Decision {
        target_state: None,
        reason,
        next_action,
    }
}
fn intent(a: &Attachment) -> Option<Operation> {
    a.last_operation.or(match a.state.as_str() {
        "attaching" | "attached" => Some(Operation::Attach),
        "detaching" | "detached" => Some(Operation::Detach),
        _ => None,
    })
}
fn decide(a: &Attachment, o: &Observation) -> Decision {
    let Some(boot) = o.boot_id.value.as_deref() else {
        return blocked(
            "host-unavailable",
            "Inspect the host boot identity; leave the attachment and private state intact.",
        );
    };
    if boot != a.host_boot_id {
        return blocked(
            "host-restarted",
            "The original mount namespace is gone. Keep the VM stopped and preserve the receipt, container and overlay. This command does not re-export mounts or restore a checkpoint after reboot.",
        );
    }
    let Some(vm) = o.vm.value.as_ref() else {
        return blocked(
            "vm-unverified",
            "Inspect the exact VM UUID and owner; do not adopt a replacement VM or remove its devices.",
        );
    };
    if !vm.stopped {
        return blocked(
            "vm-not-stopped",
            "Wait for the owner to cleanly stop the exact VM before reconciliation. This command never stops a workspace.",
        );
    }
    if !o
        .container
        .value
        .as_ref()
        .is_some_and(|c| original_container(a, c))
    {
        return blocked(
            "container-unverified",
            "Inspect the original Docker container and daemon; retain its overlay. A restarted, stopped, replaced or unavailable container cannot prove the old mount.",
        );
    }
    let (Some(export), Some(mapped)) = (&o.export.value, &o.mapped.value) else {
        return blocked(
            "mount-unverified",
            "Inspect the host export and original sandbox mount; no mount is recreated, unmounted or discarded by reconciliation.",
        );
    };
    match intent(a) {
        Some(Operation::Attach) => {
            let matching_mount = matches!(export, PathView::Mounted { fs_type, source, root, device, inode }
                if fs_type == "fuse.bindfs" && source == "/project" && root == "/"
                    && Some(*device) == a.mount_device && Some(*inode) == a.mount_inode);
            if vm.markers == Presence::Exact
                && vm.device == Presence::Exact
                && vm.local_stateful.as_deref() == Some("false")
                && matching_mount
                && export == mapped
            {
                Decision {
                    target_state: Some("attached"),
                    reason: "attach-effects-complete",
                    next_action: "Finalize only the receipt, then use the normal fleet lifecycle and incus-verify for guest readiness. No attach command will be replayed.",
                }
            } else {
                blocked(
                    "partial-or-changed-attachment",
                    "The complete original device, markers, stateful override and matching mounts were not proved. Preserve resources for inspection; do not retry attach or replace a missing mount.",
                )
            }
        }
        Some(Operation::Detach) => {
            if vm.markers == Presence::Absent
                && vm.device == Presence::Absent
                && vm.local_stateful == a.prior_stateful
                && *export == PathView::Absent
                && *mapped == PathView::Absent
            {
                Decision {
                    target_state: Some("detached"),
                    reason: "detach-effects-complete",
                    next_action: "Finalize only the receipt. Private checkout writes remain in the original running sandbox; checkpoint explicitly before closing it.",
                }
            } else {
                blocked(
                    "partial-or-changed-detachment",
                    "Detachment has not proved removal of both mounts, directories, device and markers plus restoration of the prior local stateful setting. Preserve the remaining resources; no cleanup is replayed.",
                )
            }
        }
        None => blocked(
            "legacy-operation-unknown",
            "This legacy uncertain receipt does not identify attach versus detach. Preserve it for manual inspection; absence or presence alone cannot prove that a delayed command is finished.",
        ),
    }
}

fn response(
    a: &Attachment,
    observation: &Observation,
    decision: &Decision,
    changed: bool,
) -> Value {
    json!({"ok":decision.target_state.is_some(), "workspace":a.workspace, "attachment":a.id,
        "receipt_state":a.state, "target_state":decision.target_state, "reason":decision.reason,
        "next_action":decision.next_action, "changed":changed,
        "can_apply":decision.target_state.is_some_and(|target| target != a.state),
        "commands_replayed":false, "guest_readiness_verified":false, "observation":observation})
}

fn reconcile_with(
    root: &Path,
    name: &str,
    expected_container: &str,
    apply: bool,
    mut inspect: impl FnMut(&Attachment) -> Observation,
) -> Result<Value> {
    let mut a = load(root, name)?;
    ensure!(
        a.container == expected_container,
        "Workspace and attachment container identities differ"
    );
    let observed = inspect(&a);
    let decision = decide(&a, &observed);
    let Some(target) = decision.target_state else {
        return Ok(response(&a, &observed, &decision, false));
    };
    if !apply || a.state == target {
        return Ok(response(&a, &observed, &decision, false));
    }
    // A second independent observation prevents a transient or racing device
    // state from being promoted. The host/workspace leases remain held.
    let second = inspect(&a);
    if second != observed || decide(&a, &second) != decision {
        return Ok(response(
            &a,
            &second,
            &blocked(
                "observation-changed",
                "Resource observations changed during reconciliation. Keep custody and inspect again after the owner operation settles.",
            ),
            false,
        ));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let audit = root
        .join("attachments/reconciliations")
        .join(format!("{}-{id}.json", a.workspace));
    // This is an intent record. Only the receipt's last_reconciliation field
    // establishes that finalization committed; a crash between writes is safe.
    state::write_json(
        &audit,
        &json!({"schema":1,"id":id,"intent":"finalize-completed-external-effects",
        "at_unix_seconds":SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        "previous_receipt":a,"target_state":target,"observation":observed,"commands_replayed":false}),
    )?;
    a.schema = ATTACHMENT_SCHEMA;
    a.last_operation = intent(&a);
    a.last_reconciliation = Some(id);
    a.state = target.into();
    a.last_error = None;
    save(root, &a)?;
    let mut value = response(&a, &observed, &decision, true);
    value["reconciliation"] = json!(a.last_reconciliation);
    value["checkpoint_required"] = json!(target == "detached");
    Ok(value)
}

/// Explicit node operation. The caller holds the workspace lease and checks its
/// original config binding. Even --apply only finalizes private receipts.
pub fn reconcile(
    root: &Path,
    name: &str,
    expected_container: &str,
    docker_socket: &Path,
    apply: bool,
) -> Result<Value> {
    ensure!(
        cfg!(target_os = "linux"),
        "Incus reconciliation requires Linux"
    );
    let _host = state::root_host_lock()?;
    real_path(root)?;
    reconcile_with(root, name, expected_container, apply, |a| {
        observe(a, docker_socket)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt_fixture(root: &Path, operation: Operation) -> Attachment {
        Attachment {
            schema: 3,
            workspace: "checkout".into(),
            id: uuid::Uuid::new_v4().to_string(),
            target: Target {
                project: "enginebox".into(),
                instance: "ebox-test".into(),
                owner: "fixture".into(),
                uid: 1001,
                gid: 1001,
            },
            instance_uuid: uuid::Uuid::new_v4().to_string(),
            prior_stateful: None,
            container: "a".repeat(64),
            container_pid: 1234,
            host_boot_id: "original-boot".into(),
            container_started_at: "original-start".into(),
            export: root.join("exports/checkout"),
            mount_device: Some(23),
            mount_inode: Some(1),
            state: match operation {
                Operation::Attach => "attaching",
                Operation::Detach => "detaching",
            }
            .into(),
            last_error: None,
            last_operation: Some(operation),
            last_reconciliation: None,
        }
    }
    fn check<T>(value: T) -> Check<T> {
        Check::from_result(Ok(value))
    }
    fn complete(a: &Attachment, operation: Operation) -> Observation {
        let attached = operation == Operation::Attach;
        let path = if attached {
            PathView::Mounted {
                fs_type: "fuse.bindfs".into(),
                source: "/project".into(),
                root: "/".into(),
                device: 23,
                inode: 1,
            }
        } else {
            PathView::Absent
        };
        Observation {
            boot_id: check(a.host_boot_id.clone()),
            vm: check(Vm {
                uuid: a.instance_uuid.clone(),
                stopped: true,
                markers: if attached {
                    Presence::Exact
                } else {
                    Presence::Absent
                },
                device: if attached {
                    Presence::Exact
                } else {
                    Presence::Absent
                },
                local_stateful: if attached {
                    Some("false".into())
                } else {
                    a.prior_stateful.clone()
                },
            }),
            container: check(Container {
                id: a.container.clone(),
                running: true,
                restarting: false,
                dead: false,
                pid: a.container_pid,
                started_at: a.container_started_at.clone(),
            }),
            export: check(path.clone()),
            mapped: check(path),
        }
    }
    fn vm_json(a: &Attachment) -> Value {
        let config = json!({"volatile.uuid":a.instance_uuid,"user.enginebox.owner":a.target.owner,
            MARKER:a.id, WORKSPACE_MARKER:a.workspace, STATEFUL:"false"});
        let devices = json!({DEVICE:expected_device(a)});
        json!({"name":a.target.instance,"type":"virtual-machine","status":"Stopped","stateful":false,
            "config":config,"expanded_config":config,"devices":devices,"expanded_devices":devices})
    }

    #[test]
    fn lost_final_attach_receipt_is_recovered_without_replaying_or_touching_private_writes()
    -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut a = receipt_fixture(dir.path(), Operation::Attach);
        a.state = "uncertain".into();
        a.last_error = Some("operator transport was interrupted".into());
        save(dir.path(), &a)?;
        let original = fs::read(receipt(dir.path(), "checkout"))?;
        let private = dir.path().join("private-uncheckpointed-file");
        fs::write(&private, b"private edits remain")?;
        let observation = complete(&a, Operation::Attach);
        let preview = reconcile_with(dir.path(), "checkout", &a.container, false, |_| {
            observation.clone()
        })?;
        assert_eq!(preview["can_apply"], true);
        assert_eq!(preview["changed"], false);
        assert_eq!(fs::read(receipt(dir.path(), "checkout"))?, original);
        let mut calls = 0;
        let applied = reconcile_with(dir.path(), "checkout", &a.container, true, |_| {
            calls += 1;
            observation.clone()
        })?;
        assert_eq!(calls, 2);
        assert_eq!(applied["changed"], true);
        assert_eq!(applied["commands_replayed"], false);
        assert_eq!(applied["guest_readiness_verified"], false);
        let restored = load(dir.path(), "checkout")?;
        assert_eq!(restored.state, "attached");
        assert!(ensure_detached(dir.path(), "checkout").is_err());
        let audit = state::read_json(&dir.path().join("attachments/reconciliations").join(
            format!("checkout-{}.json", restored.last_reconciliation.unwrap()),
        ))?;
        assert_eq!(
            audit["previous_receipt"]["last_error"],
            a.last_error.unwrap()
        );
        assert_eq!(fs::read(private)?, b"private edits remain");
        Ok(())
    }

    #[test]
    fn completed_detach_requires_exact_prior_local_configuration_and_releases_only_metadata_custody()
    -> Result<()> {
        for prior in [None, Some("true".into()), Some("false".into())] {
            let dir = tempfile::tempdir()?;
            let mut a = receipt_fixture(dir.path(), Operation::Detach);
            a.prior_stateful = prior;
            save(dir.path(), &a)?;
            let observation = complete(&a, Operation::Detach);
            let applied = reconcile_with(dir.path(), "checkout", &a.container, true, |_| {
                observation.clone()
            })?;
            assert_eq!(applied["checkpoint_required"], true);
            assert_eq!(applied["target_state"], "detached");
            ensure_detached(dir.path(), "checkout")?;
            let mut wrong = observation;
            wrong.vm.value.as_mut().unwrap().local_stateful = Some("unexpected".into());
            assert!(decide(&a, &wrong).target_state.is_none());
        }
        Ok(())
    }

    #[test]
    fn every_partial_attach_and_detach_keeps_custody() -> Result<()> {
        let dir = tempfile::tempdir()?;
        for operation in [Operation::Attach, Operation::Detach] {
            let a = receipt_fixture(dir.path(), operation);
            let good = complete(&a, operation);
            let mut partials = vec![];
            for presence in [
                Presence::Other,
                if operation == Operation::Attach {
                    Presence::Absent
                } else {
                    Presence::Exact
                },
            ] {
                let mut value = good.clone();
                value.vm.value.as_mut().unwrap().markers = presence;
                partials.push(value);
                let mut value = good.clone();
                value.vm.value.as_mut().unwrap().device = presence;
                partials.push(value);
            }
            let mut value = good.clone();
            value.export = check(PathView::UnmountedDirectory);
            partials.push(value);
            let mut value = good.clone();
            value.mapped = check(PathView::UnmountedDirectory);
            partials.push(value);
            let mut value = good.clone();
            value.export = Check::unavailable("mount inspection failed");
            partials.push(value);
            let mut value = good.clone();
            value.vm.value.as_mut().unwrap().stopped = false;
            partials.push(value);
            save(dir.path(), &a)?;
            let before = fs::read(receipt(dir.path(), "checkout"))?;
            for observation in partials {
                let result = reconcile_with(dir.path(), "checkout", &a.container, true, |_| {
                    observation.clone()
                })?;
                assert_eq!(result["ok"], false);
                assert_eq!(result["changed"], false);
                assert_eq!(fs::read(receipt(dir.path(), "checkout"))?, before);
                assert!(ensure_detached(dir.path(), "checkout").is_err());
            }
        }
        Ok(())
    }

    #[test]
    fn host_restart_and_reused_container_pid_never_rebind_the_export() {
        let a = receipt_fixture(Path::new("/private/fixture"), Operation::Attach);
        let mut value = complete(&a, Operation::Attach);
        value.boot_id = check("new-boot".into());
        // Even apparently identical devices, inode numbers and PIDs from a
        // new boot cannot authenticate the previous mount or cached writes.
        assert_eq!(decide(&a, &value).reason, "host-restarted");
        for change in 0..5 {
            let mut value = complete(&a, Operation::Attach);
            let container = value.container.value.as_mut().unwrap();
            match change {
                0 => container.started_at = "new-start".into(),
                1 => container.id = "b".repeat(64),
                2 => container.running = false,
                3 => container.restarting = true,
                _ => container.pid += 1,
            }
            assert_eq!(decide(&a, &value).reason, "container-unverified");
        }
    }

    #[test]
    fn legacy_unknown_intent_is_not_guessed_and_old_known_phases_can_finalize() {
        let mut a = receipt_fixture(Path::new("/private/fixture"), Operation::Attach);
        a.schema = 2;
        a.last_operation = None;
        let observation = complete(&a, Operation::Attach);
        assert_eq!(decide(&a, &observation).target_state, Some("attached"));
        a.state = "uncertain".into();
        assert_eq!(decide(&a, &observation).reason, "legacy-operation-unknown");
        a.state = "detaching".into();
        assert_eq!(
            decide(&a, &complete(&a, Operation::Detach)).target_state,
            Some("detached")
        );
    }

    #[test]
    fn changing_observation_and_wrong_workspace_identity_do_not_finalize() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let a = receipt_fixture(dir.path(), Operation::Attach);
        save(dir.path(), &a)?;
        let before = fs::read(receipt(dir.path(), "checkout"))?;
        let mut calls = 0;
        let result = reconcile_with(dir.path(), "checkout", &a.container, true, |_| {
            calls += 1;
            let mut value = complete(&a, Operation::Attach);
            if calls == 2 {
                value.vm.value.as_mut().unwrap().device = Presence::Absent;
            }
            value
        })?;
        assert_eq!(result["reason"], "observation-changed");
        assert_eq!(fs::read(receipt(dir.path(), "checkout"))?, before);
        assert!(!dir.path().join("attachments/reconciliations").exists());
        assert!(
            reconcile_with(dir.path(), "checkout", &"b".repeat(64), true, |_| panic!(
                "must not inspect another container"
            ))
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn receipt_with_conflicting_operation_cannot_release_custody() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut a = receipt_fixture(dir.path(), Operation::Attach);
        a.last_operation = Some(Operation::Detach);
        save(dir.path(), &a)?;
        assert!(
            reconcile_with(dir.path(), "checkout", &a.container, true, |_| panic!(
                "invalid intent must fail before inspection"
            ))
            .is_err()
        );
        assert!(ensure_detached(dir.path(), "checkout").is_err());
        a.last_operation = None;
        save(dir.path(), &a)?;
        assert!(load(dir.path(), "checkout").is_err());
        Ok(())
    }

    #[test]
    fn authoritative_vm_view_refuses_replacements_inherited_devices_and_partial_markers()
    -> Result<()> {
        let a = receipt_fixture(Path::new("/private/fixture"), Operation::Attach);
        let value = vm_json(&a);
        assert_eq!(vm_view(&a, &value)?.device, Presence::Exact);
        for key in ["volatile.uuid", "user.enginebox.owner"] {
            let mut value = value.clone();
            value["expanded_config"][key] = json!(uuid::Uuid::new_v4().to_string());
            assert!(vm_view(&a, &value).is_err());
        }
        let mut changed = value.clone();
        changed["devices"].as_object_mut().unwrap().remove(DEVICE);
        assert_eq!(vm_view(&a, &changed)?.device, Presence::Other);
        let mut changed = value.clone();
        changed["expanded_devices"]["other"] =
            json!({"type":"disk","path":"/project","source":"/unrelated"});
        assert_eq!(vm_view(&a, &changed)?.device, Presence::Other);
        let mut changed = value;
        changed["config"]
            .as_object_mut()
            .unwrap()
            .remove(WORKSPACE_MARKER);
        assert_eq!(vm_view(&a, &changed)?.markers, Presence::Other);
        Ok(())
    }

    #[test]
    fn mountinfo_uses_exact_decoded_targets_and_refuses_stacked_mounts() -> Result<()> {
        let record = "42 1 0:23 / /private/export\\040one rw - fuse.bindfs /project rw\n";
        assert_eq!(
            mount_at(record, "/private/export one")?,
            Some(("fuse.bindfs".into(), "/project".into(), "/".into()))
        );
        assert_eq!(mount_at(record, "/private/export")?, None);
        assert!(mount_at(&format!("{record}{record}"), "/private/export one").is_err());
        assert!(mount_at("bad record", "/private/export").is_err());
        assert!(unescape_mount_field("/bad\\999").is_err());
        Ok(())
    }

    #[test]
    fn empty_directory_and_foreign_bindfs_inode_cannot_prove_a_live_export() -> Result<()> {
        let dir = tempfile::tempdir()?;
        assert_eq!(
            path_view("", "/export", dir.path())?,
            PathView::UnmountedDirectory
        );
        assert_eq!(
            path_view("", "/export", &dir.path().join("absent"))?,
            PathView::Absent
        );
        let a = receipt_fixture(dir.path(), Operation::Attach);
        let mut observation = complete(&a, Operation::Attach);
        if let Some(PathView::Mounted { inode, .. }) = &mut observation.export.value {
            *inode += 1;
        }
        observation.mapped = observation.export.clone();
        assert!(decide(&a, &observation).target_state.is_none());
        Ok(())
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_mount_probe_reads_actual_kernel_table_and_directory_identity() -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let table = fs::read_to_string("/proc/self/mountinfo")?;
        let metadata = fs::metadata("/")?;
        let PathView::Mounted { device, inode, .. } = path_view(&table, "/", Path::new("/"))?
        else {
            anyhow::bail!("Linux root mount was not observed");
        };
        assert_eq!((device, inode), (metadata.dev(), metadata.ino()));
        Ok(())
    }
}
