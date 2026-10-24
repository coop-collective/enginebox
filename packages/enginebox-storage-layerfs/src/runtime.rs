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
#[path = "dev_export.rs"]
mod dev_export;
#[path = "github_runtime.rs"]
mod github_runtime;
#[path = "integration.rs"]
mod integration;
#[path = "preparation.rs"]
mod preparation;
#[path = "publication.rs"]
mod publication;
#[path = "recovery.rs"]
mod recovery;
#[derive(Clone, Serialize, Deserialize)]
struct Project {
    schema: u32,
    name: String,
    owner: String,
    volume: String,
    status: String,
    commit: String,
    approval: String,
    #[serde(default)]
    source_only: bool,
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
    /// Retained after a transport failure so recovery cannot mistake a partial
    /// checkpoint/remount for a completed ordinary command.
    #[serde(default)]
    last_operation: Option<String>,
    #[serde(default)]
    recoveries: Vec<Value>,
    #[serde(default)]
    pending_call: Option<String>,
}
pub struct Runtime {
    pub config: Config,
}
fn docker_at(
    socket: &std::path::Path,
    args: &[String],
    input: Option<&[u8]>,
    seconds: u64,
) -> Result<Vec<u8>> {
    let mut command = vec![
        "docker".into(),
        "--host".into(),
        format!("unix://{}", socket.display()),
    ];
    command.extend_from_slice(args);
    run_process(&command, input, Duration::from_secs(seconds))
}
fn docker_api(version: &str) -> Result<(u32, u32)> {
    let (major, minor) = version
        .split_once('.')
        .context("Invalid Docker API version")?;
    Ok((major.parse()?, minor.parse()?))
}
fn require_docker_api(server: &Value) -> Result<()> {
    let max = server["ApiVersion"]
        .as_str()
        .context("Docker maximum API version missing")?;
    let min = server["MinAPIVersion"]
        .as_str()
        .context("Docker minimum API version missing")?;
    ensure!(
        docker_api(min)? <= (1, 54) && docker_api(max)? >= (1, 54),
        "Pinned LayerFS requires Docker API 1.54; server supports {min}–{max}. Provision a compatible dedicated node; this command never upgrades the daemon"
    );
    Ok(())
}
pub(crate) fn verify_socket(_path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let path = _path;
        let link = fs::symlink_metadata(path)
            .context("Provision the configured local Docker Unix socket first")?;
        ensure!(
            !link.file_type().is_symlink(),
            "Docker socket itself must not be a symlink"
        );
        if path != crate::config::default_docker_socket() {
            real_path(path)?;
        }
        // /var/run -> /run is the conventional default on Linux. Verify the
        // resolved socket and parent permissions without rebinding its config.
        let resolved = path.canonicalize()?;
        let metadata = fs::metadata(&resolved)?;
        ensure!(
            metadata.file_type().is_socket() && metadata.uid() == 0 && metadata.mode() & 0o002 == 0,
            "Docker endpoint must be an existing root-owned Unix socket, not writable by others"
        );
        for parent in resolved.ancestors().skip(1) {
            let metadata = fs::metadata(parent)?;
            ensure!(
                metadata.is_dir() && metadata.mode() & 0o022 == 0,
                "Docker socket parent must not be group/other writable: {}",
                parent.display()
            );
        }
    }
    Ok(())
}
impl Runtime {
    fn export_workspace_files(&self, w: &Workspace, destination: &std::path::Path) -> Result<()> {
        let container = w.container.as_deref().context("Export container missing")?;
        // Docker's archive API reads the container root filesystem, not mounts
        // installed later by LayerFS. Materialize from inside the mount namespace
        // first; copying /project directly can silently produce an empty tree.
        let staging = format!("/tmp/ebox-export-{}", uuid::Uuid::new_v4().simple());
        self.docker(
            &[
                "exec".into(),
                "--user=501:20".into(),
                container.into(),
                "sh".into(),
                "-c".into(),
                format!("set -eu; mkdir -m 700 {staging}; cp -a -- /project/. {staging}/"),
            ],
            None,
            300,
        )?;
        self.docker(
            &[
                "cp".into(),
                format!("{container}:{staging}/."),
                destination.to_string_lossy().into_owned(),
            ],
            None,
            300,
        )?;
        Ok(())
    }
    fn docker(&self, args: &[String], input: Option<&[u8]>, seconds: u64) -> Result<Vec<u8>> {
        docker_at(&self.config.docker_socket, args, input, seconds)
    }
    fn d(&self, args: &[&str]) -> Result<Value> {
        Ok(serde_json::from_slice(&self.docker(
            &args.iter().map(|s| (*s).into()).collect::<Vec<_>>(),
            None,
            30,
        )?)?)
    }
    pub fn new(config: Config) -> Result<Self> {
        ensure!(
            cfg!(target_os = "linux"),
            "LayerFS execution requires a local Linux Docker node; preview works on Unix operator machines"
        );
        ensure!(
            run_process(&["id".into(), "-u".into()], None, Duration::from_secs(5))? == b"0\n",
            "Run this node-local backend as root; SDK metadata is written by the trusted root Docker helper"
        );
        config.verify_artifacts()?;
        verify_socket(&config.docker_socket)?;
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
        let integration = self.config.root.join("integration.mjs");
        if integration.exists() {
            real_path(&integration)?;
            ensure!(
                fs::read_to_string(&integration)? == include_str!("integration.mjs"),
                "Integration bridge content mismatch"
            );
        } else {
            fs::write(integration, include_str!("integration.mjs"))?;
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
        let sync = self.docker(
            &[
                "ps".into(),
                "--filter=label=enginebox.sync.attempt".into(),
                "--format={{.ID}}".into(),
            ],
            None,
            30,
        )?;
        let workspaces = state::json_files(&self.config.root.join("workspaces"))?
            .into_iter()
            .map(|p| state::read_json(&p))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|v| v["status"] != "closed")
            .count();
        Ok(workspaces + String::from_utf8(sync)?.lines().count())
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
        ensure!(
            [1, 2].contains(&w.schema) && w.name == name,
            "Invalid workspace receipt"
        );
        identifier(&w.sandbox)?;
        identifier(&w.branch)?;
        Ok(w)
    }
    fn save_workspace(&self, w: &Workspace) -> Result<()> {
        // Old runtimes must refuse rather than silently erase recovery custody.
        let mut upgraded = w.clone();
        upgraded.schema = 2;
        state::write_json(
            &self.workspace_path(&w.name),
            &serde_json::to_value(upgraded)?,
        )
    }
    fn owned_volume(&self, p: &Project) -> Result<()> {
        let v = self.d(&["volume", "inspect", &p.volume])?;
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
        let version = self.d(&["version", "--format", "{{json .Server}}"])?;
        require_docker_api(&version)?;
        let info = self.d(&["info", "--format", "{{json .}}"])?;
        ensure!(
            info["OSType"] == "linux",
            "Local Linux Docker daemon required"
        );
        let image = self.d(&["image", "inspect", &self.config.image])?;
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
            let value = self.d(&["volume", "inspect", volume])?;
            ensure!(
                value[0]["Driver"] == "local"
                    && value[0]["Options"].as_object().is_none_or(|v| v.is_empty()),
                "Cache must be an existing local volume"
            );
        }
        Ok(
            json!({"ok":true,"backend":"layerfs","qualification":"preview","revision":crate::REVISION,"filesystem":kind,"docker_api":version["ApiVersion"],
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
        let info = self.d(&["info", "--format", "{{json .}}"])?;
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
        let bridge_path = if operation.starts_with("preparation-") {
            let content = include_str!("preparation-bridge.mjs");
            let path = self.config.root.join("preparation-v1.mjs");
            if path.exists() {
                real_path(&path)?;
                ensure!(
                    fs::read_to_string(&path)? == content,
                    "Preparation adapter differs; install a compatible version"
                );
            } else {
                use std::io::Write;
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)?;
                file.write_all(content.as_bytes())?;
                file.sync_all()?;
            }
            path
        } else if operation.starts_with("project-") {
            let content = include_str!("publication-bridge.mjs");
            let path = self.config.root.join("publication-v1.mjs");
            if path.exists() {
                real_path(&path)?;
                ensure!(
                    fs::read_to_string(&path)? == content,
                    "Publication adapter differs; install a compatible version"
                );
            } else {
                use std::io::Write;
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)?;
                file.write_all(content.as_bytes())?;
                file.sync_all()?;
            }
            path
        } else {
            self.config.root.join("bridge.mjs")
        };
        let home = self.project_dir(&p.name).join("home");
        let mut request = json!({"operation":operation,"project":p.name,"home":home,"volume":p.volume,
            "prefix":format!("ebox-lfs-{}-",p.owner),"sdk":self.config.sdk,"image":self.config.image});
        request
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().context("Invalid bridge request")?.clone());
        let call = extra["_call"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("ebox-lfs-call-{}", uuid::Uuid::new_v4().simple()));
        identifier(&call)?;
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
            format!(
                "type=bind,src={},dst=/var/run/docker.sock",
                self.config.docker_socket.display()
            ),
            "--mount".into(),
            format!("type=volume,src={},dst=/layerfs-store", p.volume),
            "-i".into(),
            self.config.image.clone(),
            "timeout".into(),
            "--kill-after=5s".into(),
            format!("{timeout}s"),
            "node".into(),
            bridge_path.to_string_lossy().into_owned(),
        ];
        if extra["_call"].is_string() {
            let calls = self.project_dir(&p.name).join("calls");
            state::private_dir(&calls)?;
            let path = calls.join(&call);
            state::write_json(
                &path.with_extension("json"),
                &json!({"schema":1,"call":call,"owner":p.owner,"project":p.name,"sandbox":extra["sandbox"],"operation":operation}),
            )?;
            let result = shell_quote(&path.with_extension("result").to_string_lossy());
            let done = shell_quote(&path.with_extension("finished").to_string_lossy());
            let started = shell_quote(&path.with_extension("started").to_string_lossy());
            let bridge = shell_quote(&self.config.root.join("bridge.mjs").to_string_lossy());
            // The trusted helper writes completion before returning to its
            // operator. An absent helper alone cannot prove a delayed launch
            // will not start, so recovery requires this exact settled result.
            let script = format!(
                "umask 077; : > {started}; timeout --kill-after=5s {timeout}s node {bridge} > {result}; code=$?; printf '%s\\n' \"$code\" > {done}; cat {result}; exit \"$code\""
            );
            args.truncate(args.len() - 5);
            args.extend(["sh".into(), "-c".into(), script]);
        }
        // Explicit argv and stdin JSON: user commands never become host shell text.
        let result = self.docker(&args, Some(&serde_json::to_vec(&request)?), timeout + 15);
        args.clear();
        let output = result.with_context(|| {
            format!("SDK outcome uncertain; inspect retained helper {call}; no automatic replay")
        })?;
        let response: Value =
            serde_json::from_slice(&output).context("Invalid SDK response; outcome uncertain")?;
        if operation == "integrate"
            && response["ok"] == false
            && response["error"]["code"] == "Conflict"
        {
            return Ok(json!({"ok":false,"conflict":response["error"]}));
        }
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
        let v = self.d(&["inspect", id])?;
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
        self.docker(
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
        let v = self.d(&["inspect", id])?;
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
        self.docker(
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
        _source_only: bool,
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
            source_only: true,
        };
        let path = directory.join("project.json");
        state::write_json(&path, &json!(p))?;
        let outcome = (|| {
            source::stage_files(repo, &preview, &directory)?;
            // Docker volume create otherwise silently adopts an existing name.
            let found = self.docker(
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
            self.docker(
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
        self.fork_at(project, name, from, None)
    }
    fn fork_at(
        &self,
        project: &str,
        name: &str,
        from: Option<&str>,
        exact_ref: Option<&str>,
    ) -> Result<Value> {
        identifier(name)?;
        self.root()?;
        let _guard = state::lock(&self.config.root.join(".lock"), false)?;
        self.bind_config()?;
        self.space(0)?;
        ensure!(
            !self.workspace_path(name).exists(),
            "Workspace name already has a receipt; names are not recycled"
        );
        let active = self.active_count()?;
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
            crate::incus::ensure_detached(&self.config.root, source)?;
            ensure!(
                parent.project == project
                    && ["ready", "closed"].contains(&parent.status.as_str())
                    && parent.last_checkpoint.is_some(),
                "Fork source must have a completed checkpoint in this project and no uncertain operation"
            );
            parent.branch
        } else {
            exact_ref.unwrap_or("main").into()
        };
        let key = format!("w-{}", uuid::Uuid::new_v4().simple());
        let mut w = Workspace {
            schema: 2,
            name: name.into(),
            project: project.into(),
            sandbox: key.clone(),
            branch: key,
            container: None,
            mount: None,
            status: "creating".into(),
            last_error: None,
            last_checkpoint: None,
            last_operation: Some("creating".into()),
            recoveries: vec![],
            pending_call: None,
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
        crate::incus::ensure_detached(&self.config.root, name)?;
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
        w.last_operation = Some(operation.into());
        self.save_workspace(&w)?;
        let outcome = (|| {
            let mut request = base.clone();
            match operation {
                "exec" | "deps" => {
                    let call = format!("ebox-lfs-call-{}", uuid::Uuid::new_v4().simple());
                    w.pending_call = Some(call.clone());
                    self.save_workspace(&w)?;
                    request["_call"] = json!(call);
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
    pub fn incus_attach(&self, name: &str, target: crate::incus::Target) -> Result<Value> {
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
        let w = self.workspace(name)?;
        ensure!(
            w.status == "ready",
            "Workspace is not ready for Incus attachment"
        );
        let p = self.project(&w.project)?;
        self.owned_volume(&p)?;
        let id = w.container.as_deref().context("Container missing")?;
        self.verify_container(&p, &w.sandbox, id)?;
        self.no_helpers(&p)?;
        self.probe(&w)?;
        // Older runtimes do not understand attachment leases. Upgrade before
        // publishing an export so they refuse this workspace after rollback.
        self.save_workspace(&w)?;
        crate::incus::attach(
            &self.config.root,
            name,
            id,
            target,
            &self.config.docker_socket,
        )
    }
    pub fn incus_verify(&self, name: &str) -> Result<Value> {
        identifier(name)?;
        self.verify_binding()?;
        let _lease = state::lock(
            &self
                .config
                .root
                .join("workspaces")
                .join(format!("{name}.lock")),
            false,
        )?;
        self.workspace(name)?;
        crate::incus::verify(&self.config.root, name, &self.config.docker_socket)
    }
    pub fn incus_detach(&self, name: &str) -> Result<Value> {
        identifier(name)?;
        self.verify_binding()?;
        let _lease = state::lock(
            &self
                .config
                .root
                .join("workspaces")
                .join(format!("{name}.lock")),
            false,
        )?;
        self.workspace(name)?;
        crate::incus::detach(&self.config.root, name, &self.config.docker_socket)
    }
    /// Metadata reconciliation must still report a stopped Docker daemon or
    /// missing SDK artifact after reboot. It executes no SDK or mount operation,
    /// so use the bound configuration without the runtime artifact preflight.
    pub fn reconcile_incus(config: Config, name: &str, apply: bool) -> Result<Value> {
        ensure!(
            cfg!(target_os = "linux"),
            "Incus reconciliation requires Linux"
        );
        ensure!(
            run_process(&["id".into(), "-u".into()], None, Duration::from_secs(5))? == b"0\n",
            "Run node-local reconciliation as root"
        );
        config.validate()?;
        identifier(name)?;
        let runtime = Self { config };
        runtime.verify_binding()?;
        state::private_dir(&runtime.config.root)?;
        let _lease = state::lock(
            &runtime
                .config
                .root
                .join("workspaces")
                .join(format!("{name}.lock")),
            false,
        )?;
        let w = runtime.workspace(name)?;
        ensure!(
            w.status == "ready",
            "Workspace storage operation needs separate recovery before attachment reconciliation"
        );
        crate::incus::reconcile(
            &runtime.config.root,
            name,
            w.container
                .as_deref()
                .context("Workspace container missing")?,
            &runtime.config.docker_socket,
            apply,
        )
    }
    pub fn list(&self) -> Result<Value> {
        let github = if self.config.root.join("github").exists() {
            crate::github::Repositories::new(&self.config.root)?.list()?
        } else {
            vec![]
        };
        let path = self.config.root.join("workspaces");
        if !path.exists() {
            return Ok(json!({"workspaces":[],"github_projects":github}));
        }
        let mut values = state::json_files(&path)?
            .into_iter()
            .map(|p| state::read_json(&p))
            .collect::<Result<Vec<_>>>()?;
        for workspace in &mut values {
            for project in &github {
                if let Some(generation) = project["generations"]
                    .as_array()
                    .and_then(|gs| gs.iter().find(|g| g["project"] == workspace["project"]))
                {
                    workspace["github_project"] = project["id"].clone();
                    workspace["source_commit"] = generation["commit"].clone();
                }
            }
        }
        Ok(json!({"workspaces":values,"github_projects":github}))
    }
}
