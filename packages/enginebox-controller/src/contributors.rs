//! Outbound contributors and managed Incus hosts share scoped registration.
//! Node credentials never authorize operator APIs or another node's work.
use anyhow::{Context, Result, ensure};
use enginebox_core::state;
use enginebox_node_protocol::{
    Budget, Enrollment, Invitation, LEASE_SECONDS, ManagedHostBinding, Run, Workloads, hash, id,
};
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host_binding: Option<ManagedHostBinding>,
    /// The member whose personal engine machine this is (engine_sessions.rs).
    /// Only that member's sessions are placed on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    engine_owner: Option<String>,
}
fn invitation_response(n: &Node, code: &str) -> Value {
    json!({"id":n.id,"code":code,"expires":n.expires,"one_time":true,
        "kind":if n.host_binding.is_some() {"incus"} else {"workspace"},
        "host_id":n.host_binding.as_ref().map(|b| &b.host_id),"budget":n.budget})
}
/// An engine machine as engine session placement sees it.
pub struct EngineNode {
    pub id: String,
    pub engine_owner: Option<String>,
    pub revoked: bool,
    pub online: bool,
    pub windows: bool,
    pub epoch: u64,
    /// The node's `tools.engine` report (`{ready, unity:[versions], running}`).
    pub tools: Value,
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
        self.issue_bound(invitation, None)
    }
    pub fn issue_host(
        &self,
        host: &enginebox_core::Host,
        machine_id_hash: String,
        budget: Budget,
    ) -> Result<Value> {
        ensure!(
            host.managed_registration,
            "Enable managed_registration in the approved host configuration first"
        );
        let binding = ManagedHostBinding {
            host_id: host.id.clone(),
            binding_digest: state::digest(&host.binding()),
            machine_id_hash,
            project: host.project.clone(),
            pool: host.pool.clone(),
        };
        binding.validate()?;
        self.issue_bound(
            Invitation {
                label: host.label.clone(),
                budget,
                workloads: Workloads {
                    agents: true,
                    ci: false,
                    engine: false,
                },
            },
            Some(binding),
        )
    }
    fn issue_bound(
        &self,
        invitation: Invitation,
        host_binding: Option<ManagedHostBinding>,
    ) -> Result<Value> {
        invitation.validate()?;
        let mut nodes = self.nodes.lock().unwrap();
        ensure!(nodes.len() < 1000, "Contributor limit reached");
        if let Some(binding) = &host_binding {
            ensure!(
                !nodes.values().any(|n| !n.revoked
                    && n.host_binding
                        .as_ref()
                        .is_some_and(|b| b.host_id == binding.host_id)),
                "This host already has a registration; renew its pending code or revoke it explicitly"
            );
        }
        let id = uuid::Uuid::new_v4().simple().to_string();
        let code = format!(
            "{id}.{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        // Allow the preinstalled WSL image to download and qualify on a home
        // connection. The invitation still binds exactly one node identity.
        let expires = now() + 3600;
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
            host_binding,
            engine_owner: None,
        };
        self.save(&node)?;
        let response = invitation_response(&node, &code);
        nodes.insert(id.clone(), node);
        Ok(response)
    }
    /// Renew an owner's unconsumed invitation without replacing the identity
    /// already saved by an installer before its first network request.
    pub fn renew_enrollment(&self, key: &str) -> Result<Value> {
        ensure!(id(key), "Invalid registration identity");
        let mut nodes = self.nodes.lock().unwrap();
        let mut n = nodes.get(key).cloned().context("Unknown registration")?;
        ensure!(
            !n.revoked && n.secret_hash.is_none(),
            "Only an unrevoked, unenrolled registration can be renewed"
        );
        let code = format!(
            "{key}.{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        n.code_hash = secret_hash(&code);
        n.expires = now() + 3600;
        self.save(&n)?;
        let response = invitation_response(&n, &code);
        nodes.insert(key.to_owned(), n);
        Ok(response)
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
            n.host_binding.is_none(),
            "Use the managed Incus enrollment flow for this invitation"
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
    fn invitation_node(&self, code: &str) -> Result<Node> {
        let (key, secret) = code.split_once('.').context("Invalid registration code")?;
        ensure!(id(key) && hash(secret), "Invalid registration code");
        let n = self
            .nodes
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .context("Unknown registration")?;
        ensure!(
            !n.revoked && n.expires > now() && equal(&n.code_hash, &secret_hash(code)),
            "Registration code expired or invalid"
        );
        Ok(n)
    }
    pub fn host_info(&self, code: &str) -> Result<Value> {
        let n = self.invitation_node(code)?;
        let binding = n.host_binding.context("Not an Incus invitation")?;
        Ok(json!({"id":n.id,"kind":"incus","binding":binding,"budget":n.budget}))
    }
    pub fn replacement_host_info(&self, code: &str, body: &Value) -> Result<Value> {
        ensure!(
            body["kind"] == "incus-replace-info",
            "Invalid replacement kind"
        );
        ensure!(
            body.as_object().is_some_and(|o| o.len() == 3
                && o.contains_key("kind")
                && o.contains_key("previous_id")
                && o.contains_key("previous_secret_hash")),
            "Invalid replacement request"
        );
        let previous_id = body["previous_id"]
            .as_str()
            .context("Previous identity required")?;
        let previous_secret = body["previous_secret_hash"]
            .as_str()
            .context("Previous credential required")?;
        ensure!(
            id(previous_id) && hash(previous_secret),
            "Invalid previous identity"
        );
        let n = self.invitation_node(code)?;
        let binding = n.host_binding.as_ref().context("Not an Incus invitation")?;
        ensure!(
            n.secret_hash.is_none(),
            "Replacement invitation has already been consumed"
        );
        let nodes = self.nodes.lock().unwrap();
        let previous = nodes
            .get(previous_id)
            .context("Previous registration unavailable")?;
        let old_binding = previous
            .host_binding
            .as_ref()
            .context("Previous registration is not an Incus host")?;
        ensure!(
            previous.id != n.id
                && previous.revoked
                && old_binding.host_id == binding.host_id
                && old_binding.machine_id_hash == binding.machine_id_hash,
            "Only a revoked registration for this same machine can be replaced"
        );
        // Unconsumed invitations have no persisted credential hash yet. Their
        // explicit owner revocation and immutable machine binding authorize
        // replacement of a pending local registration as well.
        ensure!(
            previous
                .secret_hash
                .as_ref()
                .is_none_or(|hash| equal(hash, previous_secret)),
            "Previous credential does not match the revoked registration"
        );
        Ok(
            json!({"id":n.id,"kind":"incus","binding":binding,"budget":n.budget,"previous_id":previous.id}),
        )
    }
    pub fn enroll_host(&self, code: &str, body: &Value) -> Result<Value> {
        ensure!(body["kind"] == "incus", "Invalid enrollment kind");
        ensure!(
            body.as_object().is_some_and(|o| o.len() == 3
                && o.contains_key("kind")
                && o.contains_key("secret_hash")
                && o.contains_key("machine_id_hash")),
            "Invalid Incus enrollment"
        );
        let mut n = self.invitation_node(code)?;
        let binding = n.host_binding.as_ref().context("Not an Incus invitation")?;
        let secret = body["secret_hash"]
            .as_str()
            .context("Node secret hash required")?;
        ensure!(
            hash(secret) && body["machine_id_hash"] == binding.machine_id_hash,
            "This invitation belongs to a different machine"
        );
        let mut nodes = self.nodes.lock().unwrap();
        // Re-read under the commit lock so concurrent revocation/enrollment cannot be lost.
        n = nodes.get(&n.id).cloned().context("Unknown registration")?;
        ensure!(
            !n.revoked && n.expires > now() && equal(&n.code_hash, &secret_hash(code)),
            "Registration code expired or revoked"
        );
        if let Some(bound) = &n.secret_hash {
            ensure!(equal(bound, secret), "Registration code already consumed");
        } else {
            n.secret_hash = Some(secret.to_owned());
            n.os = "linux".into();
            n.architecture = "x86_64".into();
            self.save(&n)?;
        }
        let response = json!({"id":n.id,"enrolled":true,"kind":"incus","binding":n.host_binding,"budget":n.budget});
        nodes.insert(n.id.clone(), n);
        Ok(response)
    }
    /// Registration is an admission lease, not permission to kill existing VMs.
    pub fn assert_host(&self, host: &enginebox_core::Host) -> Result<()> {
        let nodes = self.nodes.lock().unwrap();
        let n = nodes
            .values()
            .find(|n| {
                !n.revoked
                    && n.host_binding
                        .as_ref()
                        .is_some_and(|b| b.host_id == host.id)
            })
            .context("Managed host has no active registration")?;
        let binding = n.host_binding.as_ref().unwrap();
        ensure!(
            binding.binding_digest == state::digest(&host.binding()),
            "Host configuration changed; renew its approved binding"
        );
        ensure!(
            n.secret_hash.is_some()
                && !n.blocked
                && n.last_seen + LEASE_SECONDS > now()
                && n.tools["host_ready"] == true
                && n.tools["backend"] == "incus"
                && n.tools["binding_digest"] == binding.binding_digest,
            "Managed host is offline, unprepared or fenced"
        );
        Ok(())
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
    /// True for a registered (not revoked) node ID: an org can own it (docs/ORGS.md).
    pub fn knows(&self, id: &str) -> bool {
        self.nodes
            .lock()
            .unwrap()
            .get(id)
            .is_some_and(|n| !n.revoked)
    }
    pub fn catalog(&self) -> Value {
        let rows=self.nodes.lock().unwrap().values().map(|n|json!({"id":n.id,"label":n.invitation.label,
            "budget":n.budget,"workloads":n.invitation.workloads,"engine_owner":n.engine_owner,"os":n.os,"architecture":n.architecture,
            "enrolled":n.secret_hash.is_some(),"revoked":n.revoked,"blocked":n.blocked,"active_job":n.active,
            "online":!n.revoked && n.last_seen+LEASE_SECONDS>now(),"last_seen":n.last_seen,"tools":n.tools,
            "kind":if n.host_binding.is_some() {"incus"} else {"workspace"},"host_id":n.host_binding.as_ref().map(|b| &b.host_id),"binding_digest":n.host_binding.as_ref().map(|b| &b.binding_digest),
            "disk_enforcement":if n.host_binding.is_some() { "Incus admission and configured host reserves" } else if n.os == "linux" { "WSL virtual disk maximum" } else { "workspace usage watchdog" },"cpu_memory_enforcement":if n.host_binding.is_some() { "Incus VM limits and configured host reserves" } else if n.os == "linux" { "WSL runtime cgroup" } else { "Windows Job Object (legacy)" }})).collect::<Vec<_>>();
        json!({"nodes":rows})
    }
    /// Record which member's personal machine an engine node is
    /// (`POST /v1/contributors/{id}/engine-owner {member}`; owner token, the
    /// dashboard sends the claiming admin). Engine sessions of other members
    /// are never placed on it.
    pub fn set_engine_owner(&self, key: &str, body: &Value) -> Result<Value> {
        crate::keys(body, &["member"], &["member"])?;
        let member = body["member"]
            .as_str()
            .context("member must be a user ID")?;
        ensure!(
            crate::audit::valid_actor(member) && member != "owner",
            "member must be a user ID"
        );
        let mut nodes = self.nodes.lock().unwrap();
        let n = nodes.get_mut(key).context("Unknown node")?;
        ensure!(
            !n.revoked && n.invitation.workloads.engine && n.host_binding.is_none(),
            "Only an engine machine has an engine owner"
        );
        n.engine_owner = Some(member.to_owned());
        self.save(n)?;
        Ok(json!({"id": n.id, "engine_owner": n.engine_owner}))
    }
    /// Engine machines as session placement sees them.
    pub fn engine_nodes(&self) -> Vec<EngineNode> {
        self.nodes
            .lock()
            .unwrap()
            .values()
            .filter(|n| n.invitation.workloads.engine && n.host_binding.is_none())
            .map(|n| EngineNode {
                id: n.id.clone(),
                engine_owner: n.engine_owner.clone(),
                revoked: n.revoked,
                online: n.secret_hash.is_some() && n.last_seen + LEASE_SECONDS > now(),
                windows: n.os == "windows",
                epoch: n.epoch,
                tools: n.tools["engine"].clone(),
            })
            .collect()
    }
    /// Authenticate and fence an `engine-poll` / `engine-report`: the node's
    /// current session and epoch, an engine machine, with a fresh heartbeat.
    /// Returns (node id, epoch). Never reads or changes the job slot.
    pub fn engine_channel(&self, token: &str, body: &Value) -> Result<(String, u64)> {
        ensure!(self.authenticated(token), "Invalid node credential");
        let key = token.split('.').next().unwrap();
        let nodes = self.nodes.lock().unwrap();
        let n = nodes.get(key).context("Unknown node")?;
        let session = body["session"].as_str().context("Session required")?;
        ensure!(
            !n.revoked
                && n.host_binding.is_none()
                && n.invitation.workloads.engine
                && n.session == session
                && body["epoch"].as_u64() == Some(n.epoch),
            "Stale node session"
        );
        ensure!(n.last_seen + LEASE_SECONDS > now(), "Heartbeat required");
        Ok((n.id.clone(), n.epoch))
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
                json!({"id":n.id,"session":n.session,"epoch":n.epoch,"budget":n.budget,"workloads":n.invitation.workloads,"blocked":n.blocked,"kind":if n.host_binding.is_some() {"incus"} else {"workspace"},"binding":n.host_binding}),
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
                if let Some(binding) = &n.host_binding {
                    ensure!(
                        tools["backend"] == "incus"
                            && tools["binding_digest"] == binding.binding_digest
                            && tools["host_id"] == binding.host_id
                            && tools["host_ready"].is_boolean()
                            && tools["worker_running"] == false,
                        "Invalid managed host report"
                    );
                }
                n.tools = tools.clone();
                n.last_seen = now();
                self.save(n)?;
                Ok(json!({"ok":true,"blocked":n.blocked}))
            }
            "poll" => {
                ensure!(
                    n.host_binding.is_none(),
                    "Managed Incus work uses the approved host adapter"
                );
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
                n.host_binding.is_none(),
                "Use the managed Incus host adapter for this node"
            );
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
                    anyhow::bail!("Host agent execution is disabled. Use a WSL template workspace.")
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
                engine: false,
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
    fn managed_host() -> enginebox_core::Host {
        enginebox_core::Host {
            id: "metal".into(),
            label: "Server".into(),
            target: "local".into(),
            managed_registration: true,
            ..Default::default()
        }
    }
    fn host_connection(service: &Contributors, token: &str, binding: &Value) -> Value {
        let session = uuid::Uuid::new_v4().to_string();
        let hello = service
            .node_call(token, "hello", json!({"session":session}))
            .unwrap();
        assert_eq!(hello["kind"], "incus");
        assert_eq!(hello["binding"], *binding);
        let body = json!({"session":session,"epoch":hello["epoch"],"tools":{"backend":"incus","host_id":"metal","binding_digest":binding["binding_digest"],"host_ready":true,"worker_running":false}});
        service.node_call(token, "heartbeat", body.clone()).unwrap();
        body
    }
    #[test]
    fn incus_invitation_binds_an_approved_machine_and_never_becomes_a_second_executor() {
        let root = tempfile::tempdir().unwrap();
        let service = Contributors::open(root.path()).unwrap();
        let host = managed_host();
        let machine = "b".repeat(64);
        let mut legacy = host.clone();
        legacy.managed_registration = false;
        assert!(
            service
                .issue_host(&legacy, machine.clone(), invitation().budget)
                .is_err()
        );
        let issued = service
            .issue_host(&host, machine.clone(), invitation().budget)
            .unwrap();
        assert!(
            service
                .issue_host(&host, machine.clone(), invitation().budget)
                .is_err()
        );
        let code = issued["code"].as_str().unwrap();
        let id = issued["id"].as_str().unwrap();
        assert_eq!(issued["kind"], "incus");
        assert_eq!(issued["host_id"], "metal");
        let info = service.host_info(code).unwrap();
        let token = format!("{id}.{}", "d".repeat(64));
        assert!(service.assert_host(&host).is_err());
        assert!(
            service
                .enroll(
                    code,
                    Enrollment {
                        secret_hash: secret_hash(&token),
                        budget: invitation().budget,
                        os: "linux".into(),
                        architecture: "x86_64".into()
                    }
                )
                .is_err()
        );
        let mut request = json!({"kind":"incus","secret_hash":secret_hash(&token),"machine_id_hash":"c".repeat(64)});
        assert!(service.enroll_host(code, &request).is_err());
        request["machine_id_hash"] = json!(machine);
        let enrolled = service.enroll_host(code, &request).unwrap();
        assert_eq!(enrolled, service.enroll_host(code, &request).unwrap());
        request["secret_hash"] = json!("e".repeat(64));
        assert!(service.enroll_host(code, &request).is_err());
        assert!(service.assert_host(&host).is_err());
        let body = host_connection(&service, &token, &info["binding"]);
        assert!(service.assert_host(&host).is_ok());
        assert!(service.node_call(&token, "poll", body).is_err());
        let jobs = crate::jobs::Jobs::open(&root.path().join("jobs")).unwrap();
        assert!(
            service
                .submit(
                    &jobs,
                    id,
                    Run {
                        request_id: "no-host-exec".into(),
                        kind: "workspace".into(),
                        request: json!({"operation":"fork","workspace":"proof"})
                    }
                )
                .is_err()
        );
        assert_eq!(service.catalog()["nodes"][0]["kind"], "incus");
        assert_eq!(service.catalog()["nodes"][0]["host_id"], "metal");
    }
    #[test]
    fn managed_admission_fences_offline_revoked_unprepared_changed_and_restarted_nodes() {
        let root = tempfile::tempdir().unwrap();
        let service = Contributors::open(root.path()).unwrap();
        let host = managed_host();
        let issued = service
            .issue_host(&host, "b".repeat(64), invitation().budget)
            .unwrap();
        let id = issued["id"].as_str().unwrap();
        let code = issued["code"].as_str().unwrap();
        let token = format!("{id}.{}", "d".repeat(64));
        let info = service.host_info(code).unwrap();
        service.enroll_host(code,&json!({"kind":"incus","secret_hash":secret_hash(&token),"machine_id_hash":"b".repeat(64)})).unwrap();
        let mut body = host_connection(&service, &token, &info["binding"]);
        body["tools"]["host_ready"] = json!(false);
        service
            .node_call(&token, "heartbeat", body.clone())
            .unwrap();
        assert!(service.assert_host(&host).is_err());
        body["tools"]["host_ready"] = json!(true);
        service.node_call(&token, "heartbeat", body).unwrap();
        assert!(service.assert_host(&host).is_ok());
        let mut changed = host.clone();
        changed.pool = "different".into();
        assert!(service.assert_host(&changed).is_err());
        service.nodes.lock().unwrap().get_mut(id).unwrap().last_seen = 0;
        assert!(service.assert_host(&host).is_err());
        host_connection(&service, &token, &info["binding"]);
        let restarted = Contributors::open(root.path()).unwrap();
        assert!(restarted.assert_host(&host).is_err());
        host_connection(&restarted, &token, &info["binding"]);
        assert!(restarted.assert_host(&host).is_ok());
        restarted.revoke(id).unwrap();
        assert!(restarted.assert_host(&host).is_err());
        assert!(!restarted.authenticated(&token));
        assert!(
            restarted
                .issue_host(&host, "b".repeat(64), invitation().budget)
                .is_ok()
        );
    }
    #[test]
    fn replacement_requires_revocation_same_machine_and_the_previous_credential() {
        let root = tempfile::tempdir().unwrap();
        let service = Contributors::open(root.path()).unwrap();
        let host = managed_host();
        let old = service
            .issue_host(&host, "b".repeat(64), invitation().budget)
            .unwrap();
        let old_id = old["id"].as_str().unwrap();
        let token = format!("{old_id}.{}", "d".repeat(64));
        service.enroll_host(old["code"].as_str().unwrap(), &json!({"kind":"incus","secret_hash":secret_hash(&token),"machine_id_hash":"b".repeat(64)})).unwrap();
        let request = json!({"kind":"incus-replace-info","previous_id":old_id,"previous_secret_hash":secret_hash(&token)});
        // An existing live identity and another server's invitation never
        // authorize a reset of the saved credential.
        assert!(
            service
                .replacement_host_info(old["code"].as_str().unwrap(), &request)
                .is_err()
        );
        let mut other_host = host.clone();
        other_host.id = "other".into();
        let other = service
            .issue_host(&other_host, "b".repeat(64), invitation().budget)
            .unwrap();
        assert!(
            service
                .replacement_host_info(other["code"].as_str().unwrap(), &request)
                .is_err()
        );
        service.revoke(old_id).unwrap();
        assert!(
            service
                .replacement_host_info(other["code"].as_str().unwrap(), &request)
                .is_err()
        );
        let next = service
            .issue_host(&host, "b".repeat(64), invitation().budget)
            .unwrap();
        let code = next["code"].as_str().unwrap();
        let info = service.replacement_host_info(code, &request).unwrap();
        assert_eq!(info["previous_id"], old_id);
        assert_eq!(info["id"], next["id"]);
        let mut forged = request.clone();
        forged["previous_secret_hash"] = json!("e".repeat(64));
        assert!(service.replacement_host_info(code, &forged).is_err());
        service.enroll_host(code, &json!({"kind":"incus","secret_hash":"a".repeat(64),"machine_id_hash":"b".repeat(64)})).unwrap();
        assert!(service.replacement_host_info(code, &request).is_err());
        service.revoke(next["id"].as_str().unwrap()).unwrap();
        let changed_machine = service
            .issue_host(&host, "c".repeat(64), invitation().budget)
            .unwrap();
        assert!(
            service
                .replacement_host_info(changed_machine["code"].as_str().unwrap(), &request)
                .is_err()
        );
        // The owner can also retire a never-consumed pending installation.
        service.revoke(other["id"].as_str().unwrap()).unwrap();
        let pending_next = service
            .issue_host(&other_host, "b".repeat(64), invitation().budget)
            .unwrap();
        assert!(service.replacement_host_info(pending_next["code"].as_str().unwrap(), &json!({"kind":"incus-replace-info","previous_id":other["id"],"previous_secret_hash":"f".repeat(64)})).is_ok());
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
    fn renewal_preserves_pending_identity_and_invalidates_old_code_durably() {
        let tmp = tempfile::tempdir().unwrap();
        let service = Contributors::open(tmp.path()).unwrap();
        let issued = service.issue(invitation()).unwrap();
        let key = issued["id"].as_str().unwrap();
        let token = format!("{key}.{}", "c".repeat(64));
        let enrollment = Enrollment {
            secret_hash: secret_hash(&token),
            budget: Budget {
                cpu: 2,
                memory_gib: 4,
                disk_gib: 20,
            },
            os: "linux".into(),
            architecture: "x86_64".into(),
        };
        service.nodes.lock().unwrap().get_mut(key).unwrap().expires = 0;
        assert!(
            service
                .enroll(issued["code"].as_str().unwrap(), enrollment.clone())
                .is_err()
        );
        let before = service.catalog();
        let renewed = service.renew_enrollment(key).unwrap();
        assert_eq!(renewed["id"], issued["id"]);
        assert_ne!(renewed["code"], issued["code"]);
        assert!(renewed["expires"].as_u64().unwrap() > now());
        assert_eq!(service.catalog(), before);
        drop(service);
        let service = Contributors::open(tmp.path()).unwrap();
        assert!(
            service
                .enroll(issued["code"].as_str().unwrap(), enrollment.clone())
                .is_err()
        );
        service
            .enroll(renewed["code"].as_str().unwrap(), enrollment)
            .unwrap();
        assert!(service.authenticated(&token));
        assert!(!service.authenticated(renewed["code"].as_str().unwrap()));
        assert!(service.renew_enrollment(key).is_err());
        assert!(service.authenticated(&token));
        let revoked = service.issue(invitation()).unwrap();
        let revoked_id = revoked["id"].as_str().unwrap();
        service.revoke(revoked_id).unwrap();
        assert!(service.renew_enrollment(revoked_id).is_err());
        assert!(service.renew_enrollment(&"f".repeat(32)).is_err());
        assert!(service.renew_enrollment("../invalid").is_err());
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
