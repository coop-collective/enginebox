//! Outbound Windows contributors share this controller's durable job authority.
//! Node credentials never authorize operator APIs or another node's work.
use anyhow::{Context, Result, ensure};
use enginebox_core::state;
use enginebox_node_protocol::{Budget, Enrollment, Invitation, LEASE_SECONDS, Run, hash, id};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use subtle::ConstantTimeEq;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
fn secret_hash(token: &str) -> String {
    state::digest(&json!(token))
}
fn equal(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

#[derive(Clone, Serialize, Deserialize)]
struct Node {
    id: String,
    invitation: Invitation,
    code_hash: String,
    expires: u64,
    secret_hash: Option<String>,
    budget: Budget,
    os: String,
    architecture: String,
    revoked: bool,
    blocked: bool,
    session: String,
    epoch: u64,
    last_seen: u64,
    tools: Value,
    active: Option<String>,
}
#[derive(Clone)]
pub struct Contributors {
    root: PathBuf,
    nodes: Arc<Mutex<BTreeMap<String, Node>>>,
    assignments: Arc<Mutex<BTreeMap<String, Value>>>,
}
impl Contributors {
    pub fn open(root: &Path) -> Result<Self> {
        let root = root.join("contributors");
        state::private_dir(&root)?;
        state::private_dir(&root.join("assignments"))?;
        let mut nodes = BTreeMap::new();
        for path in state::json_files(&root)? {
            let mut n: Node = serde_json::from_value(state::read_json(&path)?)?;
            ensure!(
                id(&n.id) && path.file_stem().and_then(|s| s.to_str()) == Some(&n.id),
                "Invalid contributor record"
            );
            n.blocked |= n.active.is_some();
            n.last_seen = 0;
            n.session.clear();
            nodes.insert(n.id.clone(), n);
        }
        let mut assignments = BTreeMap::new();
        for path in state::json_files(&root.join("assignments"))? {
            let mut a = state::read_json(&path)?;
            if a["status"] == "queued" || a["status"] == "assigned" {
                a["status"] = json!("interrupted");
                state::write_json(&path, &a)?;
            }
            assignments.insert(
                a["id"].as_str().context("Invalid assignment")?.to_owned(),
                a,
            );
        }
        Ok(Self {
            root,
            nodes: Arc::new(Mutex::new(nodes)),
            assignments: Arc::new(Mutex::new(assignments)),
        })
    }
    fn save(&self, n: &Node) -> Result<()> {
        state::write_json(
            &self.root.join(format!("{}.json", n.id)),
            &serde_json::to_value(n)?,
        )
    }
    fn save_assignment(&self, a: &Value) -> Result<()> {
        let key = a["id"].as_str().context("Missing assignment id")?;
        ensure!(id(key), "Invalid assignment id");
        state::write_json(
            &self.root.join("assignments").join(format!("{key}.json")),
            a,
        )
    }
    pub fn issue(&self, invitation: Invitation) -> Result<Value> {
        invitation.validate()?;
        let mut nodes = self.nodes.lock().unwrap();
        ensure!(nodes.len() < 1000, "Contributor limit reached");
        let id = uuid::Uuid::new_v4().simple().to_string();
        let code = format!(
            "{id}.{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let expires = now() + 600;
        let node = Node {
            id: id.clone(),
            budget: invitation.budget.clone(),
            invitation,
            code_hash: secret_hash(&code),
            expires,
            secret_hash: None,
            os: String::new(),
            architecture: String::new(),
            revoked: false,
            blocked: false,
            session: String::new(),
            epoch: 0,
            last_seen: 0,
            tools: json!({}),
            active: None,
        };
        self.save(&node)?;
        nodes.insert(id.clone(), node);
        Ok(json!({"id":id,"code":code,"expires":expires,"one_time":true}))
    }
    pub fn enroll(&self, code: &str, e: Enrollment) -> Result<Value> {
        let (key, secret) = code.split_once('.').context("Invalid registration code")?;
        ensure!(
            id(key) && hash(secret) && hash(&e.secret_hash),
            "Invalid registration identity"
        );
        e.budget.validate()?;
        ensure!(
            ["windows", "linux"].contains(&e.os.as_str()) && e.architecture == "x86_64",
            "This worker release requires Windows x64 or its Linux x64 WSL runtime"
        );
        ensure!(
            e.os != "linux" || (e.budget.memory_gib >= 4 && e.budget.disk_gib >= 20),
            "WSL runtime requires at least 4 GiB memory and 20 GiB disk"
        );
        let mut nodes = self.nodes.lock().unwrap();
        let mut n = nodes.get(key).cloned().context("Unknown registration")?;
        ensure!(
            !n.revoked && n.expires > now() && equal(&n.code_hash, &secret_hash(code)),
            "Registration code expired or invalid"
        );
        ensure!(
            e.budget.fits(&n.invitation.budget),
            "Local budget exceeds the owner's allowance"
        );
        if let Some(bound) = &n.secret_hash {
            ensure!(
                equal(bound, &e.secret_hash)
                    && n.budget == e.budget
                    && n.os == e.os
                    && n.architecture == e.architecture,
                "Registration code already consumed"
            );
        } else {
            n.secret_hash = Some(e.secret_hash);
            n.budget = e.budget;
            n.os = e.os;
            n.architecture = e.architecture;
            self.save(&n)?;
        }
        let response =
            json!({"id":n.id,"budget":n.budget,"workloads":n.invitation.workloads,"enrolled":true});
        nodes.insert(key.to_owned(), n);
        Ok(response)
    }
    pub fn authenticated(&self, token: &str) -> bool {
        let Some((key, _)) = token.split_once('.') else {
            return false;
        };
        self.nodes.lock().unwrap().get(key).is_some_and(|n| {
            !n.revoked
                && n.secret_hash
                    .as_ref()
                    .is_some_and(|h| equal(h, &secret_hash(token)))
        })
    }
    pub fn catalog(&self) -> Value {
        let rows=self.nodes.lock().unwrap().values().map(|n|json!({"id":n.id,"label":n.invitation.label,
            "budget":n.budget,"workloads":n.invitation.workloads,"os":n.os,"architecture":n.architecture,
            "enrolled":n.secret_hash.is_some(),"revoked":n.revoked,"blocked":n.blocked,"active_job":n.active,
            "online":n.last_seen+LEASE_SECONDS>now(),"last_seen":n.last_seen,"tools":n.tools,
            "disk_enforcement":"workspace usage watchdog","cpu_memory_enforcement":if n.os == "linux" { "WSL runtime cgroup" } else { "Windows Job Object (legacy)" }})).collect::<Vec<_>>();
        json!({"nodes":rows})
    }
    pub fn revoke(&self, key: &str) -> Result<Value> {
        let mut nodes = self.nodes.lock().unwrap();
        let n = nodes.get_mut(key).context("Unknown node")?;
        n.revoked = true;
        self.save(n)?;
        Ok(json!({"revoked":true}))
    }
    pub fn reconcile(&self, key: &str) -> Result<Value> {
        let mut nodes = self.nodes.lock().unwrap();
        let n = nodes.get_mut(key).context("Unknown node")?;
        ensure!(
            n.blocked && n.last_seen + LEASE_SECONDS > now() && n.tools["worker_running"] == false,
            "A live node must report its owned process stopped before reconciliation"
        );
        n.blocked = false;
        n.active = None;
        self.save(n)?;
        Ok(json!({"reconciled":true}))
    }
    pub fn node_call(&self, token: &str, action: &str, body: Value) -> Result<Value> {
        ensure!(self.authenticated(token), "Invalid node credential");
        let key = token.split('.').next().unwrap();
        let mut nodes = self.nodes.lock().unwrap();
        let n = nodes.get_mut(key).context("Unknown node")?;
        ensure!(!n.revoked, "Node revoked");
        let session = body["session"].as_str().context("Session required")?;
        ensure!(uuid::Uuid::parse_str(session).is_ok(), "Invalid session");
        if action == "hello" {
            if n.session != session {
                n.blocked |= n.active.is_some();
                n.session = session.to_owned();
                n.epoch += 1;
            }
            self.save(n)?;
            return Ok(
                json!({"id":n.id,"session":n.session,"epoch":n.epoch,"budget":n.budget,"workloads":n.invitation.workloads,"blocked":n.blocked}),
            );
        }
        ensure!(
            n.session == session && body["epoch"].as_u64() == Some(n.epoch),
            "Stale node session"
        );
        match action {
            "heartbeat" => {
                let tools = &body["tools"];
                ensure!(
                    tools.is_object() && serde_json::to_vec(tools)?.len() <= 4096,
                    "Invalid tool report"
                );
                n.tools = tools.clone();
                n.last_seen = now();
                self.save(n)?;
                Ok(json!({"ok":true,"blocked":n.blocked}))
            }
            "poll" => {
                ensure!(n.last_seen + LEASE_SECONDS > now(), "Heartbeat required");
                if n.blocked || n.active.is_some() {
                    return Ok(json!({"job":null,"blocked":n.blocked}));
                }
                let mut assignments = self.assignments.lock().unwrap();
                let Some(a) = assignments
                    .values_mut()
                    .find(|a| a["node"] == key && a["status"] == "queued")
                else {
                    return Ok(json!({"job":null}));
                };
                if a["deadline"].as_u64().unwrap_or(0) <= now() {
                    a["status"] = json!("expired");
                    self.save_assignment(a)?;
                    return Ok(json!({"job":null}));
                }
                a["status"] = json!("assigned");
                a["epoch"] = json!(n.epoch);
                a["session"] = json!(session);
                n.active = Some(a["id"].as_str().unwrap().to_owned());
                self.save_assignment(a)?;
                self.save(n)?;
                Ok(json!({"job":a}))
            }
            "result" => {
                let job = body["job"].as_str().context("Job required")?;
                let mut assignments = self.assignments.lock().unwrap();
                let a = assignments.get_mut(job).context("Unknown job")?;
                ensure!(
                    a["node"] == key && a["epoch"] == n.epoch && a["session"] == session,
                    "Result is not owned by this session"
                );
                ensure!(
                    body["result"].is_object()
                        && serde_json::to_vec(&body["result"])?.len() <= 300_000,
                    "Invalid result"
                );
                if a["status"] == "completed" {
                    ensure!(a["result"] == body["result"], "Result retry differs");
                    return Ok(json!({"ok":true}));
                }
                ensure!(
                    a["status"] == "assigned" && !n.blocked && n.active.as_deref() == Some(job),
                    "Job requires reconciliation"
                );
                ensure!(
                    n.last_seen + LEASE_SECONDS > now()
                        && a["deadline"].as_u64().unwrap_or(0) > now(),
                    "Assignment lease expired"
                );
                if body["result"]["ok"] == true && a["run"]["kind"] == "ci" {
                    ensure!(
                        body["result"]["request_id"] == a["run"]["request"]["request_id"]
                            && body["result"]["sha"] == a["run"]["request"]["sha"]
                            && body["result"]["node_id"] == key,
                        "CI result does not match its assignment"
                    );
                }
                a["status"] = json!("completed");
                a["result"] = body["result"].clone();
                self.save_assignment(a)?;
                n.active = None;
                self.save(n)?;
                Ok(json!({"ok":true}))
            }
            _ => anyhow::bail!("Unknown node operation"),
        }
    }
    pub fn submit(&self, jobs: &crate::jobs::Jobs, key: &str, run: Run) -> Result<Value> {
        ensure!(id(key) && id(&run.request_id), "Invalid run identity");
        let key_id = format!("node-{key}-{}", run.request_id);
        let digest = state::digest(&serde_json::to_value(&run)?);
        if let Some(existing) = jobs.existing_request(&key_id, &digest)? {
            return Ok(existing);
        }
        let timeout = {
            let nodes = self.nodes.lock().unwrap();
            let n = nodes.get(key).context("Unknown node")?;
            ensure!(
                !n.revoked && !n.blocked && n.last_seen + LEASE_SECONDS > now(),
                "Node is offline or needs reconciliation"
            );
            match run.kind.as_str() {
                "workspace" => {
                    ensure!(
                        n.os == "linux"
                            && (n.invitation.workloads.ci || n.invitation.workloads.agents)
                            && n.tools["workspace"] == true,
                        "The WSL template runtime is not ready"
                    );
                    let r: enginebox_node_protocol::WorkspaceRequest =
                        serde_json::from_value(run.request.clone())?;
                    r.validate()?;
                    r.timeout_seconds + 180
                }
                "ci" => {
                    ensure!(
                        n.invitation.workloads.ci && n.tools["ci"] == true,
                        "CI tools are not ready"
                    );
                    let r: enginebox_ci::Request = serde_json::from_value(run.request.clone())?;
                    r.validate()?;
                    ensure!(
                        r.request_id == run.request_id,
                        "CI request identities differ"
                    );
                    let enginebox_ci::Job::Automation {
                        timeout_minutes,
                        shell,
                        runs_on,
                        node,
                        ..
                    } = r.job
                    else {
                        anyhow::bail!("Outbound nodes accept native automations")
                    };
                    let worker = enginebox_ci::Node {
                        schema_version: 1,
                        id: key.to_owned(),
                        target: "local".into(),
                        os: enginebox_ci::model::Os::Windows,
                        architecture: "x64".into(),
                        image: "windows-native-v1".into(),
                        capabilities: vec!["pwsh".into()],
                        disabled: false,
                    };
                    ensure!(
                        shell == enginebox_ci::model::Shell::Pwsh
                            && worker.matches(&runs_on)
                            && node.as_deref().is_none_or(|selected| selected == key),
                        "Windows worker does not match requested shell or capabilities"
                    );
                    timeout_minutes * 60
                }
                "agent" => {
                    ensure!(
                        n.invitation.workloads.agents && n.tools["codex"] == true,
                        "Native Codex is not installed or signed in"
                    );
                    let r = enginebox_agents::native::RunRequest::parse(run.request.clone())?;
                    ensure!(
                        r.provider == "codex" && r.resume_id.is_none() && r.workdir == ".",
                        "This native worker uses new Codex runs"
                    );
                    r.timeout_seconds
                }
                _ => anyhow::bail!("Unknown workload"),
            }
        };
        let service = self.clone();
        let key = key.to_owned();
        let request_id = run.request_id.clone();
        jobs.submit_once(
            &key.clone(),
            "ci_contributor",
            &request_id,
            (&key_id, &digest),
            move || service.dispatch(&key, run, timeout),
        )
    }
    fn dispatch(&self, key: &str, run: Run, timeout: u64) -> Result<Value> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let a = json!({"id":id,"node":key,"status":"queued","run":run,"deadline":now()+timeout+60});
        {
            let mut assignments = self.assignments.lock().unwrap();
            ensure!(
                assignments.len() < 10000,
                "Assignment history limit reached"
            );
            self.save_assignment(&a)?;
            assignments.insert(id.clone(), a);
        }
        loop {
            std::thread::sleep(Duration::from_millis(100));
            let mut nodes = self.nodes.lock().unwrap();
            let n = nodes.get_mut(key).context("Node removed")?;
            let mut assignments = self.assignments.lock().unwrap();
            let a = assignments.get_mut(&id).unwrap();
            if a["status"] == "completed" {
                ensure!(
                    a["result"]["ok"] == true,
                    "Node workload failed; inspect its private diagnostics"
                );
                return Ok(json!({"node_id":key,"workload":a["run"]["kind"],"result":a["result"]}));
            }
            if n.revoked
                || n.last_seen + LEASE_SECONDS <= now()
                || a["deadline"].as_u64().unwrap() <= now()
                || a["status"] == "interrupted"
            {
                n.blocked |= a["status"] == "assigned";
                a["status"] = json!("interrupted");
                self.save_assignment(a)?;
                self.save(n)?;
                anyhow::bail!(
                    "Node workload interrupted; inspect owned processes before reconciliation"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use enginebox_node_protocol::Workloads;
    fn invitation() -> Invitation {
        Invitation {
            label: "Test PC".into(),
            budget: Budget {
                cpu: 4,
                memory_gib: 8,
                disk_gib: 50,
            },
            workloads: Workloads {
                ci: true,
                agents: true,
            },
        }
    }
    fn enroll(service: &Contributors) -> (String, String, String) {
        let issued = service.issue(invitation()).unwrap();
        let id = issued["id"].as_str().unwrap().to_owned();
        let code = issued["code"].as_str().unwrap().to_owned();
        let token = format!("{id}.{}", "a".repeat(64));
        service
            .enroll(
                &code,
                Enrollment {
                    secret_hash: secret_hash(&token),
                    budget: invitation().budget,
                    os: "windows".into(),
                    architecture: "x86_64".into(),
                },
            )
            .unwrap();
        (id, code, token)
    }
    fn connect(service: &Contributors, token: &str) -> Value {
        let session = uuid::Uuid::new_v4().to_string();
        let hello = service
            .node_call(token, "hello", json!({"session":session}))
            .unwrap();
        let body = json!({"session":session,"epoch":hello["epoch"],"tools":{"ci":true,"codex":true,"worker_running":false}});
        service.node_call(token, "heartbeat", body.clone()).unwrap();
        body
    }
    #[test]
    fn codes_bind_one_identity_with_downward_budgets_expiry_and_revocation() {
        let tmp = tempfile::tempdir().unwrap();
        let service = Contributors::open(tmp.path()).unwrap();
        let issued = service.issue(invitation()).unwrap();
        let key = issued["id"].as_str().unwrap();
        let code = issued["code"].as_str().unwrap();
        let token = format!("{key}.{}", "b".repeat(64));
        let mut e = Enrollment {
            secret_hash: secret_hash(&token),
            budget: Budget {
                cpu: 5,
                memory_gib: 4,
                disk_gib: 20,
            },
            os: "windows".into(),
            architecture: "x86_64".into(),
        };
        assert!(service.enroll(code, e.clone()).is_err());
        e.budget.cpu = 2;
        assert!(service.enroll(code, e.clone()).is_ok());
        assert!(service.enroll(code, e.clone()).is_ok());
        e.secret_hash = secret_hash("another-identity");
        assert!(service.enroll(code, e).is_err());
        assert!(service.authenticated(&token));
        assert!(!service.authenticated(code));
        let catalog = service.catalog().to_string();
        assert!(!catalog.contains(code));
        assert!(!catalog.contains(&token));
        assert!(!catalog.contains("secret_hash"));
        service.revoke(key).unwrap();
        assert!(!service.authenticated(&token));
        let expired = service.issue(invitation()).unwrap();
        let key = expired["id"].as_str().unwrap();
        service.nodes.lock().unwrap().get_mut(key).unwrap().expires = 0;
        assert!(
            service
                .enroll(
                    expired["code"].as_str().unwrap(),
                    Enrollment {
                        secret_hash: secret_hash("expired"),
                        budget: invitation().budget,
                        os: "windows".into(),
                        architecture: "x86_64".into()
                    }
                )
                .is_err()
        );
    }
    #[test]
    fn assignments_never_replay_and_sessions_and_results_are_bound() {
        let tmp = tempfile::tempdir().unwrap();
        let service = Contributors::open(tmp.path()).unwrap();
        let (key, _, token) = enroll(&service);
        let body = connect(&service, &token);
        let (_, _, other) = enroll(&service);
        let other_body = connect(&service, &other);
        let a = json!({"id":"job1","node":key,"status":"queued","deadline":now()+60,"run":{"kind":"ci","request":{"request_id":"check1","sha":"a".repeat(40)}}});
        service.save_assignment(&a).unwrap();
        service.assignments.lock().unwrap().insert("job1".into(), a);
        assert_eq!(
            service.node_call(&token, "poll", body.clone()).unwrap()["job"]["id"],
            "job1"
        );
        assert!(service.node_call(&token, "poll", body.clone()).unwrap()["job"].is_null());
        let mut completion = body.clone();
        completion["job"] = json!("job1");
        completion["result"] =
            json!({"ok":true,"request_id":"wrong","node_id":key,"sha":"a".repeat(40)});
        assert!(
            service
                .node_call(&token, "result", completion.clone())
                .is_err()
        );
        completion["result"]["request_id"] = json!("check1");
        let mut theft = completion.clone();
        theft["session"] = other_body["session"].clone();
        theft["epoch"] = other_body["epoch"].clone();
        assert!(service.node_call(&other, "result", theft).is_err());
        service
            .node_call(&token, "result", completion.clone())
            .unwrap();
        service
            .node_call(&token, "result", completion.clone())
            .unwrap();
        completion["result"]["ok"] = json!(false);
        assert!(service.node_call(&token, "result", completion).is_err());
        let new_session = connect(&service, &token);
        assert!(service.node_call(&token, "poll", body).is_err());
        assert!(
            !service.node_call(&token, "heartbeat", new_session).unwrap()["blocked"]
                .as_bool()
                .unwrap()
        );
    }
    #[test]
    fn restart_and_lost_leases_require_stopped_process_reconciliation() {
        let tmp = tempfile::tempdir().unwrap();
        let service = Contributors::open(tmp.path()).unwrap();
        let (key, _, token) = enroll(&service);
        let body = connect(&service, &token);
        let a = json!({"id":"uncertain","node":key,"status":"queued","deadline":now()+60});
        service.save_assignment(&a).unwrap();
        service
            .assignments
            .lock()
            .unwrap()
            .insert("uncertain".into(), a);
        service.node_call(&token, "poll", body.clone()).unwrap();
        service
            .assignments
            .lock()
            .unwrap()
            .get_mut("uncertain")
            .unwrap()["deadline"] = json!(0);
        let mut result = body;
        result["job"] = json!("uncertain");
        result["result"] = json!({"ok":true});
        assert!(service.node_call(&token, "result", result).is_err());
        drop(service);
        let recovered = Contributors::open(tmp.path()).unwrap();
        assert_eq!(recovered.catalog()["nodes"][0]["blocked"], true);
        assert!(recovered.reconcile(&key).is_err());
        let mut hello = connect(&recovered, &token);
        hello["tools"]["worker_running"] = json!(true);
        recovered
            .node_call(&token, "heartbeat", hello.clone())
            .unwrap();
        assert!(recovered.reconcile(&key).is_err());
        hello["tools"]["worker_running"] = json!(false);
        recovered
            .node_call(&token, "heartbeat", hello.clone())
            .unwrap();
        recovered.reconcile(&key).unwrap();
        assert!(recovered.node_call(&token, "poll", hello).unwrap()["job"].is_null());
    }
}
