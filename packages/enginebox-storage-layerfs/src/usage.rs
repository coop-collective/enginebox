use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

const COLLECTOR: &str = include_str!("usage.mjs");
const TTL_MS: u64 = 300_000;
const MAX_PROJECTS: usize = 32;

fn unavailable() -> Value {
    json!({"status":"unavailable","apparent_bytes":null,"allocated_bytes":null,"files":null})
}
fn fresh(value: &Value, now: u64) -> bool {
    value["schema"] == 1
        && value["observed_at_unix_ms"]
            .as_u64()
            .is_some_and(|at| now >= at && now - at < TTL_MS)
}
fn local_volume(value: &Value, name: &str, owner: Option<&str>) -> bool {
    value["Name"] == name
        && value["Driver"] == "local"
        && value["Options"].as_object().is_none_or(|o| o.is_empty())
        && owner.is_none_or(|owner| {
            value["Labels"]["enginebox.layerfs.owner"] == owner
                && value["Labels"]["enginebox.layerfs.revision"] == crate::REVISION
        })
}

impl Runtime {
    /// Best-effort metadata collection cannot fail inventory or change a Store.
    /// Concurrent inventory calls reuse the last receipt, including failures.
    pub(super) fn storage_usage(&self) -> Value {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let empty = json!({"schema":1,"observed_at_unix_ms":now,"status":"unavailable","shared_caches":[],"projects":[]});
        if self.verify_binding().is_err() {
            return empty;
        }
        let receipt = self.config.root.join("storage-usage.json");
        let old = state::read_json(&receipt).ok();
        if let Some(value) = old.as_ref().filter(|v| fresh(v, now)) {
            return value.clone();
        }
        let Ok(_lock) = state::lock(&self.config.root.join("storage-usage.lock"), false) else {
            return old.unwrap_or(empty);
        };
        let value = self.collect_usage(now).unwrap_or(empty);
        let _ = state::write_json(&receipt, &value);
        value
    }

    fn collect_usage(&self, now: u64) -> Result<Value> {
        let mut targets: Vec<(Value, String, Option<String>)> = Vec::new();
        for (kind, volume) in [
            ("pnpm-store", &self.config.pnpm_store),
            ("pnpm-metadata", &self.config.pnpm_metadata),
        ] {
            if let Some(volume) = volume {
                targets.push((json!({"kind":kind}), volume.clone(), None));
            }
        }
        // Project receipts live inside each project's directory.
        let mut projects = fs::read_dir(self.config.root.join("projects"))?
            .filter_map(|entry| entry.ok().and_then(|e| e.file_name().into_string().ok()))
            .collect::<Vec<_>>();
        projects.sort();
        let mut truncated = projects.len() > MAX_PROJECTS;
        for name in projects.iter().take(MAX_PROJECTS) {
            if let Ok(p) = self.project(name) {
                targets.push((json!({"project":p.name}), p.volume, Some(p.owner)));
            } else {
                truncated = true;
            }
        }
        let mut result = json!({"schema":1,"observed_at_unix_ms":now,"status":"complete","shared_caches":[],"projects":[],"projects_truncated":truncated});
        if truncated {
            result["status"] = json!("partial");
        }
        if targets.is_empty() {
            return Ok(result);
        }
        // Inspect exact existing volumes first. Never implicitly create a missing
        // volume, or mount remote/plugin storage or a foreign project Store.
        let mut inspect = vec!["volume".into(), "inspect".into()];
        inspect.extend(targets.iter().map(|(_, name, _)| name.clone()));
        let inspected: Vec<Value> = self
            .docker(&inspect, None, 5)
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default();
        let mut args: Vec<String> = [
            "run",
            "--rm",
            "--pull=never",
            "--network=none",
            "--read-only",
            "--cap-drop=ALL",
            "--cap-add=DAC_OVERRIDE",
            "--security-opt=no-new-privileges",
            "--cpus=0.25",
            "--memory=192m",
            "--pids-limit=32",
            "--entrypoint=timeout",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let mut mounted = Vec::new();
        for (i, (_, name, owner)) in targets.iter().enumerate() {
            if inspected
                .iter()
                .any(|v| local_volume(v, name, owner.as_deref()))
            {
                args.extend([
                    "--mount".into(),
                    format!("type=volume,src={name},dst=/usage/{i},readonly,volume-nocopy"),
                ]);
                mounted.push(i);
            }
        }
        args.push("-i".into());
        args.extend([
            self.config.image.clone(),
            "--kill-after=2s".into(),
            "8s".into(),
            "node".into(),
            "--input-type=module".into(),
            "--eval".into(),
            COLLECTOR.into(),
        ]);
        let roots = mounted
            .iter()
            .map(|i| format!("/usage/{i}"))
            .collect::<Vec<_>>();
        let values: Vec<Value> = if roots.is_empty() {
            vec![]
        } else {
            self.docker(&args, Some(&serde_json::to_vec(&roots)?), 12)
                .ok()
                .and_then(|raw| serde_json::from_slice(&raw).ok())
                .unwrap_or_default()
        };
        for (i, (mut identity, _, _)) in targets.into_iter().enumerate() {
            let observation = mounted
                .iter()
                .position(|m| *m == i)
                .and_then(|j| values.get(j))
                .cloned()
                .unwrap_or_else(unavailable);
            identity.as_object_mut().unwrap().extend(
                observation
                    .as_object()
                    .context("Invalid usage observation")?
                    .clone(),
            );
            if identity["status"] != "complete" {
                result["status"] = json!("partial");
            }
            let field = if identity["kind"].is_string() {
                "shared_caches"
            } else {
                "projects"
            };
            result[field].as_array_mut().unwrap().push(identity);
        }
        if truncated {
            result["status"] = json!("partial");
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn usage_receipts_expire_and_do_not_accept_future_observations() {
        let receipt = json!({"schema":1,"observed_at_unix_ms":1234});
        assert!(fresh(&receipt, 1234));
        assert!(!fresh(&receipt, 1233));
        assert!(!fresh(&receipt, 1234 + TTL_MS));
    }
    #[test]
    fn usage_refuses_foreign_remote_or_replaced_volumes() {
        let volume = json!({"Name":"owned","Driver":"local","Options":null,"Labels":{"enginebox.layerfs.owner":"owner","enginebox.layerfs.revision":crate::REVISION}});
        assert!(local_volume(&volume, "owned", Some("owner")));
        assert!(!local_volume(&volume, "other", Some("owner")));
        assert!(!local_volume(&volume, "owned", Some("foreign")));
        let mut remote = volume.clone();
        remote["Options"] = json!({"type":"nfs"});
        assert!(!local_volume(&remote, "owned", None));
    }

    #[test]
    #[ignore = "requires explicit local Docker socket and existing Node image; creates disposable volume"]
    fn disposable_docker_usage_and_cached_receipt() -> Result<()> {
        let socket = PathBuf::from(std::env::var("ENGINEBOX_USAGE_TEST_SOCKET")?);
        let image = std::env::var("ENGINEBOX_USAGE_TEST_IMAGE")?;
        ensure!(
            image.starts_with("sha256:"),
            "Use an existing immutable image ID"
        );
        let root = tempfile::tempdir()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700))?;
        }
        let mut config: Config = serde_json::from_value(json!({
            "schema":1,"root":root.path().canonicalize()?,"docker_socket":socket,
            "sdk":"/unused/sdk","daemon":"/unused/daemon","image":image,
            "native_sha256":"0".repeat(64),"daemon_sha256":"0".repeat(64),
            "max_workspaces":1,"cpus_per_workspace":1,"memory_mib_per_workspace":1024,
            "disk_reserve_gib":1,"import_limit_gib":1,"pnpm_store":null,"pnpm_metadata":null
        }))?;
        let owner = uuid::Uuid::new_v4().simple().to_string();
        let volume = format!("ebox-lfs-{owner}");
        config.pnpm_store = Some(volume.clone());
        let runtime = Runtime { config };
        runtime.docker(
            &[
                "volume".into(),
                "create".into(),
                "--label".into(),
                format!("enginebox.layerfs.owner={owner}"),
                "--label".into(),
                format!("enginebox.layerfs.revision={}", crate::REVISION),
                volume.clone(),
            ],
            None,
            10,
        )?;
        let check = (|| -> Result<()> {
            runtime.docker(&["run".into(),"--rm".into(),"--network=none".into(),"--mount".into(),format!("type=volume,src={volume},dst=/fixture"),"--entrypoint=node".into(),runtime.config.image.clone(),"-e".into(),
                "const fs=require('fs');fs.writeFileSync('/fixture/data','fixture');fs.linkSync('/fixture/data','/fixture/link');fs.chownSync('/fixture/data',501,20);fs.chmodSync('/fixture/data',0o600);fs.chownSync('/fixture',501,20);fs.chmodSync('/fixture',0o700)".into()], None, 15)?;
            state::private_dir(&runtime.config.root.join("projects"))?;
            state::private_dir(&runtime.project_dir("sample"))?;
            state::write_json(
                &runtime.config.root.join("config.json"),
                &json!({"schema":1,"revision":crate::REVISION,"config":runtime.config}),
            )?;
            fs::write(runtime.config.root.join("bridge.mjs"), BRIDGE)?;
            state::write_json(
                &runtime.project_dir("sample").join("project.json"),
                &json!({"schema":1,"name":"sample","owner":owner,"volume":volume,"status":"ready","commit":"fixture","approval":"fixture"}),
            )?;
            runtime.verify_binding()?;
            let usage = runtime.storage_usage();
            ensure!(usage["status"] == "complete", "{usage}");
            ensure!(
                usage["projects"][0]["apparent_bytes"] == 7,
                "Incorrect source size"
            );
            ensure!(
                usage["projects"][0]["files"] == 1,
                "Hardlinks not deduplicated"
            );
            ensure!(
                usage["shared_caches"][0]["apparent_bytes"] == 7,
                "Cache not observed"
            );
            ensure!(
                usage["projects"][0]["allocated_bytes"]
                    .as_u64()
                    .is_some_and(|n| n > 0),
                "Blocks not measured"
            );
            // A cached observation survives unavailable Docker and never rescans.
            let mut cached = runtime.config.clone();
            cached.docker_socket = PathBuf::from("/nonexistent/socket");
            state::write_json(
                &runtime.config.root.join("config.json"),
                &json!({"schema":1,"revision":crate::REVISION,"config":cached}),
            )?;
            ensure!(
                Runtime { config: cached }.storage_usage() == usage,
                "Cached receipt not reused"
            );
            Ok(())
        })();
        let cleanup = runtime.docker(&["volume".into(), "rm".into(), volume], None, 10);
        check?;
        cleanup?;
        Ok(())
    }
}
