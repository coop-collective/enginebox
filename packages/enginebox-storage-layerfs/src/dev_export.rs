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
