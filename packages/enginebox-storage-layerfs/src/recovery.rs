//! Recovery of operator metadata is separate from recovery of committed data.
//! No original command is retried; dead-daemon recovery always allocates a new
//! sandbox and keeps the old container and overlay for forensic inspection.
use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

// The probe has no dependencies beyond the pinned runtime's Node executable.
// Read kernel-owned identity instead of trusting files in the agent checkout.
const PROBE: &str = r#"
const fs = require('fs');
const expected = process.argv[1];
const processes = [];
for (const entry of fs.readdirSync('/proc')) {
  if (!/^\d+$/.test(entry) || Number(entry) === process.pid) continue;
  try {
    const status = fs.readFileSync(`/proc/${entry}/status`, 'utf8');
    const uid = Number(status.match(/^Uid:\s+(\d+)/m)?.[1]);
    const name = status.match(/^Name:\s+(.*)$/m)?.[1];
    processes.push({pid:Number(entry), uid, name});
  } catch (e) { if (e.code !== 'ENOENT' && e.code !== 'ESRCH') throw e; }
}
const mounts = fs.readFileSync('/proc/self/mountinfo','utf8').trim().split('\n').map(line => {
  const [left, right] = line.split(' - '); const f = left.split(' ');
  return {device:f[2], root:f[3], target:f[4], type:right.split(' ')[0]};
}).filter(m => m.target === '/project' || m.target === expected);
console.log(JSON.stringify({processes,mounts}));
"#;

fn eligible(w: &Workspace) -> Result<()> {
    let pending = match w.status.as_str() {
        "ready" => return Ok(()),
        "exec" | "deps" => w.status.as_str(),
        "uncertain" => w.last_operation.as_deref().unwrap_or("unknown"),
        _ => "unknown",
    };
    ensure!(
        ["exec", "deps"].contains(&pending),
        "Recovery only reconciles a ready workspace or an interrupted command; partial create/checkpoint/remount/close needs inspection"
    );
    Ok(())
}

fn validate_probe(probe: &Value, path: &str) -> Result<()> {
    let processes = probe["processes"]
        .as_array()
        .context("Missing process census")?;
    ensure!(
        processes.len() == 2
            && processes.iter().all(|p| p["uid"] == 0)
            && processes
                .iter()
                .any(|p| p["pid"] == 1 && p["name"] == "docker-init")
            && processes
                .iter()
                .any(|p| p["pid"] != 1 && p["name"] == "layerfs-daemon"),
        "Sandbox still has commands or unexpected processes; recovery never kills or replays them"
    );
    let mounts = probe["mounts"].as_array().context("Missing mount census")?;
    let original = mounts
        .iter()
        .find(|m| m["target"] == path)
        .context("Original FUSE mount is absent")?;
    let alias = mounts
        .iter()
        .find(|m| m["target"] == "/project")
        .context("Stable /project mount is absent")?;
    ensure!(
        mounts.len() == 2
            && original["type"]
                .as_str()
                .is_some_and(|s| s == "fuse" || s.starts_with("fuse."))
            && original["type"] == alias["type"]
            && original["device"] == alias["device"]
            && original["root"] == alias["root"],
        "Stable /project is not the recorded LayerFS mount"
    );
    Ok(())
}

fn validate_status(status: &Value, branch: &str) -> Result<()> {
    ensure!(
        status["kind"] == "ok"
            && status["branch"] == branch
            && status["activity"] == "Idle"
            && status["headMovedSinceMount"] == false
            && status["overlay"]["inodes"].is_u64()
            && status["overlay"]["entries"].is_u64()
            && status["overlay"]["pendingMerge"] == false,
        "Daemon did not prove an idle, bound workspace without an unresolved merge"
    );
    Ok(())
}

fn checkpoint_head(w: &Workspace) -> Result<Value> {
    let checkpoint = w
        .last_checkpoint
        .as_ref()
        .context("No completed checkpoint recorded")?;
    ensure!(
        ["Committed", "UpToDate"].contains(&checkpoint["kind"].as_str().unwrap_or("")),
        "Last checkpoint was not completed"
    );
    let head = checkpoint.get("head").context("Checkpoint head missing")?;
    ensure!(
        // Pinned LayerFS identities are typed: CommitId is tag 0x12 followed
        // by 32 bytes, unlike a plain SHA-256 artifact digest.
        head.is_null()
            || head
                .as_str()
                .and_then(|s| s.strip_prefix("12"))
                .is_some_and(crate::config::digest),
        "Invalid checkpoint identity"
    );
    Ok(head.clone())
}

fn audit(w: &Workspace, kind: &str) -> Value {
    json!({"kind":kind,"at_unix_seconds":SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
        "previous_status":w.status,"previous_operation":w.last_operation,"previous_error":w.last_error,
        "previous_sandbox":w.sandbox,"previous_container":w.container,"previous_mount":w.mount,
        "previous_branch":w.branch,"command_outcome":"unknown; not replayed"})
}

impl Runtime {
    fn settled_call(&self, p: &Project, w: &Workspace) -> Result<Value> {
        let call = w.pending_call.as_deref().context("No exact helper receipt; an absent helper does not prove a delayed launch cannot still start")?;
        identifier(call)?;
        let path = self.project_dir(&p.name).join("calls").join(call);
        for suffix in ["json", "started", "finished", "result"] {
            real_path(&path.with_extension(suffix))?;
        }
        let intent = state::read_json(&path.with_extension("json"))?;
        ensure!(
            intent
                == json!({"schema":1,"call":call,"owner":p.owner,"project":p.name,"sandbox":w.sandbox,"operation":"exec"}),
            "Helper receipt identity differs from the interrupted workspace"
        );
        ensure!(
            fs::read_to_string(path.with_extension("finished"))?.trim() == "0",
            "Original helper did not finish normally; no command replay or guessed outcome"
        );
        let response: Value = serde_json::from_slice(&fs::read(path.with_extension("result"))?)?;
        ensure!(
            response["ok"] == true && response["result"]["exitCode"].is_i64(),
            "Original helper has no completed command outcome; preserve it for inspection"
        );
        Ok(
            json!({"call":call,"exit_code":response["result"]["exitCode"],"outcome":"original returned; not replayed"}),
        )
    }
    pub(super) fn no_helpers(&self, p: &Project) -> Result<()> {
        let bytes = self.docker(
            &[
                "ps".into(),
                "--all".into(),
                "--filter".into(),
                format!("label=enginebox.layerfs.owner={}", p.owner),
                "--format={{.ID}}".into(),
            ],
            None,
            30,
        )?;
        ensure!(
            bytes.iter().all(|b| b.is_ascii_whitespace()),
            "Project still has an SDK helper; wait for the original bounded operation, never replay it"
        );
        Ok(())
    }
    pub(super) fn probe(&self, w: &Workspace) -> Result<Value> {
        let id = w.container.as_deref().context("Container missing")?;
        let path = w.mount.as_deref().context("Mount missing")?;
        let probe: Value = serde_json::from_slice(&self.docker(
            &[
                "exec".into(),
                "--user=0".into(),
                id.into(),
                "node".into(),
                "-e".into(),
                PROBE.into(),
                path.into(),
            ],
            None,
            15,
        )?)?;
        validate_probe(&probe, path)?;
        Ok(probe)
    }
    /// Reconcile an interrupted CLI once its original bounded process is gone.
    /// Live edits stay mounted; the helper's saved exit is reported without replay.
    pub fn recover(&self, name: &str) -> Result<Value> {
        identifier(name)?;
        self.root()?;
        self.verify_binding()?;
        let _lease = state::lock(
            &self
                .config
                .root
                .join("workspaces")
                .join(format!("{name}.lock")),
            false,
        )?;
        let mut w = self.workspace(name)?;
        crate::incus::ensure_detached(&self.config.root, name)?;
        eligible(&w)?;
        let p = self.project(&w.project)?;
        ensure!(p.status == "ready", "Project is not ready");
        self.owned_volume(&p)?;
        let id = w.container.as_deref().context("Container missing")?;
        self.verify_container(&p, &w.sandbox, id)?;
        let inspection = self.d(&["inspect", id])?;
        ensure!(
            inspection[0]["State"]["Running"] == true
                && inspection[0]["State"]["Paused"] == false
                && inspection[0]["State"]["Restarting"] == false,
            "Original sandbox is not running; restore a completed checkpoint into a new sandbox"
        );
        self.no_helpers(&p)?;
        let settled = if w.status == "ready" {
            None
        } else {
            Some(self.settled_call(&p, &w)?)
        };
        self.probe(&w)?;
        let status = self.bridge(
            &p,
            "status",
            json!({"sandbox":w.sandbox,"mount":w.mount}),
            30,
        )?;
        validate_status(&status, &w.branch)?;
        self.no_helpers(&p)?;
        let proof = self.probe(&w)?;
        if w.status == "ready" {
            return Ok(
                json!({"ok":true,"workspace":name,"status":"ready","changed":false,"observed":status}),
            );
        }
        let mut record = audit(&w, "operator-reconciled");
        record["command_outcome"] = json!("original returned; not replayed");
        record["settled_call"] = json!(settled);
        record["observed"] = status.clone();
        record["proof"] = proof;
        w.recoveries.push(record.clone());
        w.status = "ready".into();
        w.last_operation = Some("recover".into());
        w.last_error = None;
        self.save_workspace(&w)?;
        Ok(
            json!({"ok":true,"workspace":name,"status":"ready","changed":true,"recovery":record,"observed":status}),
        )
    }

    /// Restore only a completed file checkpoint after the old sandbox stopped.
    /// The old container is retained; neither its overlay nor receipt is deleted.
    pub fn restore_checkpoint(&self, name: &str) -> Result<Value> {
        identifier(name)?;
        self.root()?;
        self.verify_binding()?;
        let _global = state::lock(&self.config.root.join(".lock"), false)?;
        let _lease = state::lock(
            &self
                .config
                .root
                .join("workspaces")
                .join(format!("{name}.lock")),
            false,
        )?;
        let mut w = self.workspace(name)?;
        crate::incus::ensure_detached(&self.config.root, name)?;
        eligible(&w)?;
        let expected = checkpoint_head(&w)?;
        let p = self.project(&w.project)?;
        ensure!(p.status == "ready", "Project is not ready");
        self.owned_volume(&p)?;
        let old = w
            .container
            .as_deref()
            .context("Original container missing; preserve its evidence before recovery")?;
        self.verify_container(&p, &w.sandbox, old)?;
        let inspection = self.d(&["inspect", old])?;
        ensure!(
            inspection[0]["State"]["Running"] == false
                && inspection[0]["State"]["Restarting"] == false
                && inspection[0]["State"]["Dead"] == false
                && inspection[0]["State"]["Pid"] == 0,
            "Original sandbox must already be stopped; checkpoint restoration never stops live agents"
        );
        self.no_helpers(&p)?;
        self.space(0)?;
        let mut record = audit(&w, "checkpoint-restoration");
        record["checkpoint_head"] = expected.clone();
        record["old_container_retained"] = json!(true);
        record["uncheckpointed_edits_recovered"] = json!(false);
        let source = w.branch.clone();
        let key = format!("w-{}", uuid::Uuid::new_v4().simple());
        w.sandbox = key.clone();
        w.branch = key;
        w.container = None;
        w.mount = None;
        w.status = "restoring".into();
        w.last_operation = Some("restore-checkpoint".into());
        w.recoveries.push(record.clone());
        self.save_workspace(&w)?;
        let outcome = (|| {
            self.bridge(&p, "fork", json!({"branch":w.branch,"from":source}), 60)?;
            let cache = self
                .config
                .pnpm_store
                .as_ref()
                .map(|store| json!({"store":store,"metadata":self.config.pnpm_metadata}));
            self.bridge(&p, "create", json!({"sandbox":w.sandbox,"cache":cache}), 90)?;
            w.container = Some(self.container(&p, &w.sandbox)?);
            self.save_workspace(&w)?;
            self.limits(w.container.as_deref().unwrap())?;
            w.mount = Some(
                self.bridge(
                    &p,
                    "mount",
                    json!({"sandbox":w.sandbox,"branch":w.branch}),
                    60,
                )?
                .as_str()
                .context("Mount returned no path")?
                .into(),
            );
            self.save_workspace(&w)?;
            let status = self.bridge(
                &p,
                "status",
                json!({"sandbox":w.sandbox,"mount":w.mount}),
                30,
            )?;
            validate_status(&status, &w.branch)?;
            ensure!(
                status.get("branchHead") == Some(&expected)
                    && status.get("base") == Some(&expected),
                "Stored branch differs from the last completed checkpoint; replacement stays blocked for inspection"
            );
            self.alias(&w, true)?;
            self.probe(&w)?;
            Ok(
                json!({"ok":true,"workspace":name,"status":"ready","path":"/project","recovery":record,"observed":status}),
            )
        })();
        self.finish(&mut w, outcome, "ready")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn workspace(status: &str, operation: Option<&str>) -> Workspace {
        Workspace {
            schema: 1,
            name: "a".into(),
            project: "p".into(),
            sandbox: "w-a".into(),
            branch: "w-a".into(),
            container: None,
            mount: None,
            status: status.into(),
            last_error: None,
            last_checkpoint: None,
            last_operation: operation.map(str::to_owned),
            recoveries: vec![],
            pending_call: None,
        }
    }
    #[test]
    fn partial_storage_mutations_and_legacy_unknown_outcomes_cannot_be_replayed() {
        for action in [
            "creating",
            "checkpoint",
            "remount",
            "close",
            "restoring",
            "closed",
        ] {
            assert!(eligible(&workspace(action, Some(action))).is_err());
            assert!(eligible(&workspace("uncertain", Some(action))).is_err());
        }
        assert!(eligible(&workspace("uncertain", None)).is_err());
        for action in ["exec", "deps"] {
            assert!(eligible(&workspace(action, None)).is_ok());
            assert!(eligible(&workspace("uncertain", Some(action))).is_ok());
        }
    }
    #[test]
    fn docker_api_range_must_include_the_pinned_sdk_protocol() {
        assert!(require_docker_api(&json!({"ApiVersion":"1.52","MinAPIVersion":"1.44"})).is_err());
        assert!(require_docker_api(&json!({"ApiVersion":"1.55","MinAPIVersion":"1.55"})).is_err());
        assert!(require_docker_api(&json!({"ApiVersion":"1.54","MinAPIVersion":"1.44"})).is_ok());
        assert!(require_docker_api(&json!({"ApiVersion":"1.100","MinAPIVersion":"1.54"})).is_ok());
        assert!(
            require_docker_api(&json!({"ApiVersion":"unknown","MinAPIVersion":"1.44"})).is_err()
        );
    }
    #[test]
    fn recovery_requires_kernel_mount_identity_and_no_other_processes() {
        let valid = json!({"processes":[{"uid":0,"pid":1,"name":"docker-init"},{"uid":0,"pid":8,"name":"layerfs-daemon"}],"mounts":[{"target":"/project","type":"fuse.layerfs","device":"0:99","root":"/"},{"target":"/workspaces/1","type":"fuse.layerfs","device":"0:99","root":"/"}]});
        assert!(validate_probe(&valid, "/workspaces/1").is_ok());
        let mut busy = valid.clone();
        busy["processes"]
            .as_array_mut()
            .unwrap()
            .push(json!({"uid":501,"pid":11,"name":"sleep"}));
        assert!(validate_probe(&busy, "/workspaces/1").is_err());
        let mut foreign = valid.clone();
        foreign["mounts"][0]["device"] = json!("0:100");
        assert!(validate_probe(&foreign, "/workspaces/1").is_err());
        let mut unexpected = valid;
        unexpected["processes"][1]["name"] = json!("sh");
        assert!(validate_probe(&unexpected, "/workspaces/1").is_err());
    }
    #[test]
    fn uncertain_daemon_or_moved_branch_cannot_be_called_recovered() {
        let valid = json!({"kind":"ok","branch":"w-a","activity":"Idle","headMovedSinceMount":false,"overlay":{"inodes":1,"entries":2,"pendingMerge":false}});
        assert!(validate_status(&valid, "w-a").is_ok());
        assert!(validate_status(&valid, "w-b").is_err());
        for state in [
            "Committing",
            "Uncertain",
            "LocalFailure",
            "Closing",
            "Attaching",
        ] {
            let mut status = valid.clone();
            status["activity"] = json!(state);
            assert!(validate_status(&status, "w-a").is_err());
        }
        let mut moved = valid;
        moved["headMovedSinceMount"] = json!(true);
        assert!(validate_status(&moved, "w-a").is_err());
    }
    #[test]
    fn completed_checkpoint_identity_is_required() {
        let mut w = workspace("ready", None);
        assert!(checkpoint_head(&w).is_err());
        w.last_checkpoint = Some(json!({"kind":"Committed","head":format!("12{}","a".repeat(64))}));
        assert!(checkpoint_head(&w).is_ok());
        for invalid in ["a".repeat(64), format!("32{}", "a".repeat(64))] {
            w.last_checkpoint = Some(json!({"kind":"Committed","head":invalid}));
            assert!(checkpoint_head(&w).is_err());
        }
        w.last_checkpoint = Some(json!({"kind":"UpToDate","head":null}));
        assert!(checkpoint_head(&w).is_ok());
        w.last_checkpoint = Some(json!({"kind":"Committed"}));
        assert!(checkpoint_head(&w).is_err());
    }
    #[test]
    fn a_missing_helper_does_not_prove_a_delayed_launch_has_finished() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        let runtime = Runtime {
            config: Config {
                schema: 1,
                experimental: true,
                docker_socket: "/var/run/docker.sock".into(),
                root: root.clone(),
                sdk: root.join("sdk"),
                daemon: root.join("daemon"),
                native_sha256: "a".repeat(64),
                daemon_sha256: "b".repeat(64),
                image: format!("sha256:{}", "c".repeat(64)),
                max_workspaces: 2,
                cpus_per_workspace: 1,
                memory_mib_per_workspace: 1024,
                disk_reserve_gib: 1,
                import_limit_gib: 1,
                pnpm_store: None,
                pnpm_metadata: None,
            },
        };
        let p = Project {
            schema: 1,
            name: "p".into(),
            owner: "d".repeat(32),
            volume: format!("ebox-lfs-{}", "d".repeat(32)),
            status: "ready".into(),
            commit: "e".repeat(40),
            approval: "f".repeat(64),
            source_only: false,
        };
        let mut w = workspace("exec", Some("exec"));
        w.pending_call = Some("ebox-lfs-call-fixture".into());
        assert!(runtime.settled_call(&p, &w).is_err());
        let calls = root.join("projects/p/calls");
        state::private_dir(&calls)?;
        let path = calls.join("ebox-lfs-call-fixture");
        state::write_json(
            &path.with_extension("json"),
            &json!({"schema":1,"call":w.pending_call,"owner":p.owner,"project":"p","sandbox":"w-a","operation":"exec"}),
        )?;
        fs::write(path.with_extension("started"), "")?;
        fs::write(
            path.with_extension("result"),
            br#"{"ok":true,"result":{"exitCode":23}}"#,
        )?;
        assert!(runtime.settled_call(&p, &w).is_err());
        fs::write(path.with_extension("finished"), "124\n")?;
        assert!(runtime.settled_call(&p, &w).is_err());
        fs::write(path.with_extension("finished"), "0\n")?;
        assert_eq!(runtime.settled_call(&p, &w)?["exit_code"], 23);
        fs::write(
            path.with_extension("result"),
            br#"{"ok":false,"error":"outcome unknown"}"#,
        )?;
        assert!(runtime.settled_call(&p, &w).is_err());
        Ok(())
    }
}
