//! Logical same-Store integration. Immutable refs are merged by the pinned SDK;
//! SQLite is only backed up after the merge, never used as a merge format.
use super::*;
use sha2::{Digest, Sha256};
use std::{io::Read, path::Path};

fn immutable_ref(value: &str) -> bool {
    value.len() == 66
        && (value.starts_with("12") || value.starts_with("32"))
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn entries(
    root: &Path,
    dir: &Path,
    output: &mut Vec<Value>,
    bytes: &mut u64,
    limit: u64,
) -> Result<()> {
    for item in fs::read_dir(dir)? {
        let path = item?.path();
        let relative = path
            .strip_prefix(root)?
            .to_str()
            .context("Non UTF-8 source path")?
            .replace('\\', "/");
        source::allowed_path(&relative)?;
        ensure!(
            !relative.split('/').any(|s| matches!(
                s,
                "node_modules"
                    | ".pnpm-store"
                    | "Library"
                    | "DerivedDataCache"
                    | "Intermediate"
                    | ".godot"
            )),
            "Generated cache path cannot enter frozen source: {relative}"
        );
        let meta = fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            entries(root, &path, output, bytes, limit)?;
        } else if meta.file_type().is_symlink() {
            let target = fs::read_link(&path)?;
            let target = target.to_str().context("Non UTF-8 symlink")?;
            source::safe_link(&relative, target)?;
            ensure!(
                fs::canonicalize(&path)?.starts_with(root),
                "Symlink chain leaves source"
            );
            output.push(json!({"path":relative,"kind":"symlink","target":target}));
        } else {
            ensure!(meta.is_file(), "Source contains a special file");
            *bytes = bytes
                .checked_add(meta.len())
                .context("Source size overflow")?;
            ensure!(*bytes <= limit, "Source exceeds configured export size");
            let mut file = fs::File::open(&path)?;
            let mut hash = Sha256::new();
            let mut buffer = [0u8; 128 * 1024];
            loop {
                let n = file.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                hash.update(&buffer[..n]);
            }
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                meta.permissions().mode() & 0o111 != 0
            };
            #[cfg(not(unix))]
            let executable = false;
            output.push(json!({"path":relative,"kind":"file","sha256":format!("{:x}",hash.finalize()),"size":meta.len(),"executable":executable}));
        }
        ensure!(output.len() <= 1_000_000, "Source inventory too large");
    }
    Ok(())
}
impl Runtime {
    /// Freeze a branch or immutable ref and back up the Store for publication.
    /// This never publishes the branch to LayerFS main or accepts a fleet head.
    pub fn snapshot(&self, project: &str, name: &str, reference: &str) -> Result<Value> {
        identifier(project)?;
        identifier(name)?;
        ensure!(
            reference == "main" || immutable_ref(reference),
            "Snapshot requires main or an immutable reference"
        );
        self.root()?;
        self.bind_config()?;
        self.space(0)?;
        let p = self.project(project)?;
        let directory = self.config.root.join("snapshots");
        state::private_dir(&directory)?;
        let _guard = state::lock(&directory.join(format!("{name}.lock")), false)?;
        let receipt = directory.join(format!("{name}.json"));
        ensure!(
            !receipt.exists(),
            "Snapshot name has a receipt; inspect it before retrying"
        );
        state::write_json(
            &receipt,
            &json!({"project":project,"reference":reference,"status":"running"}),
        )?;
        let result = (|| {
            let mut result=self.bridge(&p,"inspect",json!({"reference":reference,"inspectPrefix":format!("s-{}-",uuid::Uuid::new_v4().simple())}),120)?;
            let snapshot = directory.join(format!("{name}.sqlite"));
            self.bridge(&p, "snapshot", json!({"output":snapshot}), 600)?;
            fs::OpenOptions::new()
                .write(true)
                .open(&snapshot)?
                .sync_all()?;
            result["snapshot"] = json!(snapshot);
            result["layerfsRevision"] = json!(crate::REVISION);
            result["projectName"] = json!(project);
            Ok(result)
        })();
        state::write_json(
            &receipt,
            &json!({"project":project,"reference":reference,"status":if result.is_ok(){"completed"}else{"uncertain"},"result":result.as_ref().ok(),"error":result.as_ref().err().map(|e|format!("{e:#}"))}),
        )?;
        result
    }
    pub fn integrate(&self, request: &Path) -> Result<Value> {
        real_path(request)?;
        ensure!(
            fs::metadata(request)?.len() <= 256 * 1024,
            "Integration request too large"
        );
        let input = state::read_json(request)?;
        let id = input["id"].as_str().context("Integration ID required")?;
        identifier(id)?;
        let fleet_project = input["project"].as_str().context("Project required")?;
        identifier(fleet_project)?;
        let generation = input["generation"]
            .as_str()
            .context("Base generation required")?;
        ensure!(crate::config::digest(generation), "Invalid base generation");
        let plan = &input["integration"];
        let project = plan["layerfsProject"]
            .as_str()
            .context("LayerFS project required")?;
        identifier(project)?;
        let base_ref = plan["baseRef"]
            .as_str()
            .context("Immutable base required")?;
        ensure!(immutable_ref(base_ref), "Immutable base required");
        ensure!(
            plan["baseRoot"].as_str().is_some_and(crate::config::digest),
            "Invalid base root"
        );
        let changes = plan["changes"].as_array().context("Changes required")?;
        ensure!(
            !changes.is_empty() && changes.len() <= 128,
            "Integration requires 1–128 changes"
        );
        for change in changes {
            ensure!(
                change["ref"].as_str().is_some_and(immutable_ref)
                    && change["root"].as_str().is_some_and(crate::config::digest),
                "Invalid change reference"
            );
        }
        self.root()?;
        self.bind_config()?;
        let integration_root = self.config.root.join("integrations");
        state::private_dir(&integration_root)?;
        let integrations = integration_root.join(fleet_project);
        state::private_dir(&integrations)?;
        let _guard = state::lock(&integrations.join(format!("{id}.lock")), false)?;
        let receipt = integrations.join(format!("{id}.json"));
        if receipt.exists() {
            let prior = state::read_json(&receipt)?;
            ensure!(
                prior["request"] == input,
                "Integration ID already has another request"
            );
            ensure!(
                prior["status"] == "completed",
                "Prior integration is uncertain or failed; inspect its private branches and workspace, never replay automatically"
            );
            return Ok(prior["result"].clone());
        }
        self.space(0)?;
        ensure!(
            self.active_count()? < self.config.max_workspaces,
            "No workspace capacity for integration"
        );
        let p = self.project(project)?;
        ensure!(
            p.source_only,
            "Logical integration requires a source-only project; import with --source-only into a new Store"
        );
        let output = integrations.join(id);
        state::private_dir(&output)?;
        let identity = format!("{:x}", Sha256::digest(format!("{fleet_project}/{id}")));
        let workspace = format!("i-{}", &identity[..32]);
        state::write_json(
            &receipt,
            &json!({"request":input,"status":"running","workspace":workspace}),
        )?;
        let outcome = (|| {
            let mut merge = plan.clone();
            merge["prefix"] = json!(format!("{workspace}-"));
            let merged = self.bridge(&p, "integrate", json!({"integration":merge}), 600)?;
            if merged["ok"] == false && merged["conflict"].is_object() {
                return Ok(merged);
            }
            let result_ref = merged["ref"]
                .as_str()
                .context("Merge returned no immutable reference")?;
            ensure!(immutable_ref(result_ref), "Invalid merge reference");
            self.fork_at(project, &workspace, None, Some(result_ref))?;
            let w = self.workspace(&workspace)?;
            let source_root = output.join("source");
            state::private_dir(&source_root)?;
            // Docker cp reads the container rootfs and can silently omit a
            // dynamically mounted FUSE tree. Archive from inside its mount
            // namespace first. This private workspace is never handed to an
            // agent; trusted tar walks an immutable tree without dereferencing
            // links, so it cannot manufacture parent/absolute archive entries.
            let container = w.container.as_deref().context("No export sandbox")?;
            let mount = w.mount.as_deref().context("No export mount")?;
            let archive_name = format!("/ebox-export-{}.tar", uuid::Uuid::new_v4().simple());
            self.docker(
                &[
                    "exec".into(),
                    "--user=0".into(),
                    container.into(),
                    "timeout".into(),
                    "600s".into(),
                    "tar".into(),
                    "-C".into(),
                    mount.into(),
                    "-cf".into(),
                    archive_name.clone(),
                    ".".into(),
                ],
                None,
                610,
            )?;
            let archive = output.join("source.tar");
            self.docker(
                &[
                    "cp".into(),
                    format!("{container}:{archive_name}"),
                    archive.to_string_lossy().into(),
                ],
                None,
                600,
            )?;
            ensure!(
                fs::metadata(&archive)?.len()
                    <= self.config.import_limit_gib * enginebox_core::GIB * 2 + 32 * 1024 * 1024,
                "Export archive exceeds source budget"
            );
            run_process(
                &[
                    "tar".into(),
                    "--extract".into(),
                    "--file".into(),
                    archive.to_string_lossy().into(),
                    "--directory".into(),
                    source_root.to_string_lossy().into(),
                    "--no-same-owner".into(),
                    "--no-same-permissions".into(),
                    "--keep-old-files".into(),
                ],
                None,
                Duration::from_secs(600),
            )?;
            fs::remove_file(&archive)?;
            let mut inventory = Vec::new();
            let mut bytes = 0;
            entries(
                &source_root,
                &source_root,
                &mut inventory,
                &mut bytes,
                self.config.import_limit_gib * enginebox_core::GIB,
            )?;
            inventory.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
            let tree_hash = format!("{:x}", Sha256::digest(serde_json::to_vec(&inventory)?));
            let manifest = output.join("source.json");
            state::write_json(
                &manifest,
                &json!({"schema":1,"project":fleet_project,"generation":generation,"layerfs":{"revision":crate::REVISION,"project":project,"ref":result_ref,"root":merged["root"]},"treeHash":tree_hash,"entries":inventory}),
            )?;
            let snapshot = output.join("store.sqlite");
            self.bridge(&p, "snapshot", json!({"output":snapshot}), 600)?;
            fs::OpenOptions::new()
                .write(true)
                .open(&snapshot)?
                .sync_all()?;
            fs::File::open(&output)?.sync_all()?;
            self.operate(&workspace, "close", &[], 120, "")?;
            Ok(
                json!({"snapshot":snapshot,"sourceRoot":source_root,"sourceManifest":manifest,"ref":result_ref,"root":merged["root"],"treeHash":tree_hash,"layerfsRevision":crate::REVISION}),
            )
        })();
        state::write_json(
            &receipt,
            &json!({"request":input,"workspace":workspace,"status":if outcome.is_ok(){"completed"}else{"uncertain"},"result":outcome.as_ref().ok(),"error":outcome.as_ref().err().map(|e|format!("{e:#}"))}),
        )?;
        outcome
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frozen_source_hash_and_escape() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("a.txt"), b"hello").unwrap();
        let mut files = vec![];
        let mut bytes = 0;
        entries(root.path(), root.path(), &mut files, &mut bytes, 10).unwrap();
        assert_eq!(bytes, 5);
        assert_eq!(
            files[0]["sha256"],
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        fs::create_dir(root.path().join(".git")).unwrap();
        assert!(entries(root.path(), root.path(), &mut files, &mut bytes, 10).is_err());
    }
    #[test]
    fn rejects_branch_names() {
        assert!(!immutable_ref("main"));
        assert!(!immutable_ref("../a"));
        assert!(immutable_ref(&format!("12{}", "a".repeat(64))));
    }
}
