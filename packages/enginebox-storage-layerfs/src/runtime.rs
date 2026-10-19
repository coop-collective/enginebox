use crate::{
    config::{Config, identifier, real_path},
    source,
};
use anyhow::{Context, Result, ensure};
use enginebox_core::{
    state,
    transport::{run_process, shell_quote},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, time::Duration};

const BRIDGE: &str = include_str!("bridge.mjs");
const SOCKET: &str = "unix:///var/run/docker.sock";
#[derive(Clone, Serialize, Deserialize)]
struct Project {
    schema: u32,
    name: String,
    owner: String,
    volume: String,
    status: String,
    commit: String,
    approval: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct Workspace {
    schema: u32,
    name: String,
    project: String,
    sandbox: String,
    branch: String,
    container: Option<String>,
    mount: Option<String>,
    status: String,
    last_error: Option<String>,
    #[serde(default)]
    last_checkpoint: Option<Value>,
}
pub struct Runtime {
    pub config: Config,
}
fn docker(args: &[String], input: Option<&[u8]>, seconds: u64) -> Result<Vec<u8>> {
    let mut command = vec!["docker".into(), "--host".into(), SOCKET.into()];
    command.extend_from_slice(args);
    run_process(&command, input, Duration::from_secs(seconds))
}
fn d(args: &[&str]) -> Result<Value> {
    Ok(serde_json::from_slice(&docker(
        &args.iter().map(|v| (*v).into()).collect::<Vec<_>>(),
        None,
        30,
    )?)?)
}
impl Runtime {
    pub fn new(config: Config) -> Result<Self> {
        ensure!(
            cfg!(target_os = "linux"),
            "LayerFS execution requires a local Linux Docker node; preview works on Unix operator machines"
        );
        ensure!(
            run_process(&["id".into(), "-u".into()], None, Duration::from_secs(5))? == b"0\n",
            "Run this optional node-local backend as root; SDK metadata is written by the trusted root Docker helper"
        );
        config.verify_artifacts()?;
        Ok(Self { config })
    }
    fn root(&self) -> Result<()> {
        if self.config.root.exists() && !self.config.root.join("config.json").exists() {
            ensure!(
                fs::read_dir(&self.config.root)?.next().is_none(),
                "Refusing to adopt a nonempty directory without an Engine Box storage binding"
            );
        }
        state::private_dir(&self.config.root)?;
        real_path(&self.config.root)?;
        for child in ["projects", "workspaces"] {
            state::private_dir(&self.config.root.join(child))?;
        }
        Ok(())
    }
    fn bind_config(&self) -> Result<()> {
        let path = self.config.root.join("config.json");
        let value = json!({"schema":1,"revision":crate::REVISION,"config":self.config});
        if path.exists() {
            ensure!(
                state::read_json(&path)? == value,
                "Configuration differs from this store root's binding; use its original configuration"
            );
        } else {
            state::write_json(&path, &value)?;
        }
        let bridge = self.config.root.join("bridge.mjs");
        if bridge.exists() {
            real_path(&bridge)?;
            ensure!(
                fs::read_to_string(&bridge)? == BRIDGE,
                "Bridge content mismatch; explicit adapter migration required"
            );
        } else {
            fs::write(&bridge, BRIDGE)?;
        }
        Ok(())
    }
    fn verify_binding(&self) -> Result<()> {
        ensure!(
            state::read_json(&self.config.root.join("config.json"))?
                == json!({"schema":1,"revision":crate::REVISION,"config":self.config}),
            "Configuration differs from this storage root"
        );
        let bridge = self.config.root.join("bridge.mjs");
        real_path(&bridge)?;
        ensure!(
            fs::read_to_string(bridge)? == BRIDGE,
            "Bridge content mismatch; explicit adapter migration required"
        );
        Ok(())
    }
    fn active_count(&self) -> Result<usize> {
        Ok(state::json_files(&self.config.root.join("workspaces"))?
            .into_iter()
            .map(|p| state::read_json(&p))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|v| v["status"] != "closed")
            .count())
    }
    fn project_dir(&self, name: &str) -> PathBuf {
        self.config.root.join("projects").join(name)
    }
    fn project(&self, name: &str) -> Result<Project> {
        identifier(name)?;
        let p: Project = serde_json::from_value(state::read_json(
            &self.project_dir(name).join("project.json"),
        )?)?;
        ensure!(
            p.schema == 1
                && p.name == name
                && p.owner.len() == 32
                && p.volume == format!("ebox-lfs-{}", p.owner),
            "Invalid project receipt"
        );
        Ok(p)
    }
    fn workspace_path(&self, name: &str) -> PathBuf {
        self.config
            .root
            .join("workspaces")
            .join(format!("{name}.json"))
    }
    fn workspace(&self, name: &str) -> Result<Workspace> {
        identifier(name)?;
        let w: Workspace = serde_json::from_value(state::read_json(&self.workspace_path(name))?)?;
        ensure!(w.schema == 1 && w.name == name, "Invalid workspace receipt");
        identifier(&w.sandbox)?;
        identifier(&w.branch)?;
        Ok(w)
    }
    fn save_workspace(&self, w: &Workspace) -> Result<()> {
        state::write_json(&self.workspace_path(&w.name), &serde_json::to_value(w)?)
    }
    fn owned_volume(&self, p: &Project) -> Result<()> {
        let v = d(&["volume", "inspect", &p.volume])?;
        ensure!(
            v[0]["Driver"] == "local"
                && v[0]["Options"].as_object().is_none_or(|o| o.is_empty())
                && v[0]["Labels"]["enginebox.layerfs.owner"] == p.owner
                && v[0]["Labels"]["enginebox.layerfs.revision"] == crate::REVISION,
            "Refusing foreign or nonlocal Store volume"
        );
        Ok(())
    }
    pub fn doctor(&self) -> Result<Value> {
        let info = d(&["info", "--format", "{{json .}}"])?;
        ensure!(
            info["OSType"] == "linux",
            "Local Linux Docker daemon required"
        );
        let image = d(&["image", "inspect", &self.config.image])?;
        ensure!(
            image[0]["Id"] == self.config.image,
            "Image identity mismatch"
        );
        let docker_root = info["DockerRootDir"]
            .as_str()
            .context("Docker root missing")?;
        let mount: Value = serde_json::from_slice(&run_process(
            &[
                "findmnt".into(),
                "-J".into(),
                "-T".into(),
                docker_root.into(),
                "-o".into(),
                "FSTYPE".into(),
            ],
            None,
            Duration::from_secs(10),
        )?)?;
        let kind = mount["filesystems"][0]["fstype"]
            .as_str()
            .context("Cannot inspect Docker storage filesystem")?;
        ensure!(
            ["ext4", "xfs", "btrfs"].contains(&kind),
            "LayerFS Store requires local ext4, XFS or Btrfs; got {kind}"
        );
        let existing = self
            .config
            .root
            .ancestors()
            .find(|p| p.exists())
            .context("State path has no existing ancestor")?;
        let state_mount: Value = serde_json::from_slice(&run_process(
            &[
                "findmnt".into(),
                "-J".into(),
                "-T".into(),
                existing.to_string_lossy().into_owned(),
                "-o".into(),
                "FSTYPE".into(),
            ],
            None,
            Duration::from_secs(10),
        )?)?;
        ensure!(
            state_mount["filesystems"][0]["fstype"]
                .as_str()
                .is_some_and(|kind| ["ext4", "xfs", "btrfs"].contains(&kind)),
            "SDK metadata and import staging require local ext4, XFS or Btrfs too"
        );
        for volume in [&self.config.pnpm_store, &self.config.pnpm_metadata]
            .into_iter()
            .flatten()
        {
            let value = d(&["volume", "inspect", volume])?;
            ensure!(
                value[0]["Driver"] == "local"
                    && value[0]["Options"].as_object().is_none_or(|v| v.is_empty()),
                "Cache must be an existing local volume"
            );
        }
        Ok(
            json!({"ok":true,"experimental":true,"revision":crate::REVISION,"filesystem":kind,
            "max_workspaces":self.config.max_workspaces,"workspace_cpu_budget":self.config.max_workspaces as u64 * self.config.cpus_per_workspace as u64,
            "workspace_memory_mib_budget":self.config.max_workspaces as u64 * self.config.memory_mib_per_workspace as u64,
            "limitations":["One administrator/trust boundary per root","No VM attachment or RAM checkpoint integration","No crash-durability guarantee","Immutable cache generations require no external writers","Disk reserve is an admission check, not a write quota","SDK helper adds up to 1 CPU and 1 GiB per concurrent operation"]}),
        )
    }
    fn space(&self, bytes: u64) -> Result<()> {
        ensure!(
            fs2::available_space(&self.config.root)?
                >= bytes + self.config.disk_reserve_gib * enginebox_core::GIB,
            "Insufficient free state disk after reserve"
        );
        let info = d(&["info", "--format", "{{json .}}"])?;
        let docker_root = info["DockerRootDir"]
            .as_str()
            .context("Docker root missing")?;
        ensure!(
            fs2::available_space(docker_root)?
                >= bytes + self.config.disk_reserve_gib * enginebox_core::GIB,
            "Insufficient free Docker storage after reserve"
        );
        Ok(())
    }
    fn bridge(&self, p: &Project, operation: &str, extra: Value, timeout: u64) -> Result<Value> {
        self.owned_volume(p)?;
        let home = self.project_dir(&p.name).join("home");
        let mut request = json!({"operation":operation,"project":p.name,"home":home,"volume":p.volume,
            "prefix":format!("ebox-lfs-{}-",p.owner),"sdk":self.config.sdk,"image":self.config.image});
        request
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().context("Invalid bridge request")?.clone());
        let call = format!("ebox-lfs-call-{}", uuid::Uuid::new_v4().simple());
        let mut args = vec![
            "run".into(),
            "--rm".into(),
            "--name".into(),
            call.clone(),
            "--network".into(),
            "host".into(),
            "--cpus=1".into(),
            "--memory=1g".into(),
            "--pids-limit=256".into(),
            "--label".into(),
            format!("enginebox.layerfs.owner={}", p.owner),
            "--mount".into(),
            format!(
                "type=bind,src={},dst={}",
                self.config.root.display(),
                self.config.root.display()
            ),
            "--mount".into(),
            format!(
                "type=bind,src={},dst={},readonly",
                self.config.sdk.display(),
                self.config.sdk.display()
            ),
            "--mount".into(),
            format!(
                "type=bind,src={},dst=/opt/enginebox-layerfs/daemon,readonly",
                self.config.daemon.display()
            ),
            "--mount".into(),
            "type=bind,src=/var/run/docker.sock,dst=/var/run/docker.sock".into(),
            "--mount".into(),
            format!("type=volume,src={},dst=/layerfs-store", p.volume),
            "-i".into(),
            self.config.image.clone(),
            "timeout".into(),
            "--kill-after=5s".into(),
            format!("{timeout}s"),
            "node".into(),
            self.config
                .root
                .join("bridge.mjs")
                .to_string_lossy()
                .into_owned(),
        ];
        // Explicit argv and stdin JSON: user commands never become host shell text.
        let result = docker(&args, Some(&serde_json::to_vec(&request)?), timeout + 15);
        args.clear();
        let output = result.with_context(|| {
            format!("SDK outcome uncertain; inspect retained helper {call}; no automatic replay")
        })?;
        let response: Value =
            serde_json::from_slice(&output).context("Invalid SDK response; outcome uncertain")?;
        ensure!(
            response["ok"] == true,
            "LayerFS refusal: {}",
            response["error"]
        );
        Ok(response["result"].clone())
    }
    fn container(&self, p: &Project, sandbox: &str) -> Result<String> {
        identifier(sandbox)?;
        let file = self
            .project_dir(&p.name)
            .join("home/_sandboxes")
            .join(sandbox)
            .join("container");
        real_path(&file)?;
        let id = fs::read_to_string(file)?.trim().to_owned();
        ensure!(crate::config::digest(&id), "Invalid container identity");
        self.verify_container(p, sandbox, &id)?;
        Ok(id)
    }
    fn verify_container(&self, p: &Project, sandbox: &str, id: &str) -> Result<()> {
        let v = d(&["inspect", id])?;
        let c = &v[0];
        ensure!(
            c["Id"] == id
                && c["Image"] == self.config.image
                && c["Name"] == format!("/ebox-lfs-{}-{sandbox}", p.owner)
                && c["Mounts"]
                    .as_array()
                    .context("Missing mounts")?
                    .iter()
                    .any(|m| m["Name"] == p.volume && m["Destination"] == "/layerfs-store"),
            "Refusing foreign sandbox container"
        );
        Ok(())
    }
    fn limits(&self, id: &str) -> Result<()> {
        docker(
            &[
                "update".into(),
                "--cpus".into(),
                self.config.cpus_per_workspace.to_string(),
                "--memory".into(),
                format!("{}m", self.config.memory_mib_per_workspace),
                "--memory-swap".into(),
                format!("{}m", self.config.memory_mib_per_workspace),
                "--pids-limit=512".into(),
                id.into(),
            ],
            None,
            30,
        )?;
        let v = d(&["inspect", id])?;
        ensure!(
            v[0]["HostConfig"]["Memory"]
                == self.config.memory_mib_per_workspace as u64 * 1024 * 1024
                && v[0]["HostConfig"]["NanoCpus"]
                    == self.config.cpus_per_workspace as u64 * 1_000_000_000,
            "Sandbox limits did not apply"
        );
        Ok(())
    }
    fn alias(&self, w: &Workspace, attach: bool) -> Result<()> {
        let id = w.container.as_deref().context("No sandbox container")?;
        let mount = w.mount.as_deref().context("No workspace mount")?;
        ensure!(
            mount
                .strip_prefix("/workspaces/")
                .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())),
            "Invalid workspace mount path"
        );
        let script = if attach {
            format!(
                "set -eu; test ! -L /project; mkdir -p /project; test -z \"$(ls -A /project)\"; mount --bind {} /project; mountpoint -q /project",
                shell_quote(mount)
            )
        } else {
            "set -eu; mountpoint -q /project; umount /project".into()
        };
        docker(
            &[
                "exec".into(),
                "--user=0".into(),
                id.into(),
                "sh".into(),
                "-c".into(),
                script,
            ],
            None,
            30,
        )?;
        Ok(())
    }
    pub fn import(
        &self,
        name: &str,
        repo: &std::path::Path,
        revision: &str,
        approval: &str,
    ) -> Result<Value> {
        identifier(name)?;
        let preview = source::preview(repo, revision)?;
        ensure!(
            preview.approval == approval,
            "Source changed or approval digest differs; review a fresh preview"
        );
        ensure!(
            preview.bytes <= self.config.import_limit_gib * enginebox_core::GIB,
            "Source exceeds import limit"
        );
        self.doctor()?;
        self.root()?;
        let _guard = state::lock(&self.config.root.join(".lock"), false)?;
        self.bind_config()?;
        self.space(preview.bytes.saturating_mul(3))?;
        ensure!(
            self.active_count()? < self.config.max_workspaces,
            "No workspace slot available for the temporary import sandbox"
        );
        let directory = self.project_dir(name);
        ensure!(
            !directory.exists(),
            "Project already exists; retained/uncertain imports require inspection"
        );
        state::private_dir(&directory)?;
        let owner = uuid::Uuid::new_v4().simple().to_string();
        let mut p = Project {
            schema: 1,
            name: name.into(),
            volume: format!("ebox-lfs-{owner}"),
            owner,
            status: "importing".into(),
            commit: preview.commit.clone(),
            approval: preview.approval.clone(),
        };
        let path = directory.join("project.json");
        state::write_json(&path, &json!(p))?;
        let outcome = (|| {
            source::stage(repo, &preview, &directory)?;
            // Docker volume create otherwise silently adopts an existing name.
            let found = docker(
                &[
                    "volume".into(),
                    "ls".into(),
                    "--format={{.Name}}".into(),
                    format!("--filter=name=^{}$", p.volume),
                ],
                None,
                30,
            )?;
            ensure!(
                found.iter().all(|b| b.is_ascii_whitespace()),
                "Store volume already exists"
            );
            docker(
                &[
                    "volume".into(),
                    "create".into(),
                    "--label".into(),
                    format!("enginebox.layerfs.owner={}", p.owner),
                    "--label".into(),
                    format!("enginebox.layerfs.revision={}", crate::REVISION),
                    p.volume.clone(),
                ],
                None,
                30,
            )?;
            self.owned_volume(&p)?;
            self.bridge(
                &p,
                "import",
                json!({"source":directory.join("source")}),
                300,
            )?;
            self.bridge(&p, "create", json!({"sandbox":"base"}), 90)?;
            let id = self.container(&p, "base")?;
            self.limits(&id)?;
            self.bridge(&p, "close", json!({"sandbox":"base"}), 60)?;
            fs::remove_dir_all(directory.join("source"))?;
            Ok(
                json!({"ok":true,"project":name,"source_commit":preview.commit,"files":preview.files.len(),"bytes":preview.bytes}),
            )
        })();
        p.status = if outcome.is_ok() {
            "ready"
        } else {
            "uncertain"
        }
        .into();
        state::write_json(&path, &json!(p))?;
        outcome
    }
    pub fn fork(&self, project: &str, name: &str, from: Option<&str>) -> Result<Value> {
        identifier(name)?;
        self.root()?;
        let _guard = state::lock(&self.config.root.join(".lock"), false)?;
        self.bind_config()?;
        self.space(0)?;
        ensure!(
            !self.workspace_path(name).exists(),
            "Workspace name already has a receipt; names are not recycled"
        );
        let active = state::json_files(&self.config.root.join("workspaces"))?
            .into_iter()
            .map(|p| state::read_json(&p))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|v| v["status"] != "closed")
            .count();
        ensure!(
            active < self.config.max_workspaces,
            "Workspace budget exhausted (uncertain workspaces also count)"
        );
        let p = self.project(project)?;
        ensure!(p.status == "ready", "Project is not ready");
        self.owned_volume(&p)?;
        let _source_lease = from
            .map(|source| {
                identifier(source)?;
                state::lock(
                    &self
                        .config
                        .root
                        .join("workspaces")
                        .join(format!("{source}.lock")),
                    false,
                )
            })
            .transpose()?;
        let source_branch = if let Some(source) = from {
            let parent = self.workspace(source)?;
            ensure!(
                parent.project == project
                    && ["ready", "closed"].contains(&parent.status.as_str())
                    && parent.last_checkpoint.is_some(),
                "Fork source must have a completed checkpoint in this project and no uncertain operation"
            );
            parent.branch
        } else {
            "main".into()
        };
        let key = format!("w-{}", uuid::Uuid::new_v4().simple());
        let mut w = Workspace {
            schema: 1,
            name: name.into(),
            project: project.into(),
            sandbox: key.clone(),
            branch: key,
            container: None,
            mount: None,
            status: "creating".into(),
            last_error: None,
            last_checkpoint: None,
        };
        self.save_workspace(&w)?;
        let outcome = (|| {
            self.bridge(
                &p,
                "fork",
                json!({"branch":w.branch,"from":source_branch}),
                60,
            )?;
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
            self.alias(&w, true)?;
            Ok(
                json!({"ok":true,"workspace":name,"project":project,"path":"/project","branch":w.branch,"cpu":self.config.cpus_per_workspace,"memory_mib":self.config.memory_mib_per_workspace}),
            )
        })();
        self.finish(&mut w, outcome, "ready")
    }
    fn finish(&self, w: &mut Workspace, outcome: Result<Value>, success: &str) -> Result<Value> {
        w.status = if outcome.is_ok() {
            success
        } else {
            "uncertain"
        }
        .into();
        w.last_error = outcome.as_ref().err().map(|e| format!("{e:#}"));
        self.save_workspace(w)?;
        outcome
    }
    pub fn operate(
        &self,
        name: &str,
        operation: &str,
        argv: &[String],
        timeout: u64,
        message: &str,
    ) -> Result<Value> {
        identifier(name)?;
        ensure!(
            (1..=3600).contains(&timeout),
            "Timeout must be 1–3600 seconds"
        );
        ensure!(
            ["exec", "deps", "checkpoint", "remount", "close"].contains(&operation),
            "Unknown operation"
        );
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
        ensure!(
            w.status == "ready",
            "Workspace is {}; inspect its receipt; uncertain operations are never replayed",
            w.status
        );
        let p = self.project(&w.project)?;
        self.owned_volume(&p)?;
        self.verify_container(
            &p,
            &w.sandbox,
            w.container.as_deref().context("Container missing")?,
        )?;
        self.space(0)?;
        let base = json!({"sandbox":w.sandbox,"mount":w.mount});
        if operation == "close" || operation == "remount" {
            let s = self.bridge(&p, "status", base.clone(), 30)?;
            ensure!(
                s["overlay"]["inodes"] == 0
                    && s["overlay"]["entries"] == 0
                    && s["overlay"]["pendingMerge"] == false,
                "Workspace has uncheckpointed changes; run checkpoint before {operation}"
            );
        }
        let arguments = if operation == "deps" {
            ensure!(
                self.config.pnpm_store.is_some(),
                "No immutable pnpm cache configured"
            );
            vec!["bash".into(),"-c".into(),"set -euo pipefail; if test ! -d /tmp/enginebox-pnpm-cache; then cp -R /pnpm-metadata /tmp/enginebox-pnpm-cache; chmod -R u+w /tmp/enginebox-pnpm-cache; fi; export PNPM_CONFIG_CACHE_DIR=/tmp/enginebox-pnpm-cache; pnpm install --frozen-store --offline --frozen-lockfile --ignore-scripts --package-import-method=copy --store-dir /pnpm-store --reporter append-only".into()]
        } else {
            argv.to_vec()
        };
        if operation == "exec" {
            ensure!(
                !argv.is_empty() && argv.iter().all(|s| !s.contains('\0')),
                "Command argv required"
            );
        }
        ensure!(message.len() <= 4096, "Checkpoint message too long");
        w.status = operation.into();
        self.save_workspace(&w)?;
        let outcome = (|| {
            let mut request = base.clone();
            match operation {
                "exec" | "deps" => {
                    request["argv"] = json!(arguments);
                    request["timeout"] = json!(timeout);
                    let result = self.bridge(&p, "exec", request, timeout + 10)?;
                    Ok(json!({"ok":result["exitCode"]==0,"workspace":name,"result":result}))
                }
                "checkpoint" => {
                    request["message"] = json!(message);
                    let result = self.bridge(&p, "checkpoint", request, 120)?;
                    w.last_checkpoint = Some(result.clone());
                    Ok(json!({"ok":true,"result":result}))
                }
                "close" | "remount" => {
                    self.alias(&w, false)?;
                    self.bridge(&p, "unmount", base.clone(), 60)?;
                    if operation == "close" {
                        self.bridge(&p, "close", json!({"sandbox":w.sandbox}), 60)?;
                    } else {
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
                        self.alias(&w, true)?;
                    }
                    Ok(
                        json!({"ok":true,"workspace":name,"path":"/project","status":if operation=="close"{"closed"}else{"ready"}}),
                    )
                }
                _ => unreachable!(),
            }
        })();
        self.finish(
            &mut w,
            outcome,
            if operation == "close" {
                "closed"
            } else {
                "ready"
            },
        )
    }
    pub fn list(&self) -> Result<Value> {
        let path = self.config.root.join("workspaces");
        if !path.exists() {
            return Ok(json!({"workspaces":[]}));
        }
        let values = state::json_files(&path)?
            .into_iter()
            .map(|p| state::read_json(&p))
            .collect::<Result<Vec<_>>>()?;
        Ok(json!({"workspaces":values}))
    }
}
