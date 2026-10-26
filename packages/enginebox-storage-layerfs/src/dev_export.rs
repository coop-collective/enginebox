//! Immutable source snapshots for the single-owner development service.
//! These are not Cloudflare accepted Store generations or database backups.
use super::*;
use sha2::{Digest, Sha256};
use std::path::Path;

fn inventory(root: &Path) -> Result<Vec<Value>> {
    fn walk(root: &Path, relative: &Path, out: &mut Vec<Value>) -> Result<()> {
        let mut entries =
            fs::read_dir(root.join(relative))?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = relative.join(entry.file_name());
            let name = path.to_str().context("Source filename must be UTF-8")?;
            source::allowed_path(name)?;
            let m = fs::symlink_metadata(entry.path())?;
            if m.is_dir() {
                walk(root, &path, out)?;
            } else if m.file_type().is_symlink() {
                let target = fs::read_link(entry.path())?;
                source::safe_link(name, target.to_str().context("Symlink must be UTF-8")?)?;
                ensure!(
                    entry.path().canonicalize()?.starts_with(root),
                    "Source symlink chain escapes snapshot"
                );
                out.push(json!({"kind":"symlink","path":name,"target":target}));
            } else {
                ensure!(m.is_file(), "Special file in development snapshot");
                let mut file = fs::File::open(entry.path())?;
                let mut hash = Sha256::new();
                std::io::copy(&mut file, &mut hash)?;
                #[cfg(unix)]
                let executable = {
                    use std::os::unix::fs::PermissionsExt;
                    m.permissions().mode() & 0o111 != 0
                };
                #[cfg(not(unix))]
                let executable = false;
                out.push(json!({"kind":"file","path":name,"size":m.len(),"sha256":format!("{:x}", hash.finalize()),"executable":executable}));
            }
            ensure!(
                out.len() <= 100000,
                "Development snapshot entry limit exceeded"
            );
        }
        Ok(())
    }
    let mut entries = vec![];
    walk(root, Path::new(""), &mut entries)?;
    Ok(entries)
}

impl Runtime {
    /// A bounded preview at a command boundary, not a checkpoint or acceptance.
    /// The writer lease and kernel census exclude both scheduled and orphaned
    /// writers. Attached guests remain unsupported: their exclusive custody and
    /// page-cache semantics must not be bypassed with a host-side copy.
    pub fn workspace_export(&self, project: &str, name: &str) -> Result<Value> {
        identifier(project)?;
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
        )
        .context("workspace-writer-busy")?;
        crate::incus::ensure_detached(&self.config.root, name)
            .context("workspace-attached-unqualified")?;
        let w = self.workspace(name)?;
        let internal = if self
            .config
            .root
            .join("github")
            .join(project)
            .join("repository.json")
            .exists()
        {
            crate::github::Repositories::new(&self.config.root)?
                .current(project)?
                .project
        } else {
            project.into()
        };
        ensure!(w.project == internal, "workspace-project-mismatch");
        ensure!(w.status == "ready", "workspace-writer-busy");
        let p = self.project(&internal)?;
        self.owned_volume(&p)?;
        self.verify_container(
            &p,
            &w.sandbox,
            w.container.as_deref().context("Container missing")?,
        )?;
        self.probe(&w).context("workspace-writer-busy")?;
        let directory = self
            .project_dir(&internal)
            .join("workspace-exports")
            .join(uuid::Uuid::new_v4().simple().to_string());
        state::private_dir(&directory)?;
        let files = directory.join("source");
        state::private_dir(&files)?;
        // No native command can begin while this lease is held. The copy runs
        // inside the verified mount namespace; Docker cp alone misses FUSE.
        self.export_workspace_files(&w, &files)?;
        self.probe(&w).context("workspace-writer-busy")?;
        let entries = inventory(&files)?;
        let tree_hash = state::digest(&json!(entries));
        let revision = state::digest(&json!({"schema":1,"project":project,"workspace":name,
            "sandbox":w.sandbox,"branch":w.branch,"treeHash":tree_hash}));
        let manifest = json!({"schema":1,"authority":"local-layerfs-workspace","project":project,
            "workspace":name,"revision":revision,"branch":w.branch,"sandbox":w.sandbox,
            "consistency":"exclusive-command-boundary","treeHash":tree_hash,"entries":entries});
        state::write_json(&directory.join("source.json"), &manifest)?;
        let retained = directory.parent().context("Export parent")?.join(&revision);
        if retained.exists() {
            ensure!(
                state::read_json(&retained.join("source.json"))? == manifest
                    && inventory(&retained.join("source"))? == entries,
                "Retained workspace export failed integrity check"
            );
            fs::remove_dir_all(&directory)?;
        } else {
            fs::rename(&directory, &retained)?;
        }
        Ok(json!({"revision":revision,"sourceRoot":retained.join("source"),"manifest":manifest}))
    }

    /// Only a stable closed checkpoint or the owned main can supply a preview.
    pub fn dev_export(&self, project: &str, workspace: Option<&str>) -> Result<Value> {
        identifier(project)?;
        self.verify_binding()?;
        let internal = if self
            .config
            .root
            .join("github")
            .join(project)
            .join("repository.json")
            .exists()
        {
            crate::github::Repositories::new(&self.config.root)?
                .current(project)?
                .project
        } else {
            project.into()
        };
        let p = self.project(&internal)?;
        let _main = self.main_lock(&p)?;
        let branch = if let Some(name) = workspace {
            identifier(name)?;
            let w = self.workspace(name)?;
            crate::incus::ensure_detached(&self.config.root, name)?;
            ensure!(
                w.project == internal && w.status == "closed" && w.last_checkpoint.is_some(),
                "Preview requires a closed checkpoint in this project"
            );
            w.branch
        } else {
            "main".into()
        };
        let head = self.bridge(&p, "project-head", json!({"branch":branch}), 30)?;
        let revision = state::digest(
            &json!({"export_schema":2,"project":internal,"branch":branch,"head":head}),
        );
        let directory = self
            .project_dir(&internal)
            .join("dev-exports")
            .join(&revision);
        if directory.join("source.json").exists() {
            let manifest = state::read_json(&directory.join("source.json"))?;
            ensure!(
                manifest["revision"] == revision
                    && manifest["entries"] == json!(inventory(&directory.join("source"))?),
                "Retained development snapshot failed integrity check"
            );
            return Ok(
                json!({"revision":revision,"sourceRoot":directory.join("source"),"manifest":manifest}),
            );
        }
        ensure!(
            !directory.exists(),
            "Interrupted development export requires inspection"
        );
        state::private_dir(&directory)?;
        state::write_json(
            &directory.join("pending.json"),
            &json!({"project":project,"branch":branch,"head":head}),
        )?;
        let name = format!("dev-{}", uuid::Uuid::new_v4().simple());
        self.fork(&internal, &name, workspace)?;
        let w = self.workspace(&name)?;
        let files = directory.join("source");
        state::private_dir(&files)?;
        self.export_workspace_files(&w, &files)?;
        self.operate(&name, "close", &[], 60, "")?;
        ensure!(
            self.bridge(&p, "project-head", json!({"branch":branch}), 30)? == head,
            "Source advanced during development export"
        );
        let entries = inventory(&files)?;
        let manifest = json!({"schema":1,"authority":"local-layerfs","project":project,"workspace":workspace,"revision":revision,"branch":branch,"head":head,"treeHash":state::digest(&json!(entries)),"entries":entries});
        state::write_json(&directory.join("source.json"), &manifest)?;
        Ok(json!({"revision":revision,"sourceRoot":files,"manifest":manifest}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inventory_binds_bytes_modes_and_refuses_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        fs::write(root.join("app.txt"), "one").unwrap();
        let first = inventory(&root).unwrap();
        fs::write(root.join("app.txt"), "two").unwrap();
        assert_ne!(first, inventory(&root).unwrap());
        fs::write(root.join(".env"), "private").unwrap();
        assert!(inventory(&root).is_err());
    }
}
