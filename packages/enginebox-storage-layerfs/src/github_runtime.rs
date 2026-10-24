use super::*;
use crate::github::Repositories;
use std::path::Path;

impl Runtime {
    fn github_repositories(&self) -> Result<Repositories> {
        self.root()?;
        let _guard = state::lock(&self.config.root.join(".lock"), false)?;
        self.bind_config()?;
        Repositories::new(&self.config.root)
    }
    pub fn github_preview(
        &self,
        project: &str,
        repository: &str,
        branch: Option<&str>,
        token_file: Option<&Path>,
    ) -> Result<Value> {
        let repos = self.github_repositories()?;
        self.space(0)?;
        repos.preview(
            project,
            repository,
            branch,
            token_file,
            self.config.import_limit_gib * enginebox_core::GIB,
        )
    }
    pub fn github_import(&self, project: &str, revision: &str, approval: &str) -> Result<Value> {
        self.github_repositories()?.import(project, revision, approval, |generation, repo, commit, digest| {
            if self.project_dir(generation).exists() {
                // Recovery of a completed import whose pointer write failed is
                // safe. Partial/uncertain imports must never be replayed.
                let p = self.project(generation)?;
                ensure!(p.status == "ready" && p.commit == commit && p.approval == digest,
                    "Retained GitHub generation is not a proved complete import; inspect it on the node");
                self.owned_volume(&p)?;
            } else {
                self.import(generation, repo, commit, digest)?;
            }
            Ok(())
        })
    }
    pub fn github_fork(&self, project: &str, name: &str, from: Option<&str>) -> Result<Value> {
        let generation = self.github_repositories()?.current(project)?;
        // Fork inside the imported project's LayerFS history. Later main
        // publications do not rewrite the files of already-created branches.
        let mut result = self.fork(&generation.project, name, from)?;
        result["github_project"] = json!(project);
        result["source_commit"] = json!(generation.commit);
        Ok(result)
    }
}
