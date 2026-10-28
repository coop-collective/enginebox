use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) const KINDS: [&str; 5] = ["test", "build", "dependencies", "dev", "other"];

fn identity(w: &Workspace) -> Value {
    json!({"project":w.project,"sandbox":w.sandbox})
}
fn matching(value: &Value, w: &Workspace) -> bool {
    value["schema"] == 1 && value["identity"] == identity(w)
}

impl Runtime {
    pub(super) fn command_timings(&self, w: &Workspace) -> Vec<Value> {
        if identifier(&w.name).is_err() {
            return vec![];
        }
        let path = self
            .config
            .root
            .join("command-timings")
            .join(format!("{}.json", w.name));
        state::read_json(&path)
            .ok()
            .filter(|v| matching(v, w))
            .and_then(|v| v["receipts"].as_object().cloned())
            .map(|receipts| {
                KINDS
                    .iter()
                    .filter_map(|kind| receipts.get(*kind).cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(super) fn record_command(
        &self,
        w: &Workspace,
        kind: &str,
        status: &str,
        elapsed_ms: Option<f64>,
    ) -> Result<()> {
        ensure!(KINDS.contains(&kind), "Invalid measurement category");
        let dir = self.config.root.join("command-timings");
        state::private_dir(&dir)?;
        let path = dir.join(format!("{}.json", w.name));
        let mut value = state::read_json(&path)
            .ok()
            .filter(|v| matching(v, w))
            .unwrap_or_else(|| json!({"schema":1,"identity":identity(w),"receipts":{}}));
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
        value["receipts"][kind] =
            json!({"kind":kind,"status":status,"elapsed_ms":elapsed_ms,"observed_at_unix_ms":now});
        state::write_json(&path, &value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn command_receipts_are_bounded_and_tied_to_the_exact_checkout() -> Result<()> {
        let root = tempfile::tempdir()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700))?;
        }
        let config: Config = serde_json::from_value(
            json!({"schema":1,"root":root.path(),"sdk":"/unused/sdk","daemon":"/unused/daemon","image":"sha256:fixture","native_sha256":"fixture","daemon_sha256":"fixture","max_workspaces":1,"cpus_per_workspace":1,"memory_mib_per_workspace":1024,"disk_reserve_gib":1,"import_limit_gib":1,"pnpm_store":null,"pnpm_metadata":null}),
        )?;
        let runtime = Runtime { config };
        let mut w: Workspace = serde_json::from_value(
            json!({"schema":2,"name":"sample","project":"project","sandbox":"original","branch":"branch","container":null,"mount":null,"status":"ready","last_error":null,"last_checkpoint":null}),
        )?;
        runtime.record_command(&w, "test", "succeeded", Some(12.5))?;
        runtime.record_command(&w, "build", "failed", Some(9.0))?;
        runtime.record_command(&w, "test", "uncertain", None)?;
        let receipts = runtime.command_timings(&w);
        assert_eq!(receipts.len(), 2);
        assert_eq!(receipts[0]["status"], "uncertain");
        assert!(receipts[0]["elapsed_ms"].is_null());
        assert_eq!(receipts[1]["elapsed_ms"], 9.0);
        assert!(
            runtime
                .record_command(&w, "arbitrary-command", "succeeded", Some(1.0))
                .is_err()
        );
        assert!(!serde_json::to_string(&receipts)?.contains("original"));
        w.sandbox = "replacement".into();
        assert!(runtime.command_timings(&w).is_empty());
        runtime.record_command(&w, "test", "succeeded", Some(1.0))?;
        assert_eq!(runtime.command_timings(&w).len(), 1);
        Ok(())
    }
}
