//! Controller-owned registrations and bounded node-local checkout operations.
//! Paths and executables come from the private administrator registry, never HTTP/MCP.
use anyhow::{Context, Result, ensure};
use enginebox_core::{Fleet, Host, state, transport::host_run};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path, sync::Arc, time::Duration};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registry {
    schema_version: u32,
    nodes: Vec<Node>,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Node {
    host_id: String,
    executable: String,
    config: String,
    projects: Vec<Project>,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Project {
    id: String,
    repository: String,
}

type Runner = Arc<dyn Fn(&Host, &[String], Duration) -> Result<Value> + Send + Sync>;
#[derive(Clone)]
pub struct Storage {
    registry: Registry,
    fleet: Fleet,
    runner: Runner,
}
pub struct Plan {
    host: Host,
    pub operation: String,
    pub name: String,
    argv: Vec<String>,
    timeout: Duration,
    runner: Runner,
}
impl Plan {
    pub fn execute(self) -> Result<Value> {
        let result = (self.runner)(&self.host, &self.argv, self.timeout)?;
        ensure!(
            result.is_object(),
            "Storage node returned an invalid result"
        );
        ensure!(result["ok"] != false, "Storage operation reported failure");
        ensure!(
            serde_json::to_vec(&result)?.len() <= 3 * 1024 * 1024,
            "Storage result exceeds the controller limit; inspect it directly on the node"
        );
        Ok(
            json!({"backend":"layerfs","host_id":self.host.id,"operation":self.operation,"result":result}),
        )
    }
}

fn identifier(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 48
            && value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !value.starts_with('-')
            && !value.ends_with('-'),
        "Storage IDs require lowercase letters, digits and internal hyphens (maximum 48)"
    );
    Ok(())
}
fn node_path(value: &str) -> Result<()> {
    // These are Linux node paths even when the controller runs on Windows/macOS.
    ensure!(
        value.starts_with('/')
            && value != "/"
            && value.len() <= 4096
            && !value.chars().any(char::is_control)
            && !value.contains('\\')
            && !value.split('/').any(|part| part == "." || part == ".."),
        "Storage node paths must be absolute without traversal or control characters"
    );
    Ok(())
}
fn fields(body: &Value, allowed: &[&str], required: &[&str]) -> Result<()> {
    let object = body.as_object().context("Expected object")?;
    ensure!(
        object.keys().all(|key| allowed.contains(&key.as_str()))
            && required.iter().all(|key| object.contains_key(*key)),
        "Unsupported storage fields"
    );
    Ok(())
}
fn text<'a>(body: &'a Value, key: &str) -> Result<&'a str> {
    body[key]
        .as_str()
        .context("Required storage string missing")
}
fn hex(value: &str, lengths: &[usize]) -> Result<()> {
    ensure!(
        lengths.contains(&value.len())
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "An exact lowercase hexadecimal digest is required"
    );
    Ok(())
}

impl Storage {
    pub fn open(root: &Path, fleet: Fleet) -> Result<Self> {
        let directory = root.join("storage");
        state::private_dir(&directory)?;
        let path = directory.join("config.json");
        let registry: Registry = if path.try_exists()? {
            serde_json::from_value(state::read_json(&path)?)?
        } else {
            Registry {
                schema_version: 1,
                nodes: vec![],
            }
        };
        ensure!(
            registry.schema_version == 1 && registry.nodes.len() <= 256,
            "Invalid storage registry schema or node count"
        );
        let mut nodes = BTreeSet::new();
        for node in &registry.nodes {
            fleet.host(&node.host_id)?;
            ensure!(nodes.insert(&node.host_id), "Duplicate storage node");
            node_path(&node.executable)?;
            node_path(&node.config)?;
            ensure!(
                node.projects.len() <= 1024,
                "Too many registered storage projects"
            );
            let mut projects = BTreeSet::new();
            for project in &node.projects {
                identifier(&project.id)?;
                node_path(&project.repository)?;
                ensure!(projects.insert(&project.id), "Duplicate storage project");
            }
        }
        Ok(Self {
            registry,
            fleet,
            runner: Arc::new(|host, argv, timeout| {
                Ok(serde_json::from_slice(&host_run(
                    host, argv, None, timeout,
                )?)?)
            }),
        })
    }
    pub fn catalog(&self) -> Value {
        json!({"backend":"layerfs","configured":!self.registry.nodes.is_empty(),"nodes":self.registry.nodes.iter().map(|node|json!({"host_id":node.host_id,"status":"registered","projects":node.projects.iter().map(|project|json!({"id":project.id})).collect::<Vec<_>>()})).collect::<Vec<_>>(),"operations":["inspect","list","preview","import","fork","deps","checkpoint","remount","close","recover"],"incus_attachment":false,"automatic_replay":false})
    }
    pub fn plan(&self, host_id: &str, operation: &str, body: &Value) -> Result<Plan> {
        let node = self
            .registry
            .nodes
            .iter()
            .find(|node| node.host_id == host_id)
            .context("Storage is not registered on this host")?;
        let host = self.fleet.host(host_id)?;
        // Strip inherited fleet selectors and provider secrets before sudo executes
        // the administrator-selected CLI. Every caller-supplied value remains argv.
        let mut argv = vec![
            "env".into(),
            "-i".into(),
            "PATH=/usr/local/bin:/usr/bin:/bin".into(),
            node.executable.clone(),
            "--host".into(),
            "local".into(),
            "storage".into(),
            "layerfs".into(),
            "--config".into(),
            node.config.clone(),
        ];
        let name;
        let timeout = match operation {
            "inspect" | "list" => {
                fields(body, &[], &[])?;
                name = host_id.to_owned();
                argv.push(
                    if operation == "inspect" {
                        "doctor"
                    } else {
                        "list"
                    }
                    .into(),
                );
                120
            }
            "preview" | "import" => {
                fields(
                    body,
                    if operation == "preview" {
                        &["project", "revision"]
                    } else {
                        &["project", "revision", "approval", "credential_free"]
                    },
                    if operation == "preview" {
                        &["project", "revision"]
                    } else {
                        &["project", "revision", "approval", "credential_free"]
                    },
                )?;
                let project = text(body, "project")?;
                let registered = node
                    .projects
                    .iter()
                    .find(|p| p.id == project)
                    .context("Unknown registered storage project")?;
                name = project.to_owned();
                let revision = text(body, "revision")?;
                ensure!(
                    !revision.is_empty()
                        && revision.len() <= 256
                        && !revision.starts_with('-')
                        && !revision.chars().any(char::is_control),
                    "Invalid Git revision"
                );
                argv.push(operation.into());
                if operation == "import" {
                    hex(revision, &[40, 64])?;
                    let approval = text(body, "approval")?;
                    hex(approval, &[64])?;
                    ensure!(
                        body["credential_free"] == true,
                        "Explicit credential-free source approval required"
                    );
                    argv.extend([
                        project.into(),
                        "--approval".into(),
                        approval.into(),
                        "--credential-free".into(),
                    ]);
                }
                argv.extend([
                    "--repo".into(),
                    registered.repository.clone(),
                    "--revision".into(),
                    revision.into(),
                ]);
                if operation == "import" { 900 } else { 300 }
            }
            "fork" => {
                fields(body, &["project", "name", "from"], &["project", "name"])?;
                let project = text(body, "project")?;
                ensure!(
                    node.projects.iter().any(|p| p.id == project),
                    "Unknown registered storage project"
                );
                name = text(body, "name")?.to_owned();
                identifier(&name)?;
                argv.extend(["fork".into(), project.into(), name.clone()]);
                if body.get("from").is_some() {
                    let from = text(body, "from")?;
                    identifier(from)?;
                    argv.extend(["--from".into(), from.into()]);
                }
                360
            }
            "deps" | "checkpoint" | "remount" | "close" | "recover" => {
                fields(
                    body,
                    if operation == "checkpoint" {
                        &["name", "message"]
                    } else {
                        &["name"]
                    },
                    &["name"],
                )?;
                name = text(body, "name")?.to_owned();
                identifier(&name)?;
                argv.extend([operation.into(), name.clone()]);
                if let Some(message) = body.get("message") {
                    let message = message.as_str().context("Invalid checkpoint message")?;
                    ensure!(
                        message.len() <= 4096 && !message.contains('\0'),
                        "Checkpoint message too long or invalid"
                    );
                    argv.extend(["--message".into(), message.into()]);
                }
                420
            }
            _ => anyhow::bail!("Unsupported storage operation"),
        };
        Ok(Plan {
            host,
            operation: format!("storage_{operation}"),
            name,
            argv,
            timeout: Duration::from_secs(timeout),
            runner: self.runner.clone(),
        })
    }
    #[cfg(test)]
    pub(crate) fn with_runner(mut self, runner: Runner) -> Self {
        self.runner = runner;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn configured() -> (tempfile::TempDir, Storage) {
        let root = tempfile::tempdir().unwrap();
        let state_root = root.path().join("state");
        let fleet = Fleet::from_hosts(
            vec![Host {
                id: "node".into(),
                target: "unused-fixture-node".into(),
                ..Default::default()
            }],
            &state_root,
        )
        .unwrap();
        state::write_json(&state_root.join("storage/config.json"),&json!({"schema_version":1,"nodes":[{"host_id":"node","executable":"/opt/enginebox/current/bin/ebox","config":"/etc/enginebox/layerfs.json","projects":[{"id":"game","repository":"/srv/repositories/game"}]}]})).unwrap();
        let storage = Storage::open(&state_root, fleet).unwrap();
        (root, storage)
    }
    #[test]
    fn registry_controls_paths_and_import_requires_exact_reviewed_commit() {
        let (_root, storage) = configured();
        let plan=storage.plan("node","import",&json!({"project":"game","revision":"a".repeat(40),"approval":"b".repeat(64),"credential_free":true})).unwrap();
        assert!(
            plan.argv
                .windows(2)
                .any(|v| v == ["--repo", "/srv/repositories/game"])
        );
        assert!(plan.argv.windows(2).any(|v| v == ["--host", "local"]));
        assert_eq!(
            &plan.argv[..3],
            &["env", "-i", "PATH=/usr/local/bin:/usr/bin:/bin"]
        );
        assert!(!storage.catalog().to_string().contains("/srv/"));
        for body in [
            json!({"project":"game","revision":"HEAD","approval":"b".repeat(64),"credential_free":true}),
            json!({"project":"game","revision":"a".repeat(40),"approval":"b".repeat(64),"credential_free":false}),
            json!({"project":"game","revision":"a".repeat(40),"approval":"b".repeat(64),"credential_free":true,"repo":"/root/private"}),
        ] {
            assert!(storage.plan("node", "import", &body).is_err());
        }
    }
    #[test]
    fn unregistered_targets_commands_and_restore_are_not_exposed() {
        let (_root, storage) = configured();
        for (host, op, body) in [
            ("foreign", "list", json!({})),
            ("node", "exec", json!({"name":"a","argv":["id"]})),
            ("node", "restore-checkpoint", json!({"name":"a"})),
            ("node", "fork", json!({"project":"game","name":"../escape"})),
            (
                "node",
                "fork",
                json!({"project":"unregistered","name":"worker"}),
            ),
            ("node", "close", json!({"name":"worker","discard":true})),
        ] {
            assert!(storage.plan(host, op, &body).is_err());
        }
        assert!(
            storage
                .plan("node", "recover", &json!({"name":"worker"}))
                .is_ok()
        );
    }
    #[test]
    fn node_failure_is_not_reported_as_success() {
        let (_root, storage) = configured();
        let storage = storage.with_runner(Arc::new(|_, _, _| Ok(json!({"ok":false}))));
        assert!(
            storage
                .plan("node", "deps", &json!({"name":"worker"}))
                .unwrap()
                .execute()
                .is_err()
        );
    }
    #[test]
    fn oversized_storage_results_cannot_poison_durable_job_records() {
        let (_root, storage) = configured();
        let storage = storage.with_runner(Arc::new(|_, _, _| {
            Ok(json!({"files":["a".repeat(3 * 1024 * 1024)]}))
        }));
        let error = storage
            .plan(
                "node",
                "preview",
                &json!({"project":"game","revision":"HEAD"}),
            )
            .unwrap()
            .execute()
            .unwrap_err();
        assert!(error.to_string().contains("controller limit"));
    }
}
