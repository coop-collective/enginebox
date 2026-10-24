//! Controller-owned registrations and bounded node-local checkout operations.
//! Paths and executables come from the private administrator registry, never HTTP/MCP.
use crate::environment::DevService;
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
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DevServices {
    schema_version: u32,
    hosts: std::collections::BTreeMap<String, DevService>,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Node {
    host_id: String,
    executable: String,
    config: String,
    projects: Vec<Project>,
    #[serde(default)]
    github_token_file: Option<String>,
    #[serde(default)]
    dev_service: Option<DevService>,
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
    action: Action,
    dev_service: Option<DevService>,
}
enum Action {
    Node,
    Dev {
        service: DevService,
        start: bool,
    },
    Compatibility,
    Prepare {
        fleet: Fleet,
        project: String,
        params: Value,
    },
    Allocation {
        fleet: Fleet,
    },
}
impl Plan {
    pub fn execute(self) -> Result<Value> {
        let result = if let Action::Dev { service, start } = &self.action {
            service.request(&self.host.id, &self.name, *start, *start)?
        } else if let Action::Allocation { fleet } = &self.action {
            enginebox_core::preparation::inspect(fleet, &self.host.id, &self.name)?
        } else {
            (self.runner)(&self.host, &self.argv, self.timeout)?
        };
        ensure!(
            result.is_object(),
            "Storage node returned an invalid result"
        );
        ensure!(result["ok"] != false, "Storage operation reported failure");
        ensure!(
            serde_json::to_vec(&result)?.len() <= 3 * 1024 * 1024,
            "Storage result exceeds the controller limit; inspect it directly on the node"
        );
        let mut result = match self.action {
            Action::Compatibility => {
                require_storage_protocol(&result)?;
                result
            }
            Action::Prepare {
                fleet,
                project,
                params,
            } => {
                require_storage_protocol(&result)?;
                enginebox_core::preparation::prepare(&fleet, &self.host.id, &project, &params)?
            }
            _ => result,
        };
        if matches!(
            self.operation.as_str(),
            "storage_github-import" | "storage_import"
        ) {
            // Import remains committed when optional dev preparation is unavailable.
            // The same idempotent project endpoint is used by explicit MCP retries.
            result["dev"] = match self.dev_service {
                Some(service) => service.request(&self.host.id, &self.name, true, false)
                    .unwrap_or_else(|_| json!({"status":"unavailable","error":"Development setup could not be queued; use dev-start to retry."})),
                None => json!({"status":"not-configured"}),
            };
        }
        ensure!(
            serde_json::to_vec(&result)?.len() <= 3 * 1024 * 1024,
            "Storage result exceeds the controller limit; inspect it directly on the node"
        );
        Ok(
            json!({"backend":"layerfs","host_id":self.host.id,"operation":self.operation,"result":result}),
        )
    }
}

fn require_storage_protocol(report: &Value) -> Result<()> {
    ensure!(
        serde_json::to_vec(report)?.len() <= 16 * 1024,
        "Node protocol report exceeds 16 KiB"
    );
    ensure!(
        report["component"] == "ebox"
            && report["protocol"] == enginebox_core::node_protocol::PROTOCOL
            && report["os"] == "linux"
            && report["layerfs"]["workspace_schema"] == 2
            && report["layerfs"]["attachment_schema"] == 3
            && report["layerfs"]["exclusive_guest_writer"] == true
            && report["capabilities"]["reserved_workspace_guard"] == 1
            && report["capabilities"]["external_checkout_lifecycle_guard"] == 1,
        "Storage executable is incompatible with stopped workspace preparation; install a compatible complete release"
    );
    Ok(())
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
        let mut registry: Registry = if path.try_exists()? {
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
        // Keep optional wiring in a separate file so compatible prior controller
        // releases can still read the original storage registry during rollback.
        let dev_path = directory.join("dev-services.json");
        if dev_path.try_exists()? {
            let services: DevServices = serde_json::from_value(state::read_json(&dev_path)?)?;
            ensure!(
                services.schema_version == 1 && services.hosts.len() <= 256,
                "Invalid development service registry"
            );
            for (host, service) in services.hosts {
                let node = registry
                    .nodes
                    .iter_mut()
                    .find(|node| node.host_id == host)
                    .context("Development service names an unknown storage host")?;
                ensure!(
                    node.dev_service.is_none(),
                    "Duplicate development service binding"
                );
                node.dev_service = Some(service);
            }
        }
        let mut nodes = BTreeSet::new();
        for node in &registry.nodes {
            fleet.host(&node.host_id)?;
            ensure!(nodes.insert(&node.host_id), "Duplicate storage node");
            node_path(&node.executable)?;
            node_path(&node.config)?;
            if let Some(path) = &node.github_token_file {
                node_path(path)?;
            }
            if let Some(service) = &node.dev_service {
                service.validate()?;
            }
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
        json!({"backend":"layerfs","configured":!self.registry.nodes.is_empty(),"nodes":self.registry.nodes.iter().map(|node|json!({"host_id":node.host_id,"status":"registered","github":{"enabled":true,"private_auth_configured":node.github_token_file.is_some()},"projects":node.projects.iter().map(|project|json!({"id":project.id})).collect::<Vec<_>>()})).collect::<Vec<_>>(),"operations":["dev-start","dev-status","github-settings","github-promote","github-delivery","github-preview","github-import","github-fork","compatibility","prepare","allocation","inspect","list","preview","import","fork","deps","checkpoint","remount","close","recover"],"incus_attachment":false,"automatic_replay":false,"preparation":{"stopped_only":true,"automatic_finalize":false,"reuses_warm_pool":false}})
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
        let mut action = Action::Node;
        let timeout = match operation {
            "dev-start" | "dev-status" => {
                fields(body, &["project"], &["project"])?;
                let project = text(body, "project")?;
                identifier(project)?;
                name = project.to_owned();
                action = Action::Dev {
                    service: node
                        .dev_service
                        .clone()
                        .context("Development service is not configured on this storage host")?,
                    start: operation == "dev-start",
                };
                15
            }
            "github-settings" | "github-promote" | "github-delivery" => {
                let (allowed, required): (&[&str], &[&str]) = match operation {
                    "github-settings" => (&["project", "enabled"], &["project"]),
                    "github-promote" => (
                        &["project", "name", "expected_main", "merge", "message"],
                        &["project", "name", "expected_main"],
                    ),
                    _ => (&["project", "action", "attempt"], &["project", "action"]),
                };
                fields(body, allowed, required)?;
                let project = text(body, "project")?;
                identifier(project)?;
                name = project.to_owned();
                argv.extend([operation.into(), project.into()]);
                if operation == "github-settings" {
                    if let Some(enabled) = body.get("enabled") {
                        argv.extend([
                            "--enabled".into(),
                            enabled
                                .as_bool()
                                .context("enabled must be boolean")?
                                .to_string(),
                        ]);
                    }
                } else if operation == "github-promote" {
                    let workspace = text(body, "name")?;
                    identifier(workspace)?;
                    let expected = text(body, "expected_main")?;
                    hex(expected, &[64])?;
                    argv.extend([workspace.into(), "--expected-main".into(), expected.into()]);
                    if let Some(merge) = body.get("merge")
                        && merge.as_bool().context("merge must be boolean")?
                    {
                        argv.push("--merge".into());
                    }
                    if body.get("message").is_some() {
                        let message = text(body, "message")?;
                        ensure!(
                            message.len() <= 4096 && !message.contains('\0'),
                            "Invalid promotion message"
                        );
                        argv.extend(["--message".into(), message.into()]);
                    }
                } else {
                    let action = text(body, "action")?;
                    ensure!(
                        ["export", "deliver", "resolve", "retry", "agent-task"].contains(&action),
                        "Unsupported sync action"
                    );
                    argv.extend(["--action".into(), action.into()]);
                    if action != "export" {
                        let attempt = text(body, "attempt")?;
                        identifier(attempt)?;
                        argv.extend(["--attempt".into(), attempt.into()]);
                    } else {
                        ensure!(
                            body.get("attempt").is_none(),
                            "Export selects the accepted main revision"
                        );
                    }
                }
                if operation != "github-settings"
                    && let Some(path) = &node.github_token_file
                {
                    argv.extend(["--token-file".into(), path.clone()]);
                }
                1800
            }
            "github-preview" => {
                fields(
                    body,
                    &["project", "repository", "branch"],
                    &["project", "repository"],
                )?;
                let project = text(body, "project")?;
                identifier(project)?;
                let repository =
                    enginebox_storage_layerfs::github::repository(text(body, "repository")?)?;
                name = project.to_owned();
                argv.extend([
                    operation.into(),
                    project.into(),
                    "--repository".into(),
                    repository,
                ]);
                if body.get("branch").is_some() {
                    let branch = text(body, "branch")?;
                    enginebox_storage_layerfs::github::branch(branch)?;
                    argv.extend(["--branch".into(), branch.into()]);
                }
                if let Some(path) = &node.github_token_file {
                    argv.extend(["--token-file".into(), path.clone()]);
                }
                780
            }
            "github-import" => {
                fields(
                    body,
                    &["project", "revision", "approval", "credential_free"],
                    &["project", "revision", "approval", "credential_free"],
                )?;
                let project = text(body, "project")?;
                identifier(project)?;
                let revision = text(body, "revision")?;
                let approval = text(body, "approval")?;
                hex(revision, &[40, 64])?;
                hex(approval, &[64])?;
                ensure!(
                    body["credential_free"] == true,
                    "Explicit credential-free source approval required"
                );
                name = project.to_owned();
                argv.extend([
                    operation.into(),
                    project.into(),
                    "--revision".into(),
                    revision.into(),
                    "--approval".into(),
                    approval.into(),
                    "--credential-free".into(),
                ]);
                900
            }
            "compatibility" | "prepare" => {
                argv.truncate(6);
                argv.push("node-protocol".into());
                if operation == "compatibility" {
                    fields(body, &[], &[])?;
                    name = host_id.to_owned();
                    action = Action::Compatibility;
                } else {
                    fields(
                        body,
                        &["project", "name", "template", "size", "resources"],
                        &["project", "name", "template"],
                    )?;
                    let project = text(body, "project")?;
                    ensure!(
                        node.projects.iter().any(|p| p.id == project),
                        "Unknown registered storage project"
                    );
                    let mut params = body.clone();
                    params.as_object_mut().unwrap().remove("project");
                    self.fleet.validate_operation(host_id, "create", &params)?;
                    name = text(body, "name")?.to_owned();
                    action = Action::Prepare {
                        fleet: self.fleet.clone(),
                        project: project.into(),
                        params,
                    };
                }
                30
            }
            "allocation" => {
                fields(body, &["name"], &["name"])?;
                name = text(body, "name")?.to_owned();
                enginebox_core::identifier(&name, true)?;
                action = Action::Allocation {
                    fleet: self.fleet.clone(),
                };
                0
            }
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
            "fork" | "github-fork" => {
                fields(body, &["project", "name", "from"], &["project", "name"])?;
                let project = text(body, "project")?;
                ensure!(
                    operation == "github-fork" || node.projects.iter().any(|p| p.id == project),
                    "Unknown registered storage project"
                );
                identifier(project)?;
                name = text(body, "name")?.to_owned();
                identifier(&name)?;
                argv.extend([operation.into(), project.into(), name.clone()]);
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
            action,
            dev_service: node.dev_service.clone(),
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
                templates: vec![enginebox_core::model::StaticTemplate {
                    id: "native".into(),
                    label: "Native".into(),
                    source: "ebox-base".into(),
                    snapshot: "prepared".into(),
                    minimum: None,
                    recommended: None,
                }],
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
    fn github_import_queues_the_same_dev_project_endpoint_used_by_mcp() {
        use std::io::{BufRead, Read, Write};
        let (root, mut storage) = configured();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let token_file = root.path().join("dev-token");
        std::fs::write(&token_file, "t".repeat(32)).unwrap();
        storage.registry.nodes[0].dev_service =
            Some(serde_json::from_value(json!({"url":url,"token_file":token_file})).unwrap());
        storage = storage.with_runner(Arc::new(|_, _, _| Ok(json!({"ok":true,"imported":true}))));
        let server = std::thread::spawn(move || {
            for retry in [false, true] {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = std::io::BufReader::new(&mut stream);
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                assert!(first.starts_with("POST /v1/environments/projects "));
                let mut length = 0;
                let mut authorized = false;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(value) = lower.strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                    if lower.trim() == format!("authorization: bearer {}", "t".repeat(32)) {
                        authorized = true;
                    }
                }
                assert!(authorized);
                let mut bytes = vec![0; length];
                reader.read_exact(&mut bytes).unwrap();
                assert_eq!(
                    serde_json::from_slice::<Value>(&bytes).unwrap(),
                    json!({"host":"node","project":"app","retry":retry})
                );
                let body = json!({"host":"node","project":"app","status":"queued"}).to_string();
                write!(stream, "HTTP/1.1 202 Accepted\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
            }
        });
        let result = storage.plan("node", "github-import", &json!({"project":"app","revision":"a".repeat(40),"approval":"b".repeat(64),"credential_free":true})).unwrap().execute().unwrap();
        assert_eq!(result["result"]["dev"]["status"], "queued");
        let result = storage
            .plan("node", "dev-start", &json!({"project":"app"}))
            .unwrap()
            .execute()
            .unwrap();
        assert_eq!(result["result"]["status"], "queued");
        server.join().unwrap();
        let result = storage.plan("node", "github-import", &json!({"project":"app","revision":"a".repeat(40),"approval":"b".repeat(64),"credential_free":true})).unwrap().execute().unwrap();
        assert_eq!(result["result"]["imported"], true);
        assert_eq!(result["result"]["dev"]["status"], "unavailable");
        assert!(!result.to_string().contains(&"t".repeat(32)));
        assert!(
            storage
                .plan(
                    "node",
                    "dev-start",
                    &json!({"project":"app","command":"host shell"})
                )
                .is_err()
        );
    }
    #[test]
    fn optional_dev_bindings_are_private_and_do_not_change_the_legacy_registry() {
        let (root, storage) = configured();
        assert!(
            storage
                .plan("node", "dev-status", &json!({"project":"app"}))
                .is_err()
        );
        let directory = root.path().join("state/storage");
        let original = std::fs::read(directory.join("config.json")).unwrap();
        let service =
            json!({"url":"http://127.0.0.1:18790/","token_file":root.path().join("private-token")});
        state::write_json(
            &directory.join("dev-services.json"),
            &json!({"schema_version":1,"hosts":{"node":service}}),
        )
        .unwrap();
        let configured = Storage::open(&root.path().join("state"), storage.fleet.clone()).unwrap();
        assert!(
            configured
                .plan("node", "dev-status", &json!({"project":"app"}))
                .is_ok()
        );
        assert_eq!(
            std::fs::read(directory.join("config.json")).unwrap(),
            original
        );
        assert!(!configured.catalog().to_string().contains("private-token"));
        state::write_json(
            &directory.join("dev-services.json"),
            &json!({"schema_version":1,"hosts":{"unknown":service}}),
        )
        .unwrap();
        assert!(Storage::open(&root.path().join("state"), storage.fleet).is_err());
    }
    #[test]
    fn github_requests_are_bound_to_node_credentials_and_exact_main_revisions() {
        let (_root, storage) = configured();
        let plan=storage.plan("node","github-preview",&json!({"project":"app","repository":"https://github.com/Owner/Repo.git","branch":"feature/new"})).unwrap();
        assert!(
            plan.argv
                .windows(2)
                .any(|v| v == ["--repository", "owner/repo"])
        );
        assert!(!plan.argv.iter().any(|s| s == "--token-file"));
        assert!(storage.plan("node","github-promote",&json!({"project":"app","name":"agent","expected_main":"a".repeat(64),"merge":true})).is_ok());
        for (op, body) in [
            (
                "github-preview",
                json!({"project":"app","repository":"https://token@github.com/a/b"}),
            ),
            (
                "github-preview",
                json!({"project":"app","repository":"a/b","token_file":"/private/token"}),
            ),
            (
                "github-import",
                json!({"project":"app","revision":"HEAD","approval":"a".repeat(64),"credential_free":true}),
            ),
            (
                "github-promote",
                json!({"project":"app","name":"agent","expected_main":"main"}),
            ),
            ("github-settings", json!({"project":"app","enabled":"true"})),
            (
                "github-delivery",
                json!({"project":"app","action":"resolve","attempt":"../other"}),
            ),
            (
                "github-delivery",
                json!({"project":"app","action":"push","attempt":"sync-a"}),
            ),
            (
                "github-delivery",
                json!({"project":"app","action":"export","attempt":"sync-a"}),
            ),
        ] {
            assert!(storage.plan("node", op, &body).is_err(), "{op}: {body}");
        }
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
    #[test]
    fn preparation_requires_registered_project_and_explicit_node_contract_before_mutation() {
        let (root, storage) = configured();
        let request = json!({"project":"game","name":"ebox-agent","template":"native"});
        for body in [
            json!({"project":"foreign","name":"ebox-agent","template":"native"}),
            json!({"project":"game","name":"ebox-agent","template":"native","source":"/private"}),
            json!({"project":"game","name":"ebox-agent","template":"missing"}),
        ] {
            assert!(storage.plan("node", "prepare", &body).is_err());
        }
        let plan = storage.plan("node", "prepare", &request).unwrap();
        assert_eq!(&plan.argv[4..], &["--host", "local", "node-protocol"]);
        let incompatible = storage.with_runner(Arc::new(|_, _, _| {
            Ok(json!({"version":"0.3.0","protocol":1}))
        }));
        assert!(
            incompatible
                .plan("node", "prepare", &request)
                .unwrap()
                .execute()
                .unwrap_err()
                .to_string()
                .contains("incompatible")
        );
        assert!(!root.path().join("state/allocations").exists());
    }
    #[test]
    fn compatibility_is_a_binary_check_and_rejects_missing_or_future_receipt_support() {
        let (_root, storage) = configured();
        let mut valid = enginebox_core::node_protocol::report();
        valid["os"] = json!("linux");
        valid["component"] = json!("ebox");
        valid["layerfs"] =
            json!({"workspace_schema":2,"attachment_schema":3,"exclusive_guest_writer":true});
        require_storage_protocol(&valid).unwrap();
        for replacement in [
            json!({}),
            json!({"workspace_schema":2,"attachment_schema":4,"exclusive_guest_writer":true}),
            json!({"workspace_schema":2,"attachment_schema":2,"exclusive_guest_writer":true}),
            json!({"workspace_schema":2,"attachment_schema":3,"exclusive_guest_writer":false}),
        ] {
            let mut bad = valid.clone();
            bad["layerfs"] = replacement;
            assert!(require_storage_protocol(&bad).is_err());
        }
        let storage = storage.with_runner(Arc::new(move |_, _, _| Ok(valid.clone())));
        assert!(
            storage
                .plan("node", "compatibility", &json!({}))
                .unwrap()
                .execute()
                .is_ok()
        );
        assert_eq!(storage.catalog()["incus_attachment"], false);
    }
}
