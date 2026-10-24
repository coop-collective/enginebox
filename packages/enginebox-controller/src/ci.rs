//! Optional CI inventory and dispatch, using the controller's existing durable
//! job journal/ownership gate. GitHub credentials never enter job records.
use anyhow::{Context, Result, ensure};
use enginebox_ci::{Job, Node, Request, executor, github::Github};
use enginebox_core::state;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

type ExecutionLocks = Arc<Mutex<BTreeMap<String, Arc<Mutex<()>>>>>;

#[derive(Clone)]
pub struct Ci {
    root: PathBuf,
    nodes: Arc<Mutex<BTreeMap<String, Node>>>,
    agent_targets: BTreeSet<String>,
    execution_locks: ExecutionLocks,
}
fn machine(target: &str) -> String {
    let host = target
        .rsplit('@')
        .next()
        .unwrap_or(target)
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if ["local", "localhost", "127.0.0.1", "::1", "[::1]"].contains(&host.as_str()) {
        "local".into()
    } else {
        host
    }
}

impl Ci {
    pub fn open(root: &Path, agent_targets: &[String]) -> Result<Self> {
        let root = root.join("ci-nodes");
        state::private_dir(&root)?;
        let mut nodes = BTreeMap::new();
        for file in state::json_files(&root)? {
            let node: Node = serde_json::from_value(state::read_json(&file)?)?;
            node.validate()?;
            ensure!(
                file.file_stem().and_then(|s| s.to_str()) == Some(&node.id),
                "Invalid CI node filename"
            );
            ensure!(
                !nodes
                    .values()
                    .any(|n: &Node| machine(&n.target) == machine(&node.target)),
                "Duplicate CI execution target"
            );
            nodes.insert(node.id.clone(), node);
        }
        Ok(Self {
            root,
            nodes: Arc::new(Mutex::new(nodes)),
            agent_targets: agent_targets.iter().map(|target| machine(target)).collect(),
            execution_locks: Arc::default(),
        })
    }
    pub fn register(&self, value: Value) -> Result<Value> {
        let node: Node = serde_json::from_value(value)?;
        node.validate()?;
        ensure!(
            !self.agent_targets.contains(&machine(&node.target)),
            "CI requires a dedicated worker; this target is already an agent host"
        );
        let mut nodes = self.nodes.lock().unwrap();
        ensure!(
            nodes.len() < 1000 || nodes.contains_key(&node.id),
            "CI node limit reached"
        );
        ensure!(
            !nodes
                .values()
                .any(|n| n.id != node.id && machine(&n.target) == machine(&node.target)),
            "Execution target already registered"
        );
        state::write_json(
            &self.root.join(format!("{}.json", node.id)),
            &serde_json::to_value(&node)?,
        )?;
        nodes.insert(node.id.clone(), node);
        Ok(json!({"registered":true}))
    }
    pub fn catalog(&self) -> Value {
        let nodes: Vec<_> = self.nodes.lock().unwrap().values().map(|n| {
            json!({"id":n.id,"os":n.os,"architecture":n.architecture,"image":n.image,"labels":n.labels(),"disabled":n.disabled,"workload_pool":"ci","agent_host_conflict":self.agent_targets.contains(&machine(&n.target)),"capacity":{"concurrent_jobs":1},"health":"not_probed","capabilities_verified":false})
        }).collect();
        json!({"schema_version":1,"nodes":nodes,"runners":["automation","github-actions"]})
    }
    pub fn select(&self, request: &Request) -> Result<Option<Node>> {
        request.validate()?;
        match &request.job {
            Job::GithubActions { .. } => Ok(None),
            Job::Automation {
                runs_on,
                node,
                shell,
                ..
            } => {
                let nodes = self.nodes.lock().unwrap();
                let candidates: Vec<_> = nodes
                    .values()
                    .filter(|n| {
                        node.as_ref().is_none_or(|id| id == &n.id)
                            && !self.agent_targets.contains(&machine(&n.target))
                            && n.matches(runs_on)
                            && (shell != &enginebox_ci::model::Shell::Pwsh
                                || n.capabilities
                                    .iter()
                                    .any(|s| s.eq_ignore_ascii_case("pwsh")))
                    })
                    .cloned()
                    .collect();
                ensure!(
                    !candidates.is_empty(),
                    "No registered node matches every requested capability"
                );
                ensure!(
                    candidates.len() == 1,
                    "Multiple nodes match; select a node explicitly"
                );
                Ok(candidates.into_iter().next())
            }
        }
    }
    pub fn plan(&self, request: &Request) -> Result<Value> {
        let node = self.select(request)?;
        Ok(
            json!({"request_id":request.request_id,"event":request.event,"sha":request.sha,"node_id":node.map(|n|n.id),"runner":if matches!(request.job,Job::Automation {..}){"automation"}else{"github-actions"},"github_placement":"Workflow runs-on labels select hosted or registered self-hosted runners","read_only":true}),
        )
    }
    pub fn submit(&self, jobs: &crate::jobs::Jobs, request: Request) -> Result<Value> {
        request.validate()?;
        let digest = state::digest(&serde_json::to_value(&request)?);
        if let Some(existing) = jobs.existing_request(&request.request_id, &digest)? {
            return Ok(existing);
        }
        let node = self.select(&request)?;
        let host = node
            .as_ref()
            .map(|n| n.id.clone())
            .unwrap_or_else(|| "github".into());
        let operation = if node.is_some() {
            "ci_automation"
        } else {
            "ci_github_dispatch"
        };
        let execution_lock = node.as_ref().map(|node| {
            self.execution_locks
                .lock()
                .unwrap()
                .entry(machine(&node.target))
                .or_default()
                .clone()
        });
        let id = request.request_id.clone();
        // Persist the request identity before the first externally visible effect.
        jobs.submit_once(
            &host,
            operation,
            &id,
            (&id, &digest),
            move || match request.job {
                Job::Automation { .. } => {
                    let _guard = execution_lock
                        .as_ref()
                        .unwrap()
                        .lock()
                        .map_err(|_| anyhow::anyhow!("CI worker lock poisoned"))?;
                    executor::execute_on(&node.context("Missing CI node")?, &request)
                }
                Job::GithubActions { .. } => Github::from_env()?.dispatch(&request),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn heavy_ci_cannot_share_an_agent_host_even_under_another_ssh_user() {
        let tmp = tempfile::tempdir().unwrap();
        let ci = Ci::open(tmp.path(), &["agent@agent-host.example".into()]).unwrap();
        let mut node = json!({"schema_version":1,"id":"build","target":"builder@AGENT-HOST.EXAMPLE.","os":"linux","architecture":"x64","image":"ubuntu-build","capabilities":[]});
        assert!(ci.register(node.clone()).is_err());
        node["target"] = json!("builder@ci-host.example");
        ci.register(node).unwrap();
        // A changed fleet configuration also fences an earlier CI registration.
        let ci = Ci::open(tmp.path(), &["agent@ci-host.example".into()]).unwrap();
        assert_eq!(ci.catalog()["nodes"][0]["agent_host_conflict"], true);
        let request = json!({"schema_version":1,"request_id":"merge-linux","event":"merge","sha":"a".repeat(40),"job":{"runner":"automation","runs-on":["linux"],"shell":"bash","run":"true","timeout-minutes":1}});
        assert!(
            ci.select(&serde_json::from_value(request).unwrap())
                .is_err()
        );
    }
    #[test]
    fn registered_capabilities_survive_restart_and_reject_mismatched_or_ambiguous_jobs() {
        let tmp = tempfile::tempdir().unwrap();
        let ci = Ci::open(tmp.path(), &[]).unwrap();
        let mut node = json!({"schema_version":1,"id":"mac","target":"builder@mac","os":"macos","architecture":"arm64","image":"macos-15-xcode","capabilities":["xcode"]});
        ci.register(node.clone()).unwrap();
        let ci = Ci::open(tmp.path(), &[]).unwrap();
        let mut request = json!({"schema_version":1,"request_id":"merge-ios","event":"merge","sha":"a".repeat(40),"job":{"runner":"automation","runs-on":["macos","xcode"],"shell":"bash","run":"true","timeout-minutes":60}});
        assert_eq!(
            ci.plan(&serde_json::from_value(request.clone()).unwrap())
                .unwrap()["node_id"],
            "mac"
        );
        request["job"]["runs-on"] = json!(["windows", "xcode"]);
        assert!(
            ci.plan(&serde_json::from_value(request.clone()).unwrap())
                .is_err()
        );
        request["job"]["runs-on"] = json!(["macos", "xcode"]);
        node["id"] = json!("mac2");
        assert!(
            ci.register(node.clone()).is_err(),
            "target cannot be registered twice"
        );
        node["target"] = json!("builder@mac2");
        ci.register(node.clone()).unwrap();
        assert!(
            ci.plan(&serde_json::from_value(request.clone()).unwrap())
                .is_err(),
            "selection must be explicit when ambiguous"
        );
        request["job"]["node"] = json!("mac2");
        assert_eq!(
            ci.plan(&serde_json::from_value(request.clone()).unwrap())
                .unwrap()["node_id"],
            "mac2"
        );
        node["disabled"] = json!(true);
        ci.register(node).unwrap();
        assert!(ci.plan(&serde_json::from_value(request).unwrap()).is_err());
        let catalog = ci.catalog().to_string();
        assert!(!catalog.contains("builder@"));
    }
}
