use super::*;
use crate::{
    github::{GitHubRemote, Repositories},
    sync::{Outbox, Status},
};
use std::path::Path;

impl Runtime {
    fn publication_head(&self, p: &Project) -> Result<Value> {
        self.bridge(p, "project-head", json!({"branch":"main"}), 30)
    }
    fn main_revision(&self, p: &Project) -> Result<String> {
        Ok(state::digest(&self.publication_head(p)?))
    }
    fn publication_sequence(&self, p: &Project) -> Result<u64> {
        let path = self.project_dir(&p.name).join("main.json");
        if !path.exists() {
            return Ok(0);
        }
        let value = state::read_json(&path)?;
        ensure!(
            value["status"] == "published",
            "Previous publication is uncertain; inspect its receipt before continuing"
        );
        value["sequence"]
            .as_u64()
            .context("Invalid publication sequence")
    }
    fn main_lock(&self, p: &Project) -> Result<state::StateLock> {
        ensure!(p.status == "ready", "Project is not ready");
        state::lock(&self.project_dir(&p.name).join("publication.lock"), false)
    }
    /// A closed workspace has no live overlay writer. Main is compared before
    /// any merge/publish; a journal remains if the SDK response is uncertain.
    fn promote_locked(
        &self,
        p: &Project,
        name: &str,
        expected: &str,
        merge: bool,
        message: &str,
    ) -> Result<Value> {
        ensure!(
            crate::config::digest(expected),
            "Exact main revision required"
        );
        ensure!(
            message.len() <= 4096 && !message.contains('\0'),
            "Invalid publication message"
        );
        let _workspace = state::lock(
            &self
                .config
                .root
                .join("workspaces")
                .join(format!("{name}.lock")),
            false,
        )?;
        let w = self.workspace(name)?;
        ensure!(
            w.project == p.name && w.status == "closed" && w.last_checkpoint.is_some(),
            "Checkpoint and close a workspace in this project before promotion"
        );
        crate::incus::ensure_detached(&self.config.root, name)?;
        ensure!(
            self.main_revision(p)? == expected,
            "LayerFS main advanced; review or merge against its current revision"
        );
        let sequence = self
            .publication_sequence(p)?
            .checked_add(1)
            .context("Publication sequence exhausted")?;
        let path = self.project_dir(&p.name).join("main.json");
        let branch = if merge {
            let candidate = format!("m-{}", uuid::Uuid::new_v4().simple());
            self.bridge(
                p,
                "project-merge",
                json!({"branch":w.branch,"candidate":candidate,"message":message}),
                120,
            )?;
            candidate
        } else {
            w.branch.clone()
        };
        // A merge refusal leaves main untouched. Publication itself is journaled
        // before crossing the SDK boundary and is never implicitly replayed.
        state::write_json(
            &path,
            &json!({"schema":1,"status":"publishing","sequence":sequence,"previous":expected,"source":name,"branch":branch}),
        )?;
        self.bridge(p, "project-publish", json!({"branch":branch}), 120)?;
        let revision = self.main_revision(p)?;
        let result = json!({"schema":1,"status":"published","sequence":sequence,"revision":revision,"previous":expected,"source":name,"branch":branch});
        state::write_json(&path, &result)?;
        Ok(result)
    }
    pub fn project_main(&self, project: &str) -> Result<Value> {
        self.verify_binding()?;
        let p = self.project(project)?;
        let _lease = self.main_lock(&p)?;
        Ok(
            json!({"project":project,"revision":self.main_revision(&p)?,"sequence":self.publication_sequence(&p)?}),
        )
    }
    pub fn project_promote(
        &self,
        project: &str,
        name: &str,
        expected: &str,
        merge: bool,
        message: &str,
    ) -> Result<Value> {
        identifier(name)?;
        self.verify_binding()?;
        let p = self.project(project)?;
        let _lease = self.main_lock(&p)?;
        self.promote_locked(&p, name, expected, merge, message)
    }
    fn github_binding(&self, project: &str) -> Result<(Repositories, Project, Outbox, Value)> {
        let repos = Repositories::new(&self.config.root)?;
        let generation = repos.current(project)?;
        let p = self.project(&generation.project)?;
        let binding = repos
            .list()?
            .into_iter()
            .find(|r| r["id"] == project)
            .context("GitHub connection missing")?;
        let outbox = Outbox::open(
            &self.config.root.join("github").join(project).join("outbox"),
            &generation.commit,
        )?;
        Ok((repos, p, outbox, binding))
    }
    pub fn github_settings(&self, project: &str, enabled: Option<bool>) -> Result<Value> {
        self.verify_binding()?;
        let (_, p, outbox, _) = self.github_binding(project)?;
        let _lease = self.main_lock(&p)?;
        if let Some(enabled) = enabled {
            outbox.configure(enabled)?;
        }
        Ok(
            json!({"project":project,"main":{"revision":self.main_revision(&p)?,"sequence":self.publication_sequence(&p)?},"sync":outbox.status()?}),
        )
    }
    fn export_main(&self, p: &Project, revision: &str) -> Result<PathBuf> {
        let exports = self.project_dir(&p.name).join("exports");
        state::private_dir(&exports)?;
        let directory = exports.join(revision);
        if directory.join("complete.json").exists() {
            ensure!(
                state::read_json(&directory.join("complete.json"))?["digest"]
                    == crate::sync::snapshot_digest(&directory.join("source"))?,
                "Retained export integrity check failed"
            );
            return Ok(directory.join("source"));
        }
        ensure!(
            !directory.exists(),
            "Interrupted source export needs inspection; it is not replayed"
        );
        state::private_dir(&directory)?;
        let name = format!("export-{}", uuid::Uuid::new_v4().simple());
        state::write_json(
            &directory.join("receipt.json"),
            &json!({"workspace":name,"revision":revision}),
        )?;
        self.fork(&p.name, &name, None)?;
        let w = self.workspace(&name)?;
        let source = directory.join("source");
        state::private_dir(&source)?;
        self.docker(
            &[
                "cp".into(),
                format!(
                    "{}:/project/.",
                    w.container.context("Export container missing")?
                ),
                source.to_string_lossy().into_owned(),
            ],
            None,
            300,
        )?;
        self.operate(&name, "close", &[], 60, "")?;
        ensure!(
            self.main_revision(p)? == revision,
            "Main changed during immutable export"
        );
        state::write_json(
            &directory.join("complete.json"),
            &json!({"revision":revision,"digest":crate::sync::snapshot_digest(&source)?}),
        )?;
        Ok(source)
    }
    fn accept_resolution(
        &self,
        p: &Project,
        attempt: &crate::sync::Attempt,
        files: &Path,
    ) -> Result<(String, u64)> {
        let name = format!(
            "resolve-{}",
            attempt
                .id
                .strip_prefix("sync-")
                .context("Invalid sync attempt")?
        );
        let main_path = self.project_dir(&p.name).join("main.json");
        if main_path.exists() {
            let published = state::read_json(&main_path)?;
            if published["status"] == "published"
                && published["source"] == name
                && published["previous"] == attempt.layerfs_revision
            {
                let accepted = published["revision"]
                    .as_str()
                    .context("Accepted revision missing")?;
                ensure!(
                    self.main_revision(p)? == accepted,
                    "LayerFS advanced after reconciliation acceptance"
                );
                return Ok((
                    accepted.into(),
                    published["sequence"]
                        .as_u64()
                        .context("Accepted sequence missing")?,
                ));
            }
        }
        ensure!(
            self.main_revision(p)? == attempt.layerfs_revision,
            "LayerFS main advanced during agent reconciliation"
        );
        if self.workspace_path(&name).exists() {
            let w = self.workspace(&name)?;
            ensure!(
                w.project == p.name && w.status == "closed" && w.last_checkpoint.is_some(),
                "Resolution workspace is incomplete; inspect it without replaying file operations"
            );
            let published = self.promote_locked(
                p,
                &name,
                &attempt.layerfs_revision,
                false,
                "Accepted GitHub reconciliation",
            )?;
            return Ok((
                published["revision"].as_str().unwrap().into(),
                published["sequence"].as_u64().unwrap(),
            ));
        }
        self.fork(&p.name, &name, None)?;
        let w = self.workspace(&name)?;
        let container = w.container.context("Resolution container missing")?;
        let archive = files
            .parent()
            .context("Resolution staging")?
            .join("source.tar");
        run_process(
            &[
                "tar".into(),
                "-C".into(),
                files.to_string_lossy().into_owned(),
                "-cf".into(),
                archive.to_string_lossy().into_owned(),
                ".".into(),
            ],
            None,
            Duration::from_secs(300),
        )?;
        self.docker(
            &[
                "cp".into(),
                archive.to_string_lossy().into_owned(),
                format!("{container}:/tmp/enginebox-resolution.tar"),
            ],
            None,
            300,
        )?;
        self.docker(
            &[
                "exec".into(),
                "--user=0".into(),
                container.clone(),
                "chmod".into(),
                "644".into(),
                "/tmp/enginebox-resolution.tar".into(),
            ],
            None,
            30,
        )?;
        // Apply only source deletions and writes. Prepared dependencies and
        // application files excluded by Git remain in the LayerFS branch.
        let removals = files
            .parent()
            .context("Resolution staging")?
            .join("remove.json");
        let paths = state::read_json(&removals)?;
        for path in paths.as_array().context("Invalid source removal plan")? {
            source::allowed_path(path.as_str().context("Invalid source path")?)?;
        }
        let writes = state::read_json(
            &files
                .parent()
                .context("Resolution staging")?
                .join("write.json"),
        )?;
        for path in writes.as_array().context("Invalid source write plan")? {
            source::allowed_path(path.as_str().context("Invalid source path")?)?;
        }
        let plan = files
            .parent()
            .context("Resolution staging")?
            .join("apply.json");
        state::write_json(&plan, &json!({"remove":paths,"write":writes}))?;
        self.docker(
            &[
                "cp".into(),
                plan.to_string_lossy().into_owned(),
                format!("{container}:/tmp/enginebox-apply.json"),
            ],
            None,
            30,
        )?;
        self.docker(
            &[
                "exec".into(),
                "--user=0".into(),
                container.clone(),
                "chmod".into(),
                "644".into(),
                "/tmp/enginebox-apply.json".into(),
            ],
            None,
            30,
        )?;
        let script = include_str!("prepare-source.cjs");
        let command = vec![
            "sh".into(),
            "-c".into(),
            format!(
                "set -eu; node -e {} /tmp/enginebox-apply.json; tar --no-same-owner -xf /tmp/enginebox-resolution.tar -C /project",
                shell_quote(script)
            ),
        ];
        let result = self.operate(&name, "exec", &command, 300, "")?;
        ensure!(result["ok"] == true, "Resolution extraction failed");
        self.operate(
            &name,
            "checkpoint",
            &[],
            120,
            "Accepted GitHub reconciliation",
        )?;
        self.operate(&name, "close", &[], 60, "")?;
        let accepted = self.promote_locked(
            p,
            &name,
            &attempt.layerfs_revision,
            false,
            "Accepted GitHub reconciliation",
        )?;
        Ok((
            accepted["revision"]
                .as_str()
                .context("Accepted revision missing")?
                .into(),
            accepted["sequence"]
                .as_u64()
                .context("Accepted sequence missing")?,
        ))
    }
    pub fn github_promote(
        &self,
        project: &str,
        name: &str,
        expected: &str,
        merge: bool,
        message: &str,
        token: Option<&Path>,
    ) -> Result<Value> {
        identifier(name)?;
        self.verify_binding()?;
        let (_, p, outbox, binding) = self.github_binding(project)?;
        let _lease = self.main_lock(&p)?;
        let publication = self.promote_locked(&p, name, expected, merge, message)?;
        // A successful main publication is not rolled back by network failure.
        let sync = (|| -> Result<Value> {
            if outbox.status()?["enabled"] != true {
                return Ok(json!({"enabled":false}));
            }
            let files = self.export_main(&p, publication["revision"].as_str().unwrap())?;
            let status = outbox.enqueue(
                publication["sequence"].as_u64().unwrap(),
                publication["revision"].as_str().unwrap(),
                &files,
            )?;
            let id = status["active"]["id"]
                .as_str()
                .context("Sync attempt missing")?;
            self.drive_sync(&p, &outbox, &binding, id, token)
        })();
        Ok(
            json!({"ok":true,"publication":publication,"sync":match sync {Ok(value)=>value,Err(error)=>json!({"status":"pending","error":error.to_string()})}}),
        )
    }
    fn drive_sync(
        &self,
        p: &Project,
        outbox: &Outbox,
        binding: &Value,
        id: &str,
        token: Option<&Path>,
    ) -> Result<Value> {
        let remote = GitHubRemote::new(
            binding["repository"]
                .as_str()
                .context("Repository missing")?,
            binding["branch"].as_str().context("Branch missing")?,
            token,
            &self
                .config
                .root
                .join("github")
                .join(binding["id"].as_str().unwrap())
                .join("repository.git"),
        )?;
        let mut attempt = outbox.read(id)?;
        if attempt.status != Status::Delivered && attempt.status != Status::Accepting {
            let expected = attempt
                .accepted_revision
                .as_deref()
                .unwrap_or(&attempt.layerfs_revision);
            ensure!(
                self.main_revision(p)? == expected,
                "LayerFS main advanced; queue its latest accepted revision"
            );
        }
        if attempt.status == Status::Queued {
            attempt = outbox.prepare(id, &remote)?;
        }
        if attempt.status == Status::NeedsAgent {
            return Ok(json!({"attempt":attempt,"agent_task_required":true}));
        }
        if matches!(
            attempt.status,
            Status::AwaitingAcceptance | Status::Accepting
        ) {
            attempt = outbox.accept(id, |a, files| self.accept_resolution(p, a, files))?;
        }
        if matches!(attempt.status, Status::Ready | Status::Delivering) {
            attempt = outbox.deliver(id, &remote)?;
        }
        Ok(json!({"attempt":attempt}))
    }
    fn agent_environment(&self, outbox: &Outbox, id: &str) -> Result<Value> {
        ensure!(
            outbox.read(id)?.status == Status::NeedsAgent,
            "No conflict agent is needed for this attempt"
        );
        let worktree = outbox.worktree(id)?;
        let directory = worktree.parent().context("Attempt directory missing")?;
        let path = directory.join("agent-environment.json");
        let name = format!("ebox-{id}");
        if path.exists() {
            let receipt = state::read_json(&path)?;
            let found = self.d(&["inspect", &name])?;
            ensure!(
                found[0]["Config"]["Labels"]["enginebox.sync.attempt"] == id
                    && found[0]["Image"] == self.config.image,
                "Foreign reconciliation environment"
            );
            ensure!(
                receipt["status"] == "ready",
                "Agent environment creation is uncertain; inspect it before continuing"
            );
        } else {
            let _admission = state::lock(&self.config.root.join(".lock"), false)?;
            self.space(0)?;
            ensure!(
                self.active_count()? < self.config.max_workspaces,
                "No resource slot for a reconciliation environment"
            );
            let found = self.docker(
                &[
                    "ps".into(),
                    "-a".into(),
                    format!("--filter=name=^{name}$"),
                    "--format={{.ID}}".into(),
                ],
                None,
                30,
            )?;
            ensure!(
                found.iter().all(u8::is_ascii_whitespace),
                "Reconciliation container already exists without its receipt"
            );
            state::write_json(
                &path,
                &json!({"status":"creating","attempt":id,"container":name}),
            )?;
            self.docker(
                &[
                    "run".into(),
                    "-d".into(),
                    "--name".into(),
                    name.clone(),
                    "--label".into(),
                    format!("enginebox.sync.attempt={id}"),
                    format!("--cpus={}", self.config.cpus_per_workspace),
                    format!("--memory={}m", self.config.memory_mib_per_workspace),
                    format!("--memory-swap={}m", self.config.memory_mib_per_workspace),
                    "--pids-limit=256".into(),
                    "--cap-drop=ALL".into(),
                    "--security-opt=no-new-privileges".into(),
                    "--read-only".into(),
                    "--user=0:0".into(),
                    "--tmpfs=/tmp:rw,nosuid,nodev,size=256m".into(),
                    "--tmpfs=/home/agent:rw,nosuid,nodev,size=256m".into(),
                    "--env=HOME=/home/agent".into(),
                    "--workdir=/workspace".into(),
                    "--mount".into(),
                    format!("type=bind,src={},dst=/workspace", worktree.display()),
                    self.config.image.clone(),
                    "sh".into(),
                    "-c".into(),
                    "command -v git >/dev/null && exec sleep infinity".into(),
                ],
                None,
                60,
            )?;
            let found = self.d(&["inspect", &name])?;
            ensure!(
                found[0]["State"]["Running"] == true,
                "Sync image must provide Git and a running reconciliation environment"
            );
            state::write_json(
                &path,
                &json!({"status":"ready","attempt":id,"container":name,"image":self.config.image}),
            )?;
        }
        let mut task = state::read_json(&directory.join("agent-task.json"))?;
        task["environment"] = json!({"kind":"docker","container":name,"workdir":"/workspace","push_credentials":false,"provider_auth":"assigned by the native-agent executor","completion":"Stop this exact reconciliation container before submitting the resolved result"});
        task.as_object_mut().unwrap().remove("worktree");
        Ok(task)
    }
    fn agent_stopped(&self, outbox: &Outbox, id: &str) -> Result<()> {
        let path = outbox
            .worktree(id)?
            .parent()
            .context("Attempt directory")?
            .join("agent-environment.json");
        if path.exists() {
            let receipt = state::read_json(&path)?;
            let container = receipt["container"]
                .as_str()
                .context("Agent container missing")?;
            let found = self.d(&["inspect", container])?;
            ensure!(
                found[0]["Config"]["Labels"]["enginebox.sync.attempt"] == id
                    && found[0]["State"]["Running"] == false,
                "The exact reconciliation environment must be stopped before accepting agent files"
            );
        }
        Ok(())
    }
    pub fn github_delivery(
        &self,
        project: &str,
        action: &str,
        id: Option<&str>,
        token: Option<&Path>,
    ) -> Result<Value> {
        self.verify_binding()?;
        let (_, p, outbox, binding) = self.github_binding(project)?;
        let _lease = self.main_lock(&p)?;
        let id = if action == "export" {
            let revision = self.main_revision(&p)?;
            let sequence = self.publication_sequence(&p)?;
            ensure!(
                sequence > 0,
                "Only accepted main promotions can be exported"
            );
            let files = self.export_main(&p, &revision)?;
            let status = outbox.enqueue(sequence, &revision, &files)?;
            status["active"]["id"]
                .as_str()
                .context("Enable outbound sync before exporting")?
                .to_owned()
        } else {
            id.context("Sync attempt ID required")?.to_owned()
        };
        match action {
            "resolve" => {
                self.agent_stopped(&outbox, &id)?;
                outbox.accept(&id, |a, files| self.accept_resolution(&p, a, files))?;
            }
            "retry" => {
                let status = outbox.retry(&id)?;
                return self.drive_sync(
                    &p,
                    &outbox,
                    &binding,
                    status["active"]["id"].as_str().unwrap(),
                    token,
                );
            }
            "agent-task" => {
                ensure!(
                    outbox.read(&id)?.status == Status::NeedsAgent,
                    "No conflict agent is needed for this attempt"
                );
                return self.agent_environment(&outbox, &id);
            }
            "export" | "deliver" => (),
            _ => anyhow::bail!("Unsupported GitHub delivery action"),
        }
        self.drive_sync(&p, &outbox, &binding, &id, token)
    }
}
