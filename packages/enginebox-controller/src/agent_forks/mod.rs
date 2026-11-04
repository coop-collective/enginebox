//! Agent forks on a live engine session's dev tree (docs/ENGINE-SESSIONS.md,
//! "Agent forks"; contract 2026-11-04 §5, owner decisions 1-4).
//!
//! A **fork** is a separate writable copy of the session's dev tree, owned by
//! one principal, where that principal's agent runs (Claude Code or Codex)
//! work. A **submission** freezes a fork head, checks it (path classes, the
//! secret scan, size limits), waits for the room owner when any path is code
//! or protected, three-way merges it into the dev head and applies it to the
//! live tree, which the Editor hot-refreshes. A merge that breaks compilation
//! is reverted automatically and the author and owner are told why.
//!
//! The controller owns records and policy; the node does the work over the
//! engine channel: every `engine-poll` session row gains `forks` (desired
//! state and pending ops; a run op carries its xinf access token in
//! `secrets.gateway`, memory only) and `dev`; the node answers through
//! `fork-report`, `fork-events` and `fork-artifact`, and `engine-report`'s
//! `detail.dev` (dev head, compile state, live code edits). Reports are
//! fenced by node, epoch and the fork's generation.
//!
//! Callers: the room owner's `agent-forks` grant acting as the owner, or for
//! a room principal the app vouched for with a signed assertion
//! (`principal.rs`); members through the dashboard; the owner token. Room
//! principals see and drive only their own forks, use only their own xinf
//! connection, and never review, revert or publish.
//!
//! Records: `engine-sessions/forks/<fork>.json`,
//! `engine-sessions/fork-submissions/<id>.json`, `engine-sessions/dev/<session>.json`,
//! run events in `engine-sessions/fork-events/`, diffs in
//! `engine-sessions/fork-diffs/` (directories older releases do not read).
pub mod classify;
pub mod gateway;
pub mod principal;

use anyhow::{Context, Result, ensure};
use classify::{Class, Finding};
use enginebox_core::state;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// Open forks one principal holds in one session (owner default: 3).
pub const MAX_FORKS_PER_PRINCIPAL: usize = 3;
/// Concurrent agent runs on one engine machine (owner decision: 3).
pub const MAX_SLOTS_PER_MACHINE: u64 = 3;
pub const DEFAULT_BUDGET_USD: f64 = 2.0;
pub const MAX_BUDGET_USD: f64 = 20.0;
pub const MIN_BUDGET_USD: f64 = 0.1;
/// A run's wall-clock limit (owner default: 30 minutes).
pub const RUN_SECONDS: u64 = 30 * 60;
/// After the limit, the run is failed and cancelled on the node.
const RUN_GRACE_SECONDS: u64 = 60;
pub const MAX_PROMPT_BYTES: usize = 16 * 1024;
const MAX_RUNS_PER_FORK: usize = 50;
const MAX_FORK_RECORDS: usize = 10_000;
const MAX_EVENTS_BYTES: usize = 512 * 1024;
const MAX_EVENT_BYTES: usize = 16 * 1024;
pub const MAX_DIFF_BYTES: usize = 2 * 1024 * 1024;
const MAX_NOTICES: usize = 20;
const MAX_MERGE_ATTEMPTS: u32 = 3;
/// Below this balance a run cannot start.
const MIN_BALANCE_USD: f64 = 0.05;

pub const GATEWAY_REQUIRED: &str = "Connect Xava Inference and add balance to run agents.";
pub const NOT_READY: &str = "The live engine session is not ready; agents can fork it once it is.";
pub const NO_FORKS: &str =
    "This engine machine cannot run agent forks yet (it reports no fork provider).";

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}
pub fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
/// A Git commit id (SHA-1 or SHA-256, lower-case hex).
pub fn valid_commit(c: &str) -> bool {
    (c.len() == 40 || c.len() == 64)
        && c.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A caller-facing refusal: HTTP status, message and, for the refusals a
/// client acts on, a machine-readable `code`.
#[derive(Debug)]
pub struct Refusal {
    pub status: u16,
    pub message: String,
    pub code: Option<&'static str>,
    pub extra: Value,
}
impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Refusal {}
fn refuse(status: u16, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(Refusal {
        status,
        message: message.into(),
        code: None,
        extra: Value::Null,
    })
}
fn coded(status: u16, code: &'static str, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(Refusal {
        status,
        message: message.into(),
        code: Some(code),
        extra: Value::Null,
    })
}
/// The status and body of an error for a caller.
pub fn status_of(error: &anyhow::Error) -> (u16, Value) {
    if let Some(r) = error.downcast_ref::<Refusal>() {
        let mut body = json!({"error": r.message});
        if let Some(code) = r.code {
            body["code"] = json!(code);
        }
        if let Some(extra) = r.extra.as_object() {
            for (k, v) in extra {
                body[k] = v.clone();
            }
        }
        return (r.status, body);
    }
    if let Some(r) = error.downcast_ref::<crate::engine_sessions::Refusal>() {
        return (r.status, json!({"error": r.message}));
    }
    if let Some(r) = error.downcast_ref::<principal::Refused>() {
        return (401, json!({"error": r.0}));
    }
    (400, json!({"error": crate::catalog::message(error)}))
}

// ------------------------------------------------------------------ callers

/// Who is calling an agent-fork route.
#[derive(Clone, Debug)]
pub enum Caller {
    /// The owner token without a member (fleet administration).
    Owner,
    /// An org member: through the dashboard, or a grant acting as its member.
    Member(String),
    /// A room member the grant's app vouched for.
    Room(principal::Room),
}
impl Caller {
    /// The fork owner key (who a fork belongs to).
    fn key(&self) -> String {
        match self {
            Self::Owner => "owner".into(),
            Self::Member(m) => m.clone(),
            Self::Room(r) => format!("{}@{}", r.principal(), r.client),
        }
    }
    /// The person whose xinf connection pays (the same across an app's rooms).
    pub fn person(&self) -> String {
        match self {
            Self::Owner => "owner".into(),
            Self::Member(m) => m.clone(),
            Self::Room(r) => format!("gj:{}@{}", r.user, r.client),
        }
    }
    fn owner(&self) -> Owner {
        let (principal, name) = match self {
            Self::Owner => ("owner".to_owned(), "Owner".to_owned()),
            Self::Member(m) => (m.clone(), m.clone()),
            Self::Room(r) => (r.principal(), r.name.clone()),
        };
        Owner {
            key: self.key(),
            principal,
            name,
            person: self.person(),
        }
    }
    fn builder(&self) -> bool {
        !matches!(self, Self::Room(r) if r.role == principal::Role::Viewer)
    }
    /// The audit actor.
    fn actor(&self) -> String {
        match self {
            Self::Room(r) => format!("{}@{}", r.principal(), r.client),
            other => other.key(),
        }
    }
}

// ------------------------------------------------------------------ records

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Owner {
    pub key: String,
    pub principal: String,
    pub name: String,
    /// Whose xinf connection pays for this fork's runs.
    pub person: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForkState {
    Creating,
    Idle,
    Running,
    Submitted,
    Closing,
    Closed,
    Failed,
}
impl ForkState {
    fn open(self) -> bool {
        !matches!(self, Self::Closing | Self::Closed | Self::Failed)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Queued,
    Dispatched,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}
impl RunState {
    fn active(self) -> bool {
        matches!(self, Self::Queued | Self::Dispatched | Self::Running)
    }
    fn on_node(self) -> bool {
        matches!(self, Self::Dispatched | Self::Running)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Run {
    pub id: String,
    pub request_id: String,
    pub request_digest: String,
    pub state: RunState,
    pub provider: String,
    #[serde(default)]
    pub model: Option<String>,
    pub budget_usd: f64,
    pub timeout_seconds: u64,
    #[serde(default)]
    pub resume: bool,
    pub created_at: u64,
    #[serde(default)]
    pub dispatched_at: Option<u64>,
    #[serde(default)]
    pub started_at: Option<u64>,
    #[serde(default)]
    pub ended_at: Option<u64>,
    #[serde(default)]
    pub spent_usd: f64,
    #[serde(default)]
    pub requests: u64,
    #[serde(default)]
    pub exit: Option<i64>,
    #[serde(default)]
    pub error: Option<String>,
}
impl Run {
    fn view(&self) -> Value {
        json!({
            "id": self.id, "state": self.state, "provider": self.provider, "model": self.model,
            "budget_usd": self.budget_usd, "spent_usd": self.spent_usd, "requests": self.requests,
            "resume": self.resume, "timeout_seconds": self.timeout_seconds,
            "created_at": self.created_at, "started_at": self.started_at, "ended_at": self.ended_at,
            "exit": self.exit, "error": self.error,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    Run,
    Snapshot,
    Merge,
    Rebase,
    Revert,
    Cancel,
    Publish,
}

/// Work for the node. `body` holds the kind's payload exactly as the node
/// receives it (`run`, `snapshot`, `merge`, `rebase`, `revert`, `cancel`,
/// `publish`); secrets are added at poll time, never stored.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Op {
    pub op_id: String,
    pub kind: OpKind,
    pub created_at: u64,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub submission: Option<String>,
    #[serde(default)]
    pub body: Value,
}
impl Op {
    fn new(kind: OpKind, body: Value) -> Self {
        Self {
            op_id: new_id(),
            kind,
            created_at: now(),
            run_id: None,
            submission: None,
            body,
        }
    }
    fn wire(&self) -> Value {
        let mut v = json!({"op_id": self.op_id, "kind": self.kind});
        if let Some(object) = self.body.as_object() {
            for (k, x) in object {
                v[k] = x.clone();
            }
        }
        v
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Change {
    pub path: String,
    pub status: String,
    #[serde(default)]
    pub mode: u32,
    #[serde(default)]
    pub lfs: bool,
    #[serde(default)]
    pub bytes: u64,
    pub class: Class,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fork {
    pub id: String,
    pub org_id: String,
    pub project: String,
    pub session: String,
    pub generation: u64,
    pub owner: Owner,
    pub title: String,
    #[serde(default)]
    pub paths: Vec<String>,
    pub state: ForkState,
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub head: Option<String>,
    #[serde(default)]
    pub changed: Vec<Change>,
    #[serde(default)]
    pub findings: Vec<Finding>,
    #[serde(default)]
    pub runs: Vec<Run>,
    #[serde(default)]
    pub ops: Vec<Op>,
    /// Recently finished op ids (repeated reports are answered, not redone).
    #[serde(default)]
    pub done_ops: Vec<String>,
    #[serde(default)]
    pub submission: Option<String>,
    pub request_id: String,
    pub request_digest: String,
    #[serde(default)]
    pub native_session_id: Option<String>,
    #[serde(default)]
    pub rebase_conflict: Vec<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub keep_snapshot: bool,
    /// The head the stored diff artifact belongs to.
    #[serde(default)]
    pub diff_head: Option<String>,
    /// The secret scan of that diff, made before it was stored (the stored
    /// copy has Engine Box's own bearers redacted).
    #[serde(default)]
    pub diff_findings: Vec<Finding>,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default)]
    pub closed_at: Option<u64>,
}
impl Fork {
    fn active_run(&self) -> Option<&Run> {
        self.runs.iter().find(|r| r.state.active())
    }
    fn summary(&self) -> Value {
        json!({
            "id": self.id, "owner": {"principal": self.owner.principal, "name": self.owner.name},
            "title": self.title, "state": self.state, "base": self.base, "head": self.head,
            "paths": self.paths, "paths_touched_count": self.changed.len(),
            "updated_at": self.updated_at,
        })
    }
    /// What another member sees: a soft claim, no diff, no head.
    fn claim(&self) -> Value {
        json!({
            "id": self.id, "owner": {"name": self.owner.name}, "title": self.title,
            "state": self.state, "paths": self.paths,
        })
    }
    fn view(&self, submission: Option<&Submission>) -> Value {
        json!({
            "id": self.id, "session": self.session,
            "owner": {"principal": self.owner.principal, "name": self.owner.name},
            "title": self.title, "paths": self.paths, "state": self.state,
            "base": self.base, "head": self.head,
            "changed": self.changed.iter().map(|c| json!({"path": c.path, "status": c.status, "class": c.class, "bytes": c.bytes, "lfs": c.lfs})).collect::<Vec<_>>(),
            "runs": self.runs.iter().rev().take(20).map(Run::view).collect::<Vec<_>>(),
            "submission": submission.map(Submission::view),
            "rebase_conflict": self.rebase_conflict, "error": self.error,
            "has_diff": self.diff_head.is_some() && self.diff_head == self.head,
            "created_at": self.created_at, "updated_at": self.updated_at, "closed_at": self.closed_at,
        })
    }
    fn finish_op(&mut self, op_id: &str) -> Option<Op> {
        let at = self.ops.iter().position(|o| o.op_id == op_id)?;
        let op = self.ops.remove(at);
        self.done_ops.push(op.op_id.clone());
        if self.done_ops.len() > 64 {
            self.done_ops.remove(0);
        }
        Some(op)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubState {
    Checking,
    Rejected,
    Conflict,
    AwaitingApproval,
    Merging,
    Applying,
    Held,
    Applied,
    Failed,
}
impl SubState {
    /// Still on its way into the dev tree.
    fn open(self) -> bool {
        matches!(
            self,
            Self::Checking | Self::AwaitingApproval | Self::Merging | Self::Applying | Self::Held
        )
    }
    fn name(self) -> &'static str {
        match self {
            Self::Checking => "checking",
            Self::Rejected => "rejected",
            Self::Conflict => "conflict",
            Self::AwaitingApproval => "awaiting_approval",
            Self::Merging => "merging",
            Self::Applying => "applying",
            Self::Held => "held",
            Self::Applied => "applied",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reverted {
    /// `reverting`, `reverted` or `failed`.
    pub state: String,
    pub auto: bool,
    pub by: String,
    pub reason: String,
    #[serde(default)]
    pub errors: Vec<Value>,
    pub at: u64,
    #[serde(default)]
    pub dev_head: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Submission {
    pub id: String,
    pub org_id: String,
    pub project: String,
    pub session: String,
    pub fork: String,
    pub owner: Owner,
    pub message: String,
    pub request_id: String,
    pub request_digest: String,
    pub fork_head: String,
    pub state: SubState,
    #[serde(default)]
    pub classes: Value,
    #[serde(default)]
    pub findings: Vec<Finding>,
    /// Classes that may merge without the owner (room policy, ⊆ config,
    /// scene, asset).
    pub auto_merge: Vec<Class>,
    #[serde(default)]
    pub needs_approval: bool,
    #[serde(default)]
    pub approved_by: Option<String>,
    #[serde(default)]
    pub approve_request: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub conflict_paths: Vec<String>,
    #[serde(default)]
    pub held_reason: Option<String>,
    #[serde(default)]
    pub dev_head_before: Option<String>,
    #[serde(default)]
    pub dev_head_after: Option<String>,
    #[serde(default)]
    pub merge_attempts: u32,
    #[serde(default)]
    pub reverted: Option<Reverted>,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default)]
    pub applied_at: Option<u64>,
}
impl Submission {
    pub fn view(&self) -> Value {
        json!({
            "id": self.id, "fork": self.fork,
            "owner": {"principal": self.owner.principal, "name": self.owner.name},
            "message": self.message, "state": self.state, "fork_head": self.fork_head,
            "classes": self.classes, "findings": self.findings,
            "needs_approval": self.needs_approval, "approved_by": self.approved_by,
            "reason": self.reason, "conflict_paths": self.conflict_paths,
            "held_reason": self.held_reason,
            "dev_head_before": self.dev_head_before, "dev_head_after": self.dev_head_after,
            "reverted": self.reverted,
            "created_at": self.created_at, "updated_at": self.updated_at, "applied_at": self.applied_at,
        })
    }
}

/// The live dev tree of a session, as the node last reported it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dev {
    pub session: String,
    pub org_id: String,
    pub project: String,
    #[serde(default)]
    pub head: Option<String>,
    #[serde(default)]
    pub applied: Option<String>,
    #[serde(default)]
    pub applied_at: Option<u64>,
    #[serde(default)]
    pub held: Option<String>,
    #[serde(default)]
    pub compile: Value,
    #[serde(default)]
    pub live_code_pending: Vec<String>,
    /// The last applied head that compiled.
    #[serde(default)]
    pub last_ok: Option<String>,
    #[serde(default)]
    pub notices: Vec<Value>,
    #[serde(default)]
    pub ops: Vec<Op>,
    #[serde(default)]
    pub done_ops: Vec<String>,
    #[serde(default)]
    pub published: Option<Value>,
    pub updated_at: u64,
}
impl Dev {
    fn new(session: &SessionInfo) -> Self {
        Self {
            session: session.id.clone(),
            org_id: session.org.clone(),
            project: session.project.clone(),
            head: None,
            applied: None,
            applied_at: None,
            held: None,
            compile: json!({"state": "unknown", "errors": []}),
            live_code_pending: vec![],
            last_ok: None,
            notices: vec![],
            ops: vec![],
            done_ops: vec![],
            published: None,
            updated_at: now(),
        }
    }
    fn notice(&mut self, notice: Value) {
        self.notices.push(notice);
        if self.notices.len() > MAX_NOTICES {
            self.notices.remove(0);
        }
    }
}

// ------------------------------------------------------------------ host

/// A session as forks see it.
#[derive(Clone, Debug)]
pub struct SessionInfo {
    pub id: String,
    pub org: String,
    pub project: String,
    pub repository: String,
    pub project_path: String,
    pub started_by: String,
    /// A Unity Editor session (not a world).
    pub editor: bool,
    pub ready: bool,
    /// Not ended, failed or lost.
    pub open: bool,
    pub node: Option<String>,
    pub node_epoch: u64,
    pub generation: u64,
}

/// What a node reports about forks (`tools.engine.forks`).
#[derive(Clone, Debug, Default)]
pub struct Caps {
    pub provider: String,
    pub slots: u64,
    /// `accounts`, `sandbox` or `none` (forks only for the session starter).
    pub isolation: String,
}
impl Caps {
    pub fn from_tools(tools: &Value) -> Option<Self> {
        let forks = tools.get("forks")?;
        let provider = forks["provider"].as_str()?;
        Some(Self {
            provider: provider.chars().take(16).collect(),
            slots: forks["slots"].as_u64().unwrap_or(0),
            isolation: forks["isolation"].as_str().unwrap_or("none").to_owned(),
        })
    }
}

/// A repository write credential for `publish`.
pub struct WriteAccess {
    pub remote: String,
    pub token: String,
    pub auth: String,
    pub expires_at: String,
}

/// What the controller provides to forks (lib.rs implements it).
pub trait Host {
    fn session(&self, id: &str) -> Option<SessionInfo>;
    /// The fork capabilities of an online node at its current epoch.
    fn caps(&self, node: &str) -> Option<Caps>;
    /// Engine Box's xinf OAuth client (`Ok(None)`: not configured).
    fn xinf(&self) -> Result<Option<Arc<dyn gateway::Xinf>>>;
    /// The https origin the dashboard serves (for owner-token `return_to`).
    fn dashboard_origin(&self) -> Option<String> {
        None
    }
    fn repository_write(&self, _session: &SessionInfo) -> Result<WriteAccess> {
        Err(refuse(
            409,
            "Publishing needs a repository Engine Box hosts or links (a write token); this session's repository has only a read token.",
        ))
    }
    fn audit(&self, org: &str, actor: &str, action: &str, target: &str, ok: bool, detail: Value);
}

// ------------------------------------------------------------------ routes

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    ListForks,
    CreateFork,
    GetFork,
    Diff,
    CreateRun,
    Events,
    CancelRun,
    Submit,
    Rebase,
    Close,
    ListSubmissions,
    Approve,
    Reject,
    Dev,
    Revert,
    Publish,
    GatewayConnect,
    GatewayGet,
    GatewayPut,
    GatewayDelete,
    Callback,
}
impl Action {
    /// The grant operation a service token needs.
    pub fn operation(self) -> Option<crate::service_tokens::Operation> {
        use crate::service_tokens::Operation as O;
        Some(match self {
            Self::ListForks | Self::ListSubmissions => O::ForksList,
            Self::CreateFork => O::ForksCreate,
            Self::GetFork | Self::Diff | Self::Events => O::ForksGet,
            Self::CreateRun | Self::CancelRun => O::ForksRun,
            Self::Submit => O::ForksSubmit,
            Self::Rebase => O::ForksRebase,
            Self::Close => O::ForksClose,
            Self::Dev => O::ForksDev,
            Self::Approve | Self::Reject => O::ForksReview,
            Self::Revert => O::ForksRevert,
            Self::Publish => O::ForksPublish,
            Self::GatewayConnect | Self::GatewayGet | Self::GatewayPut | Self::GatewayDelete => {
                O::ForksGateway
            }
            Self::Callback => return None,
        })
    }
    /// Reads take no body.
    pub fn read(self) -> bool {
        matches!(
            self,
            Self::ListForks
                | Self::GetFork
                | Self::Diff
                | Self::Events
                | Self::ListSubmissions
                | Self::Dev
                | Self::GatewayGet
                | Self::GatewayDelete
        )
    }
    /// Which query keys a route accepts.
    fn query_keys(self) -> &'static [&'static str] {
        match self {
            Self::ListForks => &["all"],
            Self::Diff => &["path", "limit"],
            Self::Events => &["after"],
            Self::ListSubmissions => &["state"],
            _ => &[],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub action: Action,
    pub project: String,
    pub session: Option<String>,
    pub fork: Option<String>,
    pub run: Option<String>,
    pub submission: Option<String>,
}

/// The agent-fork route `method path` names, if any.
pub fn route(method: &str, path: &str) -> Option<Route> {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if format!("/{}", parts.join("/")) != path {
        return None;
    }
    let r = |action, project: &str| Route {
        action,
        project: project.to_owned(),
        session: None,
        fork: None,
        run: None,
        submission: None,
    };
    use Action as A;
    let mut route = match (method, parts.as_slice()) {
        ("POST", ["v1", "gateway", "xinf", "callback"]) => return Some(r(A::Callback, "")),
        ("POST", ["v1", "projects", p, "gateway", "connect"]) => r(A::GatewayConnect, p),
        ("GET", ["v1", "projects", p, "gateway"]) => r(A::GatewayGet, p),
        ("PUT", ["v1", "projects", p, "gateway"]) => r(A::GatewayPut, p),
        ("DELETE", ["v1", "projects", p, "gateway"]) => r(A::GatewayDelete, p),
        (m, ["v1", "projects", p, "engine-sessions", s, rest @ ..]) => {
            let (action, fork, run, submission) = match (m, rest) {
                ("GET", ["forks"]) => (A::ListForks, None, None, None),
                ("POST", ["forks"]) => (A::CreateFork, None, None, None),
                ("GET", ["forks", f]) => (A::GetFork, Some(*f), None, None),
                ("GET", ["forks", f, "diff"]) => (A::Diff, Some(*f), None, None),
                ("POST", ["forks", f, "runs"]) => (A::CreateRun, Some(*f), None, None),
                ("GET", ["forks", f, "runs", run, "events"]) => {
                    (A::Events, Some(*f), Some(*run), None)
                }
                ("POST", ["forks", f, "runs", run, "cancel"]) => {
                    (A::CancelRun, Some(*f), Some(*run), None)
                }
                ("POST", ["forks", f, "submit"]) => (A::Submit, Some(*f), None, None),
                ("POST", ["forks", f, "rebase"]) => (A::Rebase, Some(*f), None, None),
                ("POST", ["forks", f, "close"]) => (A::Close, Some(*f), None, None),
                ("GET", ["submissions"]) => (A::ListSubmissions, None, None, None),
                ("POST", ["submissions", id, "approve"]) => (A::Approve, None, None, Some(*id)),
                ("POST", ["submissions", id, "reject"]) => (A::Reject, None, None, Some(*id)),
                ("GET", ["dev"]) => (A::Dev, None, None, None),
                ("POST", ["dev", "revert"]) => (A::Revert, None, None, None),
                ("POST", ["dev", "publish"]) => (A::Publish, None, None, None),
                _ => return None,
            };
            let mut route = r(action, p);
            route.session = Some((*s).to_owned());
            route.fork = fork.map(str::to_owned);
            route.run = run.map(str::to_owned);
            route.submission = submission.map(str::to_owned);
            route
        }
        _ => return None,
    };
    if !enginebox_agents::pools::valid_project(&route.project) {
        return None;
    }
    for id in [&route.session, &route.fork, &route.run, &route.submission]
        .into_iter()
        .flatten()
    {
        if !valid_id(id) {
            return None;
        }
    }
    route.project = route.project.clone();
    Some(route)
}

/// The query of a route as key → value, refusing keys the route does not take.
pub fn parse_query(action: Action, query: Option<&str>) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    let Some(query) = query.filter(|q| !q.is_empty()) else {
        return Ok(out);
    };
    ensure!(query.len() <= 512, refuse(400, "The query is too long."));
    for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
        ensure!(
            action.query_keys().contains(&k.as_ref()) && !out.contains_key(k.as_ref()),
            refuse(400, "Query parameters are not supported here.")
        );
        out.insert(k.into_owned(), v.into_owned());
    }
    Ok(out)
}

/// One agent-fork request, after authentication.
pub struct Request {
    pub route: Route,
    pub query: BTreeMap<String, String>,
    pub body: Value,
    pub org: String,
    pub caller: Caller,
    /// The member a service token's grant acts for (room principals reach
    /// only that member's sessions). None for the owner token.
    pub grant_member: Option<String>,
    /// https origins `return_to` may name (the OAuth client's redirect
    /// origins; the dashboard's own for owner-token callers).
    pub return_origins: Vec<String>,
}

/// A reply: JSON with a status, or a diff (`text/x-diff`).
pub enum Reply {
    Json(u16, Value),
    Diff(String),
}

// ------------------------------------------------------------------ store

#[derive(Default)]
struct Inner {
    forks: BTreeMap<String, Fork>,
    subs: BTreeMap<String, Submission>,
    devs: BTreeMap<String, Dev>,
}

#[derive(Clone)]
pub struct AgentForks {
    root: PathBuf,
    inner: Arc<Mutex<Inner>>,
    pub gateway: gateway::Gateway,
    verifier: principal::Verifier,
    /// Bearers delivered to each fork's runs, for exact-match secret scans
    /// and redaction (memory only).
    known: Arc<Mutex<BTreeMap<String, Vec<String>>>>,
    /// When each principal last had a run dispatched (round robin).
    last_dispatch: Arc<Mutex<BTreeMap<String, u64>>>,
    /// One write token per pending publish op (memory only).
    publish_tokens: Arc<Mutex<BTreeMap<String, Value>>>,
    events_lock: Arc<Mutex<()>>,
}

/// Write `bytes` to a private file (atomic rename).
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().context("no parent")?;
    state::private_dir(parent)?;
    let temp = parent.join(format!(".{}.tmp", new_id()));
    let mut options = std::fs::OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<()> {
        let mut f = options.write(true).create_new(true).open(&temp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&temp, path)?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&temp);
    result
}
fn read_private(path: &Path, max: u64) -> Option<Vec<u8>> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.len() > max {
        return None;
    }
    std::fs::read(path).ok()
}

impl AgentForks {
    pub fn open(root: &Path) -> Result<Self> {
        let base = root.join("engine-sessions");
        let mut inner = Inner::default();
        for (dir, kind) in [("forks", 0), ("fork-submissions", 1), ("dev", 2)] {
            let dir = base.join(dir);
            state::private_dir(&dir)?;
            for path in state::json_files(&dir)? {
                let value = state::read_json(&path)?;
                let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                match kind {
                    0 => {
                        let f: Fork = serde_json::from_value(value)?;
                        ensure!(valid_id(&f.id) && f.id == stem, "Invalid fork record");
                        inner.forks.insert(f.id.clone(), f);
                    }
                    1 => {
                        let s: Submission = serde_json::from_value(value)?;
                        ensure!(valid_id(&s.id) && s.id == stem, "Invalid submission record");
                        inner.subs.insert(s.id.clone(), s);
                    }
                    _ => {
                        let d: Dev = serde_json::from_value(value)?;
                        ensure!(
                            valid_id(&d.session) && d.session == stem,
                            "Invalid dev record"
                        );
                        inner.devs.insert(d.session.clone(), d);
                    }
                }
            }
        }
        for dir in ["fork-events", "fork-diffs"] {
            state::private_dir(&base.join(dir))?;
        }
        Ok(Self {
            root: base,
            inner: Arc::new(Mutex::new(inner)),
            gateway: gateway::Gateway::open(root)?,
            verifier: principal::Verifier::default(),
            known: Arc::new(Mutex::new(BTreeMap::new())),
            last_dispatch: Arc::new(Mutex::new(BTreeMap::new())),
            publish_tokens: Arc::new(Mutex::new(BTreeMap::new())),
            events_lock: Arc::new(Mutex::new(())),
        })
    }
    pub fn verifier(&self) -> &principal::Verifier {
        &self.verifier
    }
    fn save_fork(&self, f: &Fork) -> Result<()> {
        state::write_json(
            &self.root.join("forks").join(format!("{}.json", f.id)),
            &serde_json::to_value(f)?,
        )
    }
    fn save_sub(&self, s: &Submission) -> Result<()> {
        state::write_json(
            &self
                .root
                .join("fork-submissions")
                .join(format!("{}.json", s.id)),
            &serde_json::to_value(s)?,
        )
    }
    fn save_dev(&self, d: &Dev) -> Result<()> {
        state::write_json(
            &self.root.join("dev").join(format!("{}.json", d.session)),
            &serde_json::to_value(d)?,
        )
    }
    fn known_for(&self, fork: &str) -> Vec<String> {
        self.known
            .lock()
            .unwrap()
            .get(fork)
            .cloned()
            .unwrap_or_default()
    }

    // -------------------------------------------------------------- tick

    /// Fail forks whose session ended or moved, and time out runs.
    fn tick(&self, inner: &mut Inner, host: &dyn Host) {
        let t = now();
        let mut sessions: BTreeMap<String, Option<SessionInfo>> = BTreeMap::new();
        let ids: Vec<String> = inner
            .forks
            .values()
            .filter(|f| f.state != ForkState::Closed && f.state != ForkState::Failed)
            .map(|f| f.id.clone())
            .collect();
        for id in ids {
            let (session_id, generation) = {
                let f = &inner.forks[&id];
                (f.session.clone(), f.generation)
            };
            let session = sessions
                .entry(session_id.clone())
                .or_insert_with(|| host.session(&session_id))
                .clone();
            let gone = session
                .as_ref()
                .is_none_or(|s| !s.open || s.generation != generation);
            let f = inner.forks.get_mut(&id).unwrap();
            let mut changed = false;
            if gone {
                f.state = if f.state == ForkState::Closing {
                    ForkState::Closed
                } else {
                    ForkState::Failed
                };
                f.error
                    .get_or_insert_with(|| "The live engine session ended.".into());
                f.closed_at = Some(t);
                for r in f.runs.iter_mut().filter(|r| r.state.active()) {
                    r.state = RunState::Failed;
                    r.ended_at = Some(t);
                    r.error = Some("The live engine session ended.".into());
                }
                f.ops.clear();
                if let Some(sub) = f.submission.clone()
                    && let Some(s) = inner.subs.get_mut(&sub).filter(|s| s.state.open())
                {
                    s.state = SubState::Failed;
                    s.reason = Some("The live engine session ended.".into());
                    s.updated_at = t;
                    let _ = self.save_sub(s);
                }
                self.known.lock().unwrap().remove(&id);
                changed = true;
            } else {
                let mut timed_out = vec![];
                for r in f.runs.iter_mut().filter(|r| r.state.on_node()) {
                    let since = r.dispatched_at.unwrap_or(r.created_at);
                    if since + r.timeout_seconds + RUN_GRACE_SECONDS <= t {
                        r.state = RunState::Failed;
                        r.ended_at = Some(t);
                        r.error = Some(format!(
                            "The run reached its {} minute limit.",
                            r.timeout_seconds / 60
                        ));
                        timed_out.push(r.id.clone());
                    }
                }
                for run in timed_out {
                    f.ops
                        .retain(|o| !(o.kind == OpKind::Run && o.run_id.as_deref() == Some(&run)));
                    let mut op = Op::new(OpKind::Cancel, json!({"cancel": {"run_id": run}}));
                    op.run_id = Some(run);
                    f.ops.push(op);
                    if f.state == ForkState::Running {
                        f.state = ForkState::Idle;
                    }
                    changed = true;
                }
            }
            if changed {
                f.updated_at = t;
                let _ = self.save_fork(f);
            }
        }
    }

    // -------------------------------------------------------------- API

    pub fn handle(&self, host: &dyn Host, req: Request) -> Result<Reply> {
        use Action as A;
        match req.route.action {
            A::Callback => return self.callback(host, &req),
            A::GatewayConnect => return self.gateway_connect(host, &req),
            A::GatewayGet => {
                let xinf = host.xinf().ok().flatten();
                return Ok(Reply::Json(
                    200,
                    self.gateway
                        .status(&req.org, &req.caller.person(), xinf.as_deref()),
                ));
            }
            A::GatewayPut => {
                crate::keys(&req.body, &["key"], &["key"])
                    .map_err(|_| refuse(400, "Send {key: \"xk_live_…\"}."))?;
                self.gateway.put_key(
                    &req.org,
                    &req.caller.person(),
                    req.body["key"].as_str().unwrap_or(""),
                )?;
                host.audit(
                    &req.org,
                    &req.caller.actor(),
                    "agent_fork.gateway",
                    &req.route.project,
                    true,
                    json!({"kind": "key"}),
                );
                let xinf = host.xinf().ok().flatten();
                return Ok(Reply::Json(
                    200,
                    self.gateway
                        .status(&req.org, &req.caller.person(), xinf.as_deref()),
                ));
            }
            A::GatewayDelete => {
                let xinf = host.xinf().ok().flatten();
                let removed =
                    self.gateway
                        .delete(&req.org, &req.caller.person(), xinf.as_deref())?;
                host.audit(
                    &req.org,
                    &req.caller.actor(),
                    "agent_fork.gateway",
                    &req.route.project,
                    removed,
                    json!({"kind": "disconnect"}),
                );
                return Ok(Reply::Json(
                    200,
                    json!({"connected": false, "removed": removed}),
                ));
            }
            _ => {}
        }
        let session_id = req.route.session.clone().unwrap_or_default();
        let session = host
            .session(&session_id)
            .filter(|s| s.org == req.org && s.project == req.route.project)
            .ok_or_else(|| refuse(404, "Unknown engine session."))?;
        // Who may reach this session at all, and as what.
        let room_owner = match &req.caller {
            Caller::Owner => true,
            Caller::Member(m) => {
                ensure!(
                    &session.started_by == m,
                    refuse(404, "Unknown engine session.")
                );
                true
            }
            Caller::Room(_) => {
                ensure!(
                    req.grant_member.as_deref() == Some(session.started_by.as_str()),
                    refuse(404, "Unknown engine session.")
                );
                false
            }
        };
        let access = Access {
            room_owner,
            caller: &req.caller,
            session: &session,
        };
        match req.route.action {
            A::ListForks => self.list_forks(host, &access, &req),
            A::CreateFork => self.create_fork(host, &access, &req),
            A::GetFork => self.get_fork(host, &access, &req),
            A::Diff => self.diff(host, &access, &req),
            A::CreateRun => self.create_run(host, &access, &req),
            A::Events => self.events(host, &access, &req),
            A::CancelRun => self.cancel_run(host, &access, &req),
            A::Submit => self.submit(host, &access, &req),
            A::Rebase => self.rebase(host, &access, &req),
            A::Close => self.close(host, &access, &req),
            A::ListSubmissions => self.list_submissions(host, &access, &req),
            A::Approve => self.approve(host, &access, &req),
            A::Reject => self.reject(host, &access, &req),
            A::Dev => self.dev_view(host, &access),
            A::Revert => self.revert(host, &access, &req),
            A::Publish => self.publish(host, &access, &req),
            _ => Err(refuse(404, "Unknown agent fork operation.")),
        }
    }

    fn gateway_connect(&self, host: &dyn Host, req: &Request) -> Result<Reply> {
        crate::keys(&req.body, &["return_to"], &["return_to"])
            .map_err(|_| refuse(400, "Send {return_to}."))?;
        let xinf = host
            .xinf()?
            .ok_or_else(|| refuse(409, gateway::NOT_CONFIGURED))?;
        let mut origins = req.return_origins.clone();
        if matches!(req.caller, Caller::Owner | Caller::Member(_))
            && req.grant_member.is_none()
            && let Some(origin) = host.dashboard_origin()
        {
            origins.push(origin);
        }
        let return_to = gateway::check_return_to(
            req.body["return_to"].as_str().unwrap_or(""),
            &origins,
        )
        .ok_or_else(|| {
            refuse(
                400,
                "return_to must be an https URL on one of the app's registered redirect origins.",
            )
        })?;
        let url = self
            .gateway
            .connect(&req.org, &req.caller.person(), &return_to, &*xinf)?;
        Ok(Reply::Json(200, json!({"authorize_url": url})))
    }

    fn callback(&self, host: &dyn Host, req: &Request) -> Result<Reply> {
        ensure!(
            matches!(req.caller, Caller::Owner) && req.grant_member.is_none(),
            refuse(403, "Only the dashboard forwards xinf callbacks.")
        );
        crate::keys(&req.body, &["state", "code", "error"], &["state"])
            .map_err(|_| refuse(400, "Send {state, code} or {state, error}."))?;
        let state = req.body["state"].as_str().unwrap_or("");
        ensure!(
            (1..=128).contains(&state.len()),
            refuse(400, "Invalid state.")
        );
        let code = req.body["code"].as_str();
        ensure!(
            code.is_some() != req.body.get("error").is_some(),
            refuse(400, "Send exactly one of code and error.")
        );
        let xinf = host
            .xinf()?
            .ok_or_else(|| refuse(409, gateway::NOT_CONFIGURED))?;
        let (return_to, who) = self
            .gateway
            .callback(state, code, &*xinf)
            .map_err(|e| refuse(400, e.to_string()))?;
        if let Some((org, person)) = who {
            host.audit(
                &org,
                &person,
                "agent_fork.gateway",
                "xinf",
                true,
                json!({"kind": "oauth"}),
            );
        }
        Ok(Reply::Json(200, json!({"return_to": return_to})))
    }

    /// The caller's run bearer can be had (and balance is not known to be
    /// exhausted). Network calls: never under the store lock.
    fn check_gateway(&self, host: &dyn Host, org: &str, person: &str) -> Result<()> {
        let record = self
            .gateway
            .record(org, person)
            .ok_or_else(|| coded(409, "gateway_required", GATEWAY_REQUIRED))?;
        ensure!(
            record.status == gateway::Status::Connected,
            coded(409, "gateway_required", gateway::RECONNECT)
        );
        if record.kind == gateway::Kind::Oauth {
            let xinf = host
                .xinf()?
                .ok_or_else(|| refuse(409, gateway::NOT_CONFIGURED))?;
            match self.gateway.access(org, person, Some(&*xinf)) {
                Err(e) if e.to_string() == gateway::RECONNECT => {
                    return Err(coded(409, "gateway_required", gateway::RECONNECT));
                }
                Err(_) | Ok(_) => {}
            }
            if let Some(account) = self.gateway.account(org, person, &*xinf) {
                let balance = gateway::usd(&account["balance"]["available_usd"]);
                let remaining = gateway::usd(&account["connection"]["remaining_budget_usd"]);
                if balance.is_some_and(|b| b < MIN_BALANCE_USD)
                    || remaining.is_some_and(|r| r < MIN_BALANCE_USD)
                {
                    let topup = format!("{}/dashboard/billing", xinf.origin());
                    return Err(anyhow::Error::new(Refusal {
                        status: 409,
                        message: "Your Xava Inference balance or this app's monthly cap is used up. Add balance to run agents.".into(),
                        code: Some("insufficient_balance"),
                        extra: json!({"topup_url": topup}),
                    }));
                }
            }
        }
        Ok(())
    }

    fn list_forks(&self, host: &dyn Host, a: &Access, req: &Request) -> Result<Reply> {
        let all = req.query.get("all").map(String::as_str) == Some("1");
        ensure!(
            !all || a.room_owner,
            refuse(403, "Only the room owner lists every fork in full.")
        );
        let mut inner = self.inner.lock().unwrap();
        self.tick(&mut inner, host);
        let me = a.caller.key();
        let mut rows: Vec<&Fork> = inner
            .forks
            .values()
            .filter(|f| f.session == a.session.id)
            .collect();
        rows.sort_by_key(|f| std::cmp::Reverse(f.created_at));
        let forks: Vec<Value> = rows
            .into_iter()
            .filter_map(|f| {
                if f.owner.key == me || all {
                    Some(f.summary())
                } else if f.state.open() {
                    Some(f.claim())
                } else {
                    None
                }
            })
            .take(200)
            .collect();
        Ok(Reply::Json(200, json!({"forks": forks})))
    }

    /// The fork the caller may read or drive (404 otherwise).
    fn fork_for<'a>(
        inner: &'a mut Inner,
        a: &Access,
        id: &str,
        owner_only: bool,
    ) -> Result<&'a mut Fork> {
        let me = a.caller.key();
        inner
            .forks
            .get_mut(id)
            .filter(|f| {
                f.session == a.session.id && (f.owner.key == me || (!owner_only && a.room_owner))
            })
            .ok_or_else(|| refuse(404, "Unknown fork."))
    }

    fn create_fork(&self, host: &dyn Host, a: &Access, req: &Request) -> Result<Reply> {
        crate::keys(
            &req.body,
            &["request_id", "prompt", "provider", "model", "budget_usd", "title", "paths"],
            &["request_id", "prompt", "provider"],
        )
        .map_err(|_| {
            refuse(
                400,
                "Send request_id, prompt, provider (claude or codex), and optionally model, budget_usd, title and paths.",
            )
        })?;
        ensure!(
            a.caller.builder(),
            refuse(403, "Running agents needs builder rights in this room.")
        );
        let request_id = req.body["request_id"].as_str().unwrap_or("");
        ensure!(
            enginebox_node_protocol::id(request_id),
            refuse(400, "request_id must be 1-80 letters, digits, - or _.")
        );
        let spec = RunSpec::parse(&req.body, None)?;
        let title = match req.body.get("title") {
            None | Some(Value::Null) => spec
                .prompt
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(80)
                .collect::<String>(),
            Some(v) => v
                .as_str()
                .filter(|t| {
                    !t.trim().is_empty()
                        && t.chars().count() <= 120
                        && !t.chars().any(char::is_control)
                })
                .ok_or_else(|| refuse(400, "title must be 1-120 printable characters."))?
                .trim()
                .to_owned(),
        };
        let paths: Vec<String> = match req.body.get("paths") {
            None | Some(Value::Null) => vec![],
            Some(Value::Array(list)) if list.len() <= 32 => list
                .iter()
                .map(|p| {
                    p.as_str()
                        .filter(|p| {
                            !p.is_empty()
                                && p.len() <= 200
                                && !p.starts_with('/')
                                && !p.contains("..")
                                && !p.chars().any(|c| c.is_control() || c == '\\')
                        })
                        .map(str::to_owned)
                        .ok_or_else(|| refuse(400, "paths must be relative globs."))
                })
                .collect::<Result<_>>()?,
            _ => return Err(refuse(400, "paths must list at most 32 globs.")),
        };
        let owner = a.caller.owner();
        let digest = state::digest(&json!({
            "prompt": spec.prompt, "provider": spec.provider, "model": spec.model,
            "budget_usd": spec.budget_usd, "title": title, "paths": paths,
        }));
        // A retry of the same request answers the fork it made.
        {
            let inner = self.inner.lock().unwrap();
            if let Some(f) = inner.forks.values().find(|f| {
                f.session == a.session.id && f.owner.key == owner.key && f.request_id == request_id
            }) {
                ensure!(
                    f.request_digest == digest,
                    refuse(409, "request_id was already used for a different fork.")
                );
                return Ok(Reply::Json(
                    202,
                    json!({"fork": f.summary(), "run": f.runs.first().map(Run::view)}),
                ));
            }
        }
        let audit_refusal = |message: &str| {
            host.audit(
                &req.org,
                &a.caller.actor(),
                "agent_fork.create",
                &a.session.id,
                false,
                json!({"reason": message}),
            );
        };
        if !(a.session.ready && a.session.editor) {
            audit_refusal(NOT_READY);
            return Err(refuse(409, NOT_READY));
        }
        let caps = a
            .session
            .node
            .as_deref()
            .and_then(|n| host.caps(n))
            .ok_or_else(|| coded(409, "forks_unsupported", NO_FORKS))?;
        if caps.isolation == "none" && !a.room_owner {
            let message = "This engine machine has no isolated agent accounts yet, so only the room owner can run agents here.";
            audit_refusal(message);
            return Err(coded(409, "isolation_required", message));
        }
        if let Err(e) = self.check_gateway(host, &req.org, &owner.person) {
            audit_refusal(&e.to_string());
            return Err(e);
        }
        let t = now();
        let mut inner = self.inner.lock().unwrap();
        self.tick(&mut inner, host);
        let open = inner
            .forks
            .values()
            .filter(|f| f.session == a.session.id && f.owner.key == owner.key && f.state.open())
            .count();
        if open >= MAX_FORKS_PER_PRINCIPAL {
            drop(inner);
            let message = format!(
                "You already have {MAX_FORKS_PER_PRINCIPAL} open forks here; close one first."
            );
            audit_refusal(&message);
            return Err(coded(409, "fork_limit", message));
        }
        if inner.forks.len() >= MAX_FORK_RECORDS {
            let mut done: Vec<(u64, String)> = inner
                .forks
                .values()
                .filter(|f| !f.state.open() && f.ops.is_empty())
                .map(|f| (f.updated_at, f.id.clone()))
                .collect();
            done.sort();
            for (_, id) in done.into_iter().take(100) {
                inner.forks.remove(&id);
                let _ = std::fs::remove_file(self.root.join("forks").join(format!("{id}.json")));
            }
            ensure!(
                inner.forks.len() < MAX_FORK_RECORDS,
                refuse(429, "Too many forks.")
            );
        }
        let base = inner.devs.get(&a.session.id).and_then(|d| d.head.clone());
        let id = new_id();
        let run = spec.run(request_id, &digest, t);
        let mut fork = Fork {
            id: id.clone(),
            org_id: req.org.clone(),
            project: req.route.project.clone(),
            session: a.session.id.clone(),
            generation: a.session.generation,
            owner,
            title,
            paths,
            state: ForkState::Creating,
            base,
            head: None,
            changed: vec![],
            findings: vec![],
            runs: vec![],
            ops: vec![],
            done_ops: vec![],
            submission: None,
            request_id: request_id.to_owned(),
            request_digest: digest,
            native_session_id: None,
            rebase_conflict: vec![],
            error: None,
            keep_snapshot: false,
            diff_head: None,
            diff_findings: vec![],
            created_at: t,
            updated_at: t,
            closed_at: None,
        };
        fork.ops.push(spec.op(&run, None));
        fork.runs.push(run);
        self.save_fork(&fork)?;
        host.audit(&req.org, &a.caller.actor(), "agent_fork.create", &id, true, json!({"session": a.session.id, "provider": spec.provider, "budget_usd": spec.budget_usd}));
        let reply = json!({"fork": {"id": id, "state": "creating", "base": fork.base}, "run": {"id": fork.runs[0].id, "state": "queued"}});
        inner.forks.insert(id, fork);
        Ok(Reply::Json(202, reply))
    }

    fn get_fork(&self, host: &dyn Host, a: &Access, req: &Request) -> Result<Reply> {
        let mut inner = self.inner.lock().unwrap();
        self.tick(&mut inner, host);
        let id = req.route.fork.as_deref().unwrap_or("");
        let f = Self::fork_for(&mut inner, a, id, false)?.clone();
        let sub = f.submission.as_ref().and_then(|s| inner.subs.get(s));
        Ok(Reply::Json(200, f.view(sub)))
    }

    fn diff(&self, host: &dyn Host, a: &Access, req: &Request) -> Result<Reply> {
        let id = req.route.fork.as_deref().unwrap_or("");
        {
            let mut inner = self.inner.lock().unwrap();
            self.tick(&mut inner, host);
            let f = Self::fork_for(&mut inner, a, id, false)?;
            ensure!(
                f.diff_head.is_some(),
                refuse(404, "This fork has no diff yet.")
            );
        }
        let limit = match req.query.get("limit") {
            None => MAX_DIFF_BYTES,
            Some(l) => l
                .parse::<usize>()
                .ok()
                .filter(|l| (1..=MAX_DIFF_BYTES).contains(l))
                .ok_or_else(|| refuse(400, "limit must be 1-2097152."))?,
        };
        let bytes = read_private(
            &self.root.join("fork-diffs").join(format!("{id}.diff")),
            MAX_DIFF_BYTES as u64,
        )
        .ok_or_else(|| refuse(404, "This fork has no diff yet."))?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let text = match req.query.get("path") {
            None => text,
            Some(path) => only_path(&text, path),
        };
        let mut cut = text.len().min(limit);
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        Ok(Reply::Diff(text[..cut].to_owned()))
    }

    fn create_run(&self, host: &dyn Host, a: &Access, req: &Request) -> Result<Reply> {
        crate::keys(
            &req.body,
            &["request_id", "prompt", "resume", "budget_usd"],
            &["request_id", "prompt"],
        )
        .map_err(|_| {
            refuse(
                400,
                "Send request_id, prompt, and optionally resume and budget_usd.",
            )
        })?;
        ensure!(
            a.caller.builder(),
            refuse(403, "Running agents needs builder rights in this room.")
        );
        let request_id = req.body["request_id"].as_str().unwrap_or("");
        ensure!(
            enginebox_node_protocol::id(request_id),
            refuse(400, "request_id must be 1-80 letters, digits, - or _.")
        );
        let id = req.route.fork.clone().unwrap_or_default();
        let (person, previous) = {
            let mut inner = self.inner.lock().unwrap();
            self.tick(&mut inner, host);
            let f = Self::fork_for(&mut inner, a, &id, true)?;
            if let Some(r) = f.runs.iter().find(|r| r.request_id == request_id) {
                return Ok(Reply::Json(202, json!({"run": r.view()})));
            }
            (
                f.owner.person.clone(),
                f.runs.last().map(|r| (r.provider.clone(), r.model.clone())),
            )
        };
        let spec = RunSpec::parse(&req.body, previous)?;
        self.check_gateway(host, &req.org, &person)?;
        let mut inner = self.inner.lock().unwrap();
        let subs_open = |s: &Option<String>, inner: &Inner| {
            s.as_ref()
                .and_then(|s| inner.subs.get(s))
                .is_some_and(|s| s.state.open())
        };
        let open_sub = {
            let f = Self::fork_for(&mut inner, a, &id, true)?;
            f.submission.clone()
        };
        let blocked = subs_open(&open_sub, &inner);
        let f = Self::fork_for(&mut inner, a, &id, true)?;
        ensure!(
            matches!(f.state, ForkState::Idle | ForkState::Creating) && !blocked,
            refuse(
                409,
                "This fork is not idle (a run, submission or close is in progress)."
            )
        );
        ensure!(
            f.active_run().is_none(),
            refuse(409, "This fork already has a run in progress.")
        );
        ensure!(
            !spec.resume || f.native_session_id.is_some(),
            refuse(409, "There is no earlier agent session to resume.")
        );
        if f.runs.len() >= MAX_RUNS_PER_FORK {
            f.runs.remove(0);
        }
        let digest = state::digest(
            &json!({"prompt": spec.prompt, "resume": spec.resume, "budget_usd": spec.budget_usd}),
        );
        let run = spec.run(request_id, &digest, now());
        let resume_id = spec.resume.then(|| f.native_session_id.clone()).flatten();
        f.ops.push(spec.op(&run, resume_id));
        let view = run.view();
        f.runs.push(run);
        f.updated_at = now();
        self.save_fork(f)?;
        host.audit(
            &req.org,
            &a.caller.actor(),
            "agent_fork.run",
            &id,
            true,
            json!({"run": view["id"], "provider": spec.provider, "budget_usd": spec.budget_usd}),
        );
        Ok(Reply::Json(202, json!({"run": view})))
    }

    fn events(&self, host: &dyn Host, a: &Access, req: &Request) -> Result<Reply> {
        let id = req.route.fork.clone().unwrap_or_default();
        let run = req.route.run.clone().unwrap_or_default();
        {
            let mut inner = self.inner.lock().unwrap();
            self.tick(&mut inner, host);
            let f = Self::fork_for(&mut inner, a, &id, false)?;
            ensure!(
                f.runs.iter().any(|r| r.id == run),
                refuse(404, "Unknown run.")
            );
        }
        let after: u64 = match req.query.get("after") {
            None => 0,
            Some(v) => v
                .parse()
                .ok()
                .filter(|_| v.len() <= 10)
                .ok_or_else(|| refuse(400, "after must be a number."))?,
        };
        let _guard = self.events_lock.lock().unwrap();
        let stored = self.read_events(&id, &run);
        let events: Vec<Value> = stored
            .iter()
            .filter(|e| e["seq"].as_u64().unwrap_or(0) > after)
            .take(500)
            .cloned()
            .collect();
        let next = events
            .last()
            .and_then(|e| e["seq"].as_u64())
            .unwrap_or(after);
        Ok(Reply::Json(200, json!({"events": events, "next": next})))
    }
    fn events_path(&self, fork: &str, run: &str) -> PathBuf {
        self.root
            .join("fork-events")
            .join(format!("{fork}-{run}.json"))
    }
    fn read_events(&self, fork: &str, run: &str) -> Vec<Value> {
        state::read_json(&self.events_path(fork, run))
            .ok()
            .and_then(|v| v["events"].as_array().cloned())
            .unwrap_or_default()
    }

    fn cancel_run(&self, host: &dyn Host, a: &Access, req: &Request) -> Result<Reply> {
        crate::keys(&req.body, &[], &[]).map_err(|_| refuse(400, "Send {}."))?;
        let id = req.route.fork.clone().unwrap_or_default();
        let run_id = req.route.run.clone().unwrap_or_default();
        let mut inner = self.inner.lock().unwrap();
        self.tick(&mut inner, host);
        let f = Self::fork_for(&mut inner, a, &id, false)?;
        let state = f
            .runs
            .iter()
            .find(|r| r.id == run_id)
            .map(|r| r.state)
            .ok_or_else(|| refuse(404, "Unknown run."))?;
        match state {
            RunState::Queued => {
                f.ops.retain(|o| o.run_id.as_deref() != Some(&run_id));
                let r = f.runs.iter_mut().find(|r| r.id == run_id).unwrap();
                r.state = RunState::Cancelled;
                r.ended_at = Some(now());
                if f.state == ForkState::Creating && f.head.is_none() {
                    // The node still creates the tree; the fork stays.
                } else if f.state == ForkState::Running {
                    f.state = ForkState::Idle;
                }
            }
            s if s.on_node()
                && !f
                    .ops
                    .iter()
                    .any(|o| o.kind == OpKind::Cancel && o.run_id.as_deref() == Some(&run_id)) =>
            {
                let mut op = Op::new(OpKind::Cancel, json!({"cancel": {"run_id": run_id}}));
                op.run_id = Some(run_id.clone());
                f.ops.push(op);
            }
            _ => {}
        }
        f.updated_at = now();
        self.save_fork(f)?;
        let view = f.runs.iter().find(|r| r.id == run_id).unwrap().view();
        Ok(Reply::Json(200, json!({"run": view})))
    }

    fn submit(&self, host: &dyn Host, a: &Access, req: &Request) -> Result<Reply> {
        crate::keys(
            &req.body,
            &["request_id", "message", "expected_head", "auto_merge"],
            &["request_id", "message", "expected_head"],
        )
        .map_err(|_| {
            refuse(
                400,
                "Send request_id, message, expected_head, and optionally auto_merge.",
            )
        })?;
        ensure!(
            a.caller.builder(),
            refuse(403, "Submitting needs builder rights in this room.")
        );
        let request_id = req.body["request_id"].as_str().unwrap_or("");
        ensure!(
            enginebox_node_protocol::id(request_id),
            refuse(400, "request_id must be 1-80 letters, digits, - or _.")
        );
        let message = req.body["message"]
            .as_str()
            .map(str::trim)
            .filter(|m| !m.is_empty() && m.chars().count() <= 500 && !m.contains('\0'))
            .ok_or_else(|| refuse(400, "message must be 1-500 characters."))?
            .to_owned();
        let expected = req.body["expected_head"].as_str().unwrap_or("");
        ensure!(
            valid_commit(expected),
            refuse(400, "expected_head must be the fork head you reviewed.")
        );
        let auto_merge: Vec<Class> = match req.body.get("auto_merge") {
            None | Some(Value::Null) => vec![Class::Config, Class::Scene, Class::Asset],
            Some(Value::Array(list)) => list
                .iter()
                .map(|c| {
                    c.as_str()
                        .and_then(Class::parse)
                        .filter(|c| c.automatic())
                        .ok_or_else(|| {
                            refuse(400, "auto_merge may list only scene, asset and config.")
                        })
                })
                .collect::<Result<_>>()?,
            _ => return Err(refuse(400, "auto_merge must be a list.")),
        };
        let id = req.route.fork.clone().unwrap_or_default();
        let digest = state::digest(
            &json!({"message": message, "expected_head": expected, "auto_merge": auto_merge}),
        );
        let mut inner = self.inner.lock().unwrap();
        self.tick(&mut inner, host);
        let me = a.caller.key();
        if let Some(s) = inner
            .subs
            .values()
            .find(|s| s.fork == id && s.owner.key == me && s.request_id == request_id)
        {
            ensure!(
                s.request_digest == digest,
                refuse(
                    409,
                    "request_id was already used for a different submission."
                )
            );
            return Ok(Reply::Json(202, json!({"submission": s.view()})));
        }
        let open_sub = Self::fork_for(&mut inner, a, &id, true)?.submission.clone();
        ensure!(
            !open_sub
                .and_then(|s| inner.subs.get(&s))
                .is_some_and(|s| s.state.open()),
            refuse(409, "This fork already has a submission in progress.")
        );
        let f = Self::fork_for(&mut inner, a, &id, true)?;
        ensure!(
            f.state == ForkState::Idle && f.active_run().is_none(),
            refuse(409, "Submit when the fork is idle (no run in progress).")
        );
        ensure!(
            f.head.as_deref() == Some(expected),
            refuse(
                409,
                "The fork head moved; review the new head and submit again."
            )
        );
        let t = now();
        let sub = Submission {
            id: new_id(),
            org_id: f.org_id.clone(),
            project: f.project.clone(),
            session: f.session.clone(),
            fork: f.id.clone(),
            owner: f.owner.clone(),
            message,
            request_id: request_id.to_owned(),
            request_digest: digest,
            fork_head: expected.to_owned(),
            state: SubState::Checking,
            classes: json!({}),
            findings: vec![],
            auto_merge,
            needs_approval: false,
            approved_by: None,
            approve_request: None,
            reason: None,
            conflict_paths: vec![],
            held_reason: None,
            dev_head_before: None,
            dev_head_after: None,
            merge_attempts: 0,
            reverted: None,
            created_at: t,
            updated_at: t,
            applied_at: None,
        };
        let mut op = Op::new(
            OpKind::Snapshot,
            json!({"snapshot": {"expected_head": expected, "submission": sub.id}}),
        );
        op.submission = Some(sub.id.clone());
        f.ops.push(op);
        f.state = ForkState::Submitted;
        f.submission = Some(sub.id.clone());
        f.updated_at = t;
        self.save_fork(f)?;
        self.save_sub(&sub)?;
        host.audit(
            &req.org,
            &a.caller.actor(),
            "agent_fork.submit",
            &sub.id,
            true,
            json!({"fork": id, "head": expected}),
        );
        let view = sub.view();
        inner.subs.insert(sub.id.clone(), sub);
        Ok(Reply::Json(202, json!({"submission": view})))
    }

    fn rebase(&self, host: &dyn Host, a: &Access, req: &Request) -> Result<Reply> {
        crate::keys(&req.body, &["request_id"], &["request_id"])
            .map_err(|_| refuse(400, "Send {request_id}."))?;
        ensure!(
            a.caller.builder(),
            refuse(403, "Rebasing needs builder rights in this room.")
        );
        let id = req.route.fork.clone().unwrap_or_default();
        let mut inner = self.inner.lock().unwrap();
        self.tick(&mut inner, host);
        Self::fork_for(&mut inner, a, &id, true)?;
        let onto = inner
            .devs
            .get(&a.session.id)
            .and_then(|d| d.head.clone())
            .ok_or_else(|| refuse(409, "The dev head is not known yet; try again shortly."))?;
        let f = Self::fork_for(&mut inner, a, &id, true)?;
        if f.ops.iter().any(|o| o.kind == OpKind::Rebase) {
            return Ok(Reply::Json(202, json!({"fork": f.summary()})));
        }
        ensure!(
            f.state == ForkState::Idle && f.active_run().is_none(),
            refuse(409, "Rebase when the fork is idle.")
        );
        f.ops
            .push(Op::new(OpKind::Rebase, json!({"rebase": {"onto": onto}})));
        f.rebase_conflict.clear();
        f.updated_at = now();
        self.save_fork(f)?;
        Ok(Reply::Json(202, json!({"fork": f.summary()})))
    }

    fn close(&self, host: &dyn Host, a: &Access, req: &Request) -> Result<Reply> {
        crate::keys(&req.body, &["keep_snapshot"], &[])
            .map_err(|_| refuse(400, "Send {} or {keep_snapshot}."))?;
        let keep = req.body["keep_snapshot"].as_bool().unwrap_or(true);
        let id = req.route.fork.clone().unwrap_or_default();
        let mut inner = self.inner.lock().unwrap();
        self.tick(&mut inner, host);
        let sub = Self::fork_for(&mut inner, a, &id, false)?
            .submission
            .clone();
        let merging = sub
            .as_ref()
            .and_then(|s| inner.subs.get(s))
            .is_some_and(|s| {
                matches!(
                    s.state,
                    SubState::Merging | SubState::Applying | SubState::Held
                )
            });
        ensure!(
            !merging,
            refuse(
                409,
                "This fork's submission is being merged; close it once it is applied."
            )
        );
        if let Some(s) = sub
            .as_ref()
            .and_then(|s| inner.subs.get_mut(s))
            .filter(|s| s.state.open())
        {
            s.state = SubState::Rejected;
            s.reason = Some("The fork was closed.".into());
            s.updated_at = now();
            self.save_sub(s)?;
        }
        let f = Self::fork_for(&mut inner, a, &id, false)?;
        if f.state.open() {
            let t = now();
            for r in f.runs.iter_mut().filter(|r| r.state.active()) {
                r.state = RunState::Cancelled;
                r.ended_at = Some(t);
            }
            f.ops.retain(|o| matches!(o.kind, OpKind::Revert));
            f.state = ForkState::Closing;
            f.keep_snapshot = keep;
            f.updated_at = t;
            self.save_fork(f)?;
        }
        Ok(Reply::Json(200, json!({"fork": f.summary()})))
    }

    fn list_submissions(&self, host: &dyn Host, a: &Access, req: &Request) -> Result<Reply> {
        let wanted = req.query.get("state");
        let mut inner = self.inner.lock().unwrap();
        self.tick(&mut inner, host);
        let me = a.caller.key();
        let mut rows: Vec<&Submission> = inner
            .subs
            .values()
            .filter(|s| s.session == a.session.id && (a.room_owner || s.owner.key == me))
            .filter(|s| wanted.is_none_or(|w| s.state.name() == w))
            .collect();
        rows.sort_by_key(|s| std::cmp::Reverse(s.created_at));
        Ok(Reply::Json(
            200,
            json!({"submissions": rows.into_iter().take(200).map(Submission::view).collect::<Vec<_>>()}),
        ))
    }

    fn approve(&self, host: &dyn Host, a: &Access, req: &Request) -> Result<Reply> {
        crate::keys(
            &req.body,
            &["request_id", "expected_fork_head"],
            &["request_id", "expected_fork_head"],
        )
        .map_err(|_| refuse(400, "Send request_id and expected_fork_head."))?;
        ensure!(
            a.room_owner,
            refuse(403, "Only the room owner approves submissions.")
        );
        let request_id = req.body["request_id"].as_str().unwrap_or("");
        ensure!(
            enginebox_node_protocol::id(request_id),
            refuse(400, "request_id must be 1-80 letters, digits, - or _.")
        );
        let expected = req.body["expected_fork_head"].as_str().unwrap_or("");
        let id = req.route.submission.clone().unwrap_or_default();
        let mut inner = self.inner.lock().unwrap();
        self.tick(&mut inner, host);
        let dev_head = inner.devs.get(&a.session.id).and_then(|d| d.head.clone());
        let s = inner
            .subs
            .get_mut(&id)
            .filter(|s| s.session == a.session.id)
            .ok_or_else(|| refuse(404, "Unknown submission."))?;
        if s.approve_request.as_deref() == Some(request_id) {
            return Ok(Reply::Json(202, json!({"submission": s.view()})));
        }
        ensure!(
            s.state == SubState::AwaitingApproval,
            refuse(
                409,
                format!(
                    "The submission is {}, not awaiting approval.",
                    s.state.name()
                )
            )
        );
        ensure!(
            s.fork_head == expected,
            refuse(409, "The fork head is not the one you reviewed.")
        );
        let actor = a.caller.actor();
        s.state = SubState::Merging;
        s.approved_by = Some(actor.clone());
        s.approve_request = Some(request_id.to_owned());
        s.updated_at = now();
        let s = s.clone();
        self.save_sub(&s)?;
        let f = inner
            .forks
            .get_mut(&s.fork)
            .ok_or_else(|| refuse(404, "Unknown fork."))?;
        ensure!(
            f.head.as_deref() == Some(expected),
            refuse(409, "The fork head is not the one you reviewed.")
        );
        let expected_dev = dev_head.or_else(|| f.base.clone());
        f.ops
            .push(merge_op(&s, expected_dev.as_deref(), Some(&actor)));
        f.updated_at = now();
        self.save_fork(f)?;
        host.audit(
            &req.org,
            &actor,
            "agent_fork.approve",
            &id,
            true,
            json!({"fork": s.fork, "head": expected}),
        );
        Ok(Reply::Json(202, json!({"submission": s.view()})))
    }

    fn reject(&self, host: &dyn Host, a: &Access, req: &Request) -> Result<Reply> {
        crate::keys(&req.body, &["reason"], &[]).map_err(|_| refuse(400, "Send {reason}."))?;
        ensure!(
            a.room_owner,
            refuse(403, "Only the room owner rejects submissions.")
        );
        let reason = match req.body.get("reason") {
            None | Some(Value::Null) => "Rejected by the room owner.".to_owned(),
            Some(v) => v
                .as_str()
                .filter(|r| r.chars().count() <= 500)
                .ok_or_else(|| refuse(400, "reason must be at most 500 characters."))?
                .to_owned(),
        };
        let id = req.route.submission.clone().unwrap_or_default();
        let mut inner = self.inner.lock().unwrap();
        self.tick(&mut inner, host);
        let s = inner
            .subs
            .get_mut(&id)
            .filter(|s| s.session == a.session.id)
            .ok_or_else(|| refuse(404, "Unknown submission."))?;
        ensure!(
            matches!(s.state, SubState::AwaitingApproval | SubState::Checking),
            refuse(
                409,
                format!(
                    "The submission is {}; it can no longer be rejected.",
                    s.state.name()
                )
            )
        );
        s.state = SubState::Rejected;
        s.reason = Some(reason);
        s.updated_at = now();
        let s = s.clone();
        self.save_sub(&s)?;
        if let Some(f) = inner.forks.get_mut(&s.fork) {
            f.ops.retain(|o| o.submission.as_deref() != Some(&id));
            if f.state == ForkState::Submitted {
                f.state = ForkState::Idle;
            }
            f.updated_at = now();
            self.save_fork(f)?;
        }
        host.audit(
            &req.org,
            &a.caller.actor(),
            "agent_fork.reject",
            &id,
            true,
            json!({"fork": s.fork}),
        );
        Ok(Reply::Json(200, json!({"submission": s.view()})))
    }

    fn dev_view(&self, host: &dyn Host, a: &Access) -> Result<Reply> {
        let mut inner = self.inner.lock().unwrap();
        self.tick(&mut inner, host);
        let pending: Vec<&String> = inner
            .subs
            .values()
            .filter(|s| s.session == a.session.id && s.state.open())
            .map(|s| &s.id)
            .collect();
        let dev = inner.devs.get(&a.session.id);
        let last_revert = inner
            .subs
            .values()
            .filter(|s| s.session == a.session.id)
            .filter_map(|s| s.reverted.as_ref().map(|r| (r.at, s)))
            .max_by_key(|(at, _)| *at)
            .map(|(_, s)| json!({"submission": s.id, "reverted": s.reverted}));
        Ok(Reply::Json(
            200,
            json!({
                "head": dev.and_then(|d| d.head.clone()),
                "applied": dev.and_then(|d| d.applied.clone()),
                "applied_at": dev.and_then(|d| d.applied_at),
                "pending": pending,
                "held": dev.and_then(|d| d.held.clone()).map(|reason| json!({"reason": reason})),
                "compile": dev.map_or(json!({"state": "unknown", "errors": []}), |d| d.compile.clone()),
                "live_code_pending": dev.map_or(vec![], |d| d.live_code_pending.clone()),
                "last_revert": last_revert,
                "notices": dev.map_or(vec![], |d| d.notices.clone()),
                "published": dev.and_then(|d| d.published.clone()),
            }),
        ))
    }

    fn revert(&self, host: &dyn Host, a: &Access, req: &Request) -> Result<Reply> {
        crate::keys(
            &req.body,
            &["request_id", "submission"],
            &["request_id", "submission"],
        )
        .map_err(|_| refuse(400, "Send request_id and submission."))?;
        ensure!(
            a.room_owner,
            refuse(403, "Only the room owner reverts merges.")
        );
        let id = req.body["submission"].as_str().unwrap_or("").to_owned();
        let mut inner = self.inner.lock().unwrap();
        self.tick(&mut inner, host);
        let actor = a.caller.actor();
        let dev_head = inner.devs.get(&a.session.id).and_then(|d| d.head.clone());
        let s = inner
            .subs
            .get(&id)
            .filter(|s| s.session == a.session.id)
            .ok_or_else(|| refuse(404, "Unknown submission."))?
            .clone();
        if s.reverted.as_ref().is_some_and(|r| r.state != "failed") {
            return Ok(Reply::Json(202, json!({"submission": s.view()})));
        }
        ensure!(
            matches!(s.state, SubState::Applied | SubState::Held) && s.dev_head_after.is_some(),
            refuse(409, "Only an applied merge can be reverted.")
        );
        self.queue_revert(
            &mut inner,
            &s,
            dev_head,
            false,
            &actor,
            "reverted by the room owner",
            vec![],
        )?;
        host.audit(
            &req.org,
            &actor,
            "agent_fork.revert",
            &id,
            true,
            json!({"auto": false}),
        );
        let view = inner.subs[&id].view();
        Ok(Reply::Json(202, json!({"submission": view})))
    }

    #[allow(clippy::too_many_arguments)]
    fn queue_revert(
        &self,
        inner: &mut Inner,
        s: &Submission,
        dev_head: Option<String>,
        auto: bool,
        by: &str,
        reason: &str,
        errors: Vec<Value>,
    ) -> Result<()> {
        let f = inner
            .forks
            .get_mut(&s.fork)
            .ok_or_else(|| refuse(404, "The submission's fork record is gone."))?;
        let mut op = Op::new(
            OpKind::Revert,
            json!({"revert": {
                "submission": s.id,
                "commit": s.dev_head_after,
                "parent": s.dev_head_before,
                "expected_dev_head": dev_head,
                "message": format!("Revert \"{}\" ({reason})", s.message),
                "auto": auto,
            }}),
        );
        op.submission = Some(s.id.clone());
        f.ops.push(op);
        f.updated_at = now();
        self.save_fork(f)?;
        let s = inner.subs.get_mut(&s.id).unwrap();
        s.reverted = Some(Reverted {
            state: "reverting".into(),
            auto,
            by: by.to_owned(),
            reason: reason.to_owned(),
            errors,
            at: now(),
            dev_head: None,
        });
        s.updated_at = now();
        self.save_sub(s)
    }

    fn publish(&self, host: &dyn Host, a: &Access, req: &Request) -> Result<Reply> {
        crate::keys(
            &req.body,
            &["request_id", "branch"],
            &["request_id", "branch"],
        )
        .map_err(|_| refuse(400, "Send request_id and branch (jam/<name>)."))?;
        ensure!(
            a.room_owner,
            refuse(403, "Only the room owner publishes the dev tree.")
        );
        let branch = req.body["branch"].as_str().unwrap_or("");
        ensure!(
            branch.strip_prefix("jam/").is_some_and(|n| !n.is_empty()
                && n.len() <= 80
                && crate::previews::valid_branch(branch)),
            refuse(400, "branch must be jam/<name>.")
        );
        let request_id = req.body["request_id"].as_str().unwrap_or("").to_owned();
        ensure!(
            enginebox_node_protocol::id(&request_id),
            refuse(400, "request_id must be 1-80 letters, digits, - or _.")
        );
        // Checked now (a clear refusal); minted again at poll time.
        host.repository_write(a.session)?;
        let mut inner = self.inner.lock().unwrap();
        self.tick(&mut inner, host);
        let dev = inner
            .devs
            .entry(a.session.id.clone())
            .or_insert_with(|| Dev::new(a.session));
        if let Some(op) = dev
            .ops
            .iter()
            .find(|o| o.body["publish"]["request_id"] == request_id.as_str())
        {
            return Ok(Reply::Json(
                202,
                json!({"op_id": op.op_id, "branch": branch}),
            ));
        }
        ensure!(
            dev.head.is_some(),
            refuse(409, "The dev head is not known yet; try again shortly.")
        );
        let op = Op::new(
            OpKind::Publish,
            json!({"publish": {"branch": branch, "head": dev.head, "request_id": request_id}}),
        );
        let op_id = op.op_id.clone();
        dev.ops.push(op);
        dev.updated_at = now();
        let dev = dev.clone();
        self.save_dev(&dev)?;
        host.audit(
            &req.org,
            &a.caller.actor(),
            "agent_fork.publish",
            &a.session.id,
            true,
            json!({"branch": branch}),
        );
        Ok(Reply::Json(202, json!({"op_id": op_id, "branch": branch})))
    }

    // -------------------------------------------------------------- node

    /// Add `forks` and `dev` to each session row of an `engine-poll` reply for
    /// `node`, dispatching queued runs into free slots (round robin per
    /// principal) and adding the run bearer to each run in flight.
    pub fn decorate_poll(&self, host: &dyn Host, node: &str, polled: &mut Value) -> Result<()> {
        let Some(rows) = polled["sessions"].as_array_mut() else {
            return Ok(());
        };
        let session_ids: BTreeSet<String> = rows
            .iter()
            .filter_map(|r| r["id"].as_str().map(str::to_owned))
            .collect();
        let caps = host.caps(node);
        // (fork, op_id, org, person, run_id) needing a bearer.
        let mut wanted: Vec<(String, String, String, String)> = vec![];
        let mut publish: Vec<(String, String)> = vec![];
        let mut per_session: BTreeMap<String, (Vec<Value>, Value)> = BTreeMap::new();
        {
            let mut inner = self.inner.lock().unwrap();
            self.tick(&mut inner, host);
            // Dispatch: free slots on this machine, oldest-served principal first.
            let slots = caps
                .as_ref()
                .map_or(0, |c| c.slots.min(MAX_SLOTS_PER_MACHINE));
            let busy = inner
                .forks
                .values()
                .filter(|f| session_ids.contains(&f.session))
                .flat_map(|f| f.runs.iter())
                .filter(|r| r.state.on_node())
                .count() as u64;
            let mut free = slots.saturating_sub(busy);
            let t = now();
            while free > 0 {
                let last = self.last_dispatch.lock().unwrap().clone();
                let next = inner
                    .forks
                    .values()
                    .filter(|f| session_ids.contains(&f.session) && f.state.open())
                    .filter_map(|f| {
                        f.runs
                            .iter()
                            .find(|r| r.state == RunState::Queued)
                            .map(|r| {
                                (
                                    last.get(&f.owner.key).copied().unwrap_or(0),
                                    r.created_at,
                                    f.id.clone(),
                                    r.id.clone(),
                                )
                            })
                    })
                    .min();
                let Some((_, _, fork_id, run_id)) = next else {
                    break;
                };
                let f = inner.forks.get_mut(&fork_id).unwrap();
                let r = f.runs.iter_mut().find(|r| r.id == run_id).unwrap();
                r.state = RunState::Dispatched;
                r.dispatched_at = Some(t);
                if f.state == ForkState::Idle {
                    f.state = ForkState::Running;
                }
                f.updated_at = t;
                self.save_fork(f)?;
                self.last_dispatch
                    .lock()
                    .unwrap()
                    .insert(f.owner.key.clone(), t);
                free -= 1;
            }
            for row in rows.iter() {
                let Some(sid) = row["id"].as_str() else {
                    continue;
                };
                let generation = row["generation"].as_u64().unwrap_or(0);
                let mut forks = vec![];
                for f in inner
                    .forks
                    .values()
                    .filter(|f| f.session == sid && f.generation == generation)
                {
                    let desired = match f.state {
                        ForkState::Closing | ForkState::Closed | ForkState::Failed => "closed",
                        _ => "open",
                    };
                    if f.state == ForkState::Closed && f.ops.is_empty() {
                        continue;
                    }
                    if f.state == ForkState::Failed && f.ops.is_empty() {
                        continue;
                    }
                    let mut ops = vec![];
                    for op in &f.ops {
                        if op.kind == OpKind::Run {
                            let run = op
                                .run_id
                                .as_ref()
                                .and_then(|id| f.runs.iter().find(|r| &r.id == id));
                            if !run.is_some_and(|r| r.state.on_node()) {
                                continue;
                            }
                            wanted.push((
                                f.id.clone(),
                                op.op_id.clone(),
                                f.org_id.clone(),
                                f.owner.person.clone(),
                            ));
                        }
                        ops.push(op.wire());
                    }
                    forks.push(json!({
                        "id": f.id, "generation": f.generation, "desired": desired,
                        "base": f.base, "keep_snapshot": f.keep_snapshot, "ops": ops,
                    }));
                }
                let dev = inner.devs.get(sid);
                let dev_ops: Vec<Value> = dev
                    .map(|d| {
                        d.ops
                            .iter()
                            .map(|o| {
                                publish.push((sid.to_owned(), o.op_id.clone()));
                                o.wire()
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                per_session.insert(
                    sid.to_owned(),
                    (
                        forks,
                        json!({"capture_on_save": true, "head": dev.and_then(|d| d.head.clone()), "ops": dev_ops}),
                    ),
                );
            }
        }
        // Bearers (network when a refresh is due): outside the lock.
        let xinf = host.xinf().ok().flatten();
        let mut bearers: BTreeMap<String, Value> = BTreeMap::new();
        let mut failed: Vec<(String, String, String)> = vec![];
        for (fork, op_id, org, person) in wanted {
            let got = self.gateway.access(&org, &person, xinf.as_deref());
            match got {
                Ok(access) => {
                    {
                        let mut known = self.known.lock().unwrap();
                        let list = known.entry(fork.clone()).or_default();
                        if !list.contains(&access.token) {
                            list.push(access.token.clone());
                            if list.len() > 8 {
                                list.remove(0);
                            }
                        }
                    }
                    let origin = xinf
                        .as_ref()
                        .map_or(gateway::XINF_ORIGIN.to_owned(), |x| x.origin());
                    bearers.insert(
                        op_id,
                        json!({"gateway": {"access_token": access.token, "expires_at": access.expires_at, "base_url": origin, "kind": access.kind}}),
                    );
                }
                Err(e) => failed.push((fork, op_id, e.to_string())),
            }
        }
        let mut repository: BTreeMap<String, Value> = BTreeMap::new();
        for (sid, op_id) in publish {
            let cached = self.publish_tokens.lock().unwrap().get(&op_id).cloned();
            let token = match cached {
                Some(t) => Some(t),
                None => host.session(&sid).and_then(|s| {
                    host.repository_write(&s).ok().map(|w| {
                        let v = json!({"repository": {"remote": w.remote, "token": w.token, "auth": w.auth, "expires_at": w.expires_at}});
                        self.publish_tokens.lock().unwrap().insert(op_id.clone(), v.clone());
                        v
                    })
                }),
            };
            if let Some(t) = token {
                repository.insert(op_id, t);
            }
        }
        if !failed.is_empty() {
            let mut inner = self.inner.lock().unwrap();
            for (fork, op_id, why) in &failed {
                if let Some(f) = inner.forks.get_mut(fork)
                    && let Some(op) = f.finish_op(op_id)
                {
                    if let Some(r) = f
                        .runs
                        .iter_mut()
                        .find(|r| Some(&r.id) == op.run_id.as_ref())
                    {
                        r.state = RunState::Failed;
                        r.ended_at = Some(now());
                        r.error = Some(why.clone());
                    }
                    if f.state == ForkState::Running {
                        f.state = ForkState::Idle;
                    }
                    let _ = self.save_fork(f);
                }
            }
        }
        for row in rows.iter_mut() {
            let Some(sid) = row["id"].as_str().map(str::to_owned) else {
                continue;
            };
            let Some((mut forks, mut dev)) = per_session.remove(&sid) else {
                continue;
            };
            for fork in forks.iter_mut() {
                if let Some(ops) = fork["ops"].as_array_mut() {
                    ops.retain(|op| !failed.iter().any(|(_, id, _)| op["op_id"] == id.as_str()));
                    for op in ops.iter_mut() {
                        if let Some(secrets) = op["op_id"].as_str().and_then(|id| bearers.get(id)) {
                            op["secrets"] = secrets.clone();
                        }
                    }
                }
            }
            if let Some(ops) = dev["ops"].as_array_mut() {
                for op in ops.iter_mut() {
                    if let Some(secrets) = op["op_id"].as_str().and_then(|id| repository.get(id)) {
                        op["secrets"] = secrets.clone();
                    }
                }
            }
            row["forks"] = json!(forks);
            row["dev"] = dev;
        }
        Ok(())
    }

    /// Find a fork a node may report on: its session placed on `node` at
    /// `epoch`, and the fork's generation.
    fn reported<'a>(
        inner: &'a mut Inner,
        host: &dyn Host,
        node: &str,
        epoch: u64,
        fork: &str,
        generation: u64,
    ) -> Result<&'a mut Fork> {
        let f = inner.forks.get_mut(fork).context("Unknown fork")?;
        let s = host.session(&f.session).context("Unknown session")?;
        ensure!(
            s.node.as_deref() == Some(node) && s.node_epoch == epoch && f.generation == generation,
            "Fork is not placed on this node, epoch and generation"
        );
        Ok(f)
    }

    /// `fork-report {session, epoch, fork, generation, op_id?, state, error?, detail?}`.
    pub fn report(&self, host: &dyn Host, node: &str, epoch: u64, body: &Value) -> Result<Value> {
        crate::keys(
            body,
            &[
                "session",
                "epoch",
                "fork",
                "generation",
                "op_id",
                "state",
                "error",
                "detail",
            ],
            &["fork", "generation", "state"],
        )?;
        let fork_id = body["fork"]
            .as_str()
            .filter(|f| valid_id(f))
            .context("fork required")?;
        let generation = body["generation"].as_u64().context("generation required")?;
        let state = body["state"].as_str().context("state required")?;
        let error = clean_error(body.get("error"))?;
        let detail = body.get("detail").cloned().unwrap_or(json!({}));
        ensure!(detail.is_object(), "detail must be an object");
        let mut guard = self.inner.lock().unwrap();
        let inner: &mut Inner = &mut guard;
        let f = Self::reported(inner, host, node, epoch, fork_id, generation)?;
        let session = host.session(&f.session).context("Unknown session")?;
        let Some(op_id) = body["op_id"].as_str() else {
            // Fork-level state.
            let t = now();
            match state {
                "idle" | "ready" => {
                    if f.state == ForkState::Creating {
                        f.state = if f.runs.iter().any(|r| r.state.on_node()) {
                            ForkState::Running
                        } else {
                            ForkState::Idle
                        };
                    }
                    if let Some(base) = commit(&detail, "base")? {
                        f.base = Some(base);
                    }
                    if let Some(head) = commit(&detail, "head")? {
                        f.head = Some(head);
                    } else if f.head.is_none() {
                        f.head = f.base.clone();
                    }
                }
                "failed" => {
                    f.state = ForkState::Failed;
                    f.error =
                        error.or(Some("The engine machine could not create the fork.".into()));
                    f.closed_at = Some(t);
                    for r in f.runs.iter_mut().filter(|r| r.state.active()) {
                        r.state = RunState::Failed;
                        r.ended_at = Some(t);
                        r.error = f.error.clone();
                    }
                    f.ops.retain(|o| o.kind == OpKind::Revert);
                }
                "closed" => {
                    f.state = ForkState::Closed;
                    f.closed_at.get_or_insert(t);
                    f.ops.retain(|o| o.kind == OpKind::Revert);
                    self.known.lock().unwrap().remove(fork_id);
                }
                _ => anyhow::bail!("Fork state must be idle, failed or closed"),
            }
            f.updated_at = t;
            self.save_fork(f)?;
            return Ok(json!({"ok": true, "state": f.state}));
        };
        if f.done_ops.iter().any(|d| d == op_id) {
            return Ok(json!({"ok": true, "done": true}));
        }
        let op = f
            .ops
            .iter()
            .find(|o| o.op_id == op_id)
            .cloned()
            .context("Unknown op")?;
        let terminal = !matches!(state, "running" | "applying" | "held");
        ensure!(
            [
                "running",
                "applying",
                "held",
                "done",
                "failed",
                "conflict",
                "cancelled"
            ]
            .contains(&state),
            "Op state must be running, applying, held, done, failed, conflict or cancelled"
        );
        let changes = parse_changes(&detail, &session.project_path)?;
        let node_findings = parse_findings(&detail)?;
        let t = now();
        if let Some(head) = commit(&detail, "head")? {
            f.head = Some(head);
        }
        if let Some(changes) = &changes {
            f.changed = changes.clone();
        }
        if let Some(findings) = &node_findings {
            f.findings = findings.clone();
        }
        let mut audits: Vec<(String, &'static str, String, bool, Value)> = vec![];
        match op.kind {
            OpKind::Run => {
                let run_id = op.run_id.clone().unwrap_or_default();
                let run = &detail["run"];
                if let Some(native) = run["native_session_id"]
                    .as_str()
                    .filter(|s| s.len() <= 200 && !s.is_empty())
                {
                    f.native_session_id = Some(native.to_owned());
                }
                if let Some(r) = f.runs.iter_mut().find(|r| r.id == run_id) {
                    if let Some(spent) = run["spent_usd"]
                        .as_f64()
                        .or_else(|| gateway::usd(&run["spent_usd"]))
                    {
                        r.spent_usd = spent.max(0.0);
                    }
                    if let Some(n) = run["requests"].as_u64() {
                        r.requests = n;
                    }
                    if let Some(code) = run["exit"].as_i64() {
                        r.exit = Some(code);
                    }
                    if state == "running" {
                        if r.state == RunState::Dispatched {
                            r.state = RunState::Running;
                            r.started_at = Some(t);
                        }
                    } else if r.state.active() {
                        r.state = match state {
                            "done" => RunState::Succeeded,
                            "cancelled" => RunState::Cancelled,
                            _ => RunState::Failed,
                        };
                        r.ended_at = Some(t);
                        r.error = error.clone();
                    }
                    if r.state == RunState::Running && f.state == ForkState::Creating {
                        f.state = ForkState::Running;
                    }
                }
                if terminal {
                    f.finish_op(op_id);
                    if matches!(f.state, ForkState::Running | ForkState::Creating) {
                        f.state = ForkState::Idle;
                    }
                }
            }
            OpKind::Cancel => {
                if terminal {
                    let run_id = op.run_id.clone().unwrap_or_default();
                    f.ops.retain(|o| {
                        !(o.kind == OpKind::Run && o.run_id.as_deref() == Some(&run_id))
                    });
                    if let Some(r) = f
                        .runs
                        .iter_mut()
                        .find(|r| r.id == run_id && r.state.active())
                    {
                        r.state = RunState::Cancelled;
                        r.ended_at = Some(t);
                    }
                    f.finish_op(op_id);
                    if f.state == ForkState::Running && f.active_run().is_none() {
                        f.state = ForkState::Idle;
                    }
                }
            }
            OpKind::Rebase => {
                if terminal {
                    f.finish_op(op_id);
                    match state {
                        "done" => {
                            if let Some(base) = commit(&detail, "base")?
                                .or_else(|| op.body["rebase"]["onto"].as_str().map(str::to_owned))
                            {
                                f.base = Some(base);
                            }
                            f.rebase_conflict.clear();
                        }
                        "conflict" => {
                            f.rebase_conflict = paths_of(&detail["conflict_paths"])
                                .or_else(|| paths_of(&detail["merge"]["conflict_paths"]))
                                .unwrap_or_default();
                        }
                        _ => f.error = error.clone(),
                    }
                }
            }
            OpKind::Snapshot => {
                if terminal {
                    f.finish_op(op_id);
                    let sub_id = op.submission.clone().unwrap_or_default();
                    let head = f.head.clone();
                    let changed = f.changed.clone();
                    let findings = f.findings.clone();
                    let fork_id = f.id.clone();
                    // The controller's own scan of the diff for this head.
                    let scanned = if f.diff_head == head && head.is_some() {
                        f.diff_findings.clone()
                    } else {
                        vec![]
                    };
                    let dev_head = inner.devs.get(&session.id).and_then(|d| d.head.clone());
                    let (next, outcome) = {
                        let s = inner.subs.get_mut(&sub_id).context("Unknown submission")?;
                        if s.state != SubState::Checking {
                            (None, None)
                        } else if state != "done" {
                            s.state = SubState::Rejected;
                            s.findings = findings;
                            s.reason = Some(error.clone().unwrap_or_else(|| {
                                "The fork failed its checks on the engine machine.".into()
                            }));
                            (None, Some("rejected"))
                        } else if head.as_deref() != Some(s.fork_head.as_str()) {
                            s.state = SubState::Failed;
                            s.reason = Some("The fork changed after you submitted; review the new head and submit again.".into());
                            (None, Some("failed"))
                        } else {
                            evaluate(s, &changed, &[findings, scanned].concat());
                            if s.state == SubState::Merging {
                                (
                                    Some(merge_op(
                                        s,
                                        dev_head
                                            .as_deref()
                                            .or(op.body["snapshot"]["base"].as_str()),
                                        None,
                                    )),
                                    Some("merging"),
                                )
                            } else {
                                (None, Some(s.state.name()))
                            }
                        }
                    };
                    let s = inner.subs[&sub_id].clone();
                    self.save_sub(&s)?;
                    let f = inner.forks.get_mut(&fork_id).unwrap();
                    if let Some(mut op) = next {
                        if op.body["merge"]["expected_dev_head"].is_null() {
                            op.body["merge"]["expected_dev_head"] = json!(f.base);
                        }
                        f.ops.push(op);
                    } else if !s.state.open() {
                        f.state = ForkState::Idle;
                    }
                    if outcome == Some("rejected") {
                        audits.push((
                            s.org_id.clone(),
                            "agent_fork.reject",
                            s.id.clone(),
                            true,
                            json!({"by": "checks", "findings": s.findings.len()}),
                        ));
                    }
                }
            }
            OpKind::Merge => {
                let sub_id = op.submission.clone().unwrap_or_default();
                let merged = commit(&detail["merge"], "dev_head")?;
                let applied = detail["apply"]["applied"].as_bool();
                let held = detail["apply"]["held_reason"]
                    .as_str()
                    .map(|h| h.chars().take(200).collect::<String>());
                let stale = detail["merge"]["stale"] == true;
                if terminal || state == "held" {
                    let fork_id = f.id.clone();
                    if terminal {
                        f.finish_op(op_id);
                    }
                    let dev = inner
                        .devs
                        .entry(session.id.clone())
                        .or_insert_with(|| Dev::new(&session));
                    let before = dev.head.clone();
                    let mut requeue = None;
                    {
                        let s = inner.subs.get_mut(&sub_id).context("Unknown submission")?;
                        s.updated_at = t;
                        match state {
                            "conflict" => {
                                s.state = SubState::Conflict;
                                s.conflict_paths = paths_of(&detail["merge"]["conflict_paths"])
                                    .or_else(|| paths_of(&detail["conflict_paths"]))
                                    .unwrap_or_default();
                                s.reason = Some("The fork conflicts with the live tree; rebase it and submit again.".into());
                            }
                            "failed" if stale && s.merge_attempts < MAX_MERGE_ATTEMPTS => {
                                s.merge_attempts += 1;
                                requeue =
                                    Some(merge_op(s, merged.as_deref(), s.approved_by.as_deref()));
                            }
                            "failed" | "cancelled" => {
                                s.state = SubState::Failed;
                                s.reason = error
                                    .clone()
                                    .or(Some("The merge failed on the engine machine.".into()));
                            }
                            _ => {
                                // done or held: merged into the dev head.
                                if let Some(m) = &merged {
                                    if s.dev_head_before.is_none() {
                                        s.dev_head_before = before.clone();
                                    }
                                    s.dev_head_after = Some(m.clone());
                                    dev.head = Some(m.clone());
                                }
                                if state == "held" || (applied == Some(false)) {
                                    s.state = SubState::Held;
                                    s.held_reason =
                                        held.clone().or(Some("The Editor is busy.".into()));
                                    dev.held = s.held_reason.clone();
                                } else {
                                    s.state = SubState::Applied;
                                    s.applied_at = Some(t);
                                    s.held_reason = None;
                                    dev.held = None;
                                    dev.applied = s.dev_head_after.clone();
                                    dev.applied_at = Some(t);
                                }
                            }
                        }
                    }
                    dev.updated_at = t;
                    let dev = dev.clone();
                    self.save_dev(&dev)?;
                    let s = inner.subs[&sub_id].clone();
                    self.save_sub(&s)?;
                    if s.state == SubState::Applied {
                        audits.push((
                            s.org_id.clone(),
                            "agent_fork.apply",
                            s.id.clone(),
                            true,
                            json!({"fork": s.fork, "dev_head": s.dev_head_after}),
                        ));
                    }
                    let f = inner.forks.get_mut(&fork_id).unwrap();
                    if let Some(op) = requeue {
                        f.ops.push(op);
                    } else if terminal && !matches!(s.state, SubState::Merging | SubState::Applying)
                    {
                        f.state = if f.state == ForkState::Submitted {
                            ForkState::Idle
                        } else {
                            f.state
                        };
                    }
                } else if (state == "applying" || state == "running")
                    && let Some(s) = inner.subs.get_mut(&sub_id)
                {
                    if let Some(m) = &merged {
                        s.dev_head_after = Some(m.clone());
                    }
                    s.state = SubState::Applying;
                    s.updated_at = t;
                    let s = s.clone();
                    self.save_sub(&s)?;
                }
            }
            OpKind::Revert => {
                if terminal {
                    f.finish_op(op_id);
                    let sub_id = op.submission.clone().unwrap_or_default();
                    let merged = commit(&detail["merge"], "dev_head")?;
                    let dev = inner
                        .devs
                        .entry(session.id.clone())
                        .or_insert_with(|| Dev::new(&session));
                    if let Some(s) = inner.subs.get_mut(&sub_id)
                        && let Some(r) = s.reverted.as_mut()
                    {
                        if state == "done" {
                            r.state = "reverted".into();
                            r.dev_head = merged.clone();
                            if let Some(m) = &merged {
                                dev.head = Some(m.clone());
                                if detail["apply"]["applied"] != false {
                                    dev.applied = Some(m.clone());
                                    dev.applied_at = Some(t);
                                }
                            }
                        } else {
                            r.state = "failed".into();
                            dev.notice(json!({"at": t, "kind": "revert_failed", "submission": sub_id, "reason": error.clone().unwrap_or_else(|| state.to_owned())}));
                        }
                        s.updated_at = t;
                        let s = s.clone();
                        self.save_sub(&s)?;
                        audits.push((s.org_id.clone(), "agent_fork.revert", s.id.clone(), state == "done", json!({"auto": s.reverted.as_ref().is_some_and(|r| r.auto), "stage": "applied"})));
                    }
                    dev.updated_at = t;
                    let dev = dev.clone();
                    self.save_dev(&dev)?;
                }
            }
            OpKind::Publish => anyhow::bail!("Publish results arrive in engine-report's dev"),
        }
        let f = inner.forks.get_mut(fork_id).unwrap();
        f.updated_at = t;
        self.save_fork(f)?;
        let reply = json!({"ok": true, "state": f.state});
        drop(guard);
        for (org, action, target, ok, detail) in audits {
            host.audit(&org, "enginebox", action, &target, ok, detail);
        }
        Ok(reply)
    }

    /// `fork-events {session, epoch, fork, generation?, run_id, events:[{kind, data, at?}]}`.
    pub fn events_report(
        &self,
        host: &dyn Host,
        node: &str,
        epoch: u64,
        body: &Value,
    ) -> Result<Value> {
        crate::keys(
            body,
            &["session", "epoch", "fork", "generation", "run_id", "events"],
            &["fork", "run_id", "events"],
        )?;
        let fork_id = body["fork"]
            .as_str()
            .filter(|f| valid_id(f))
            .context("fork required")?;
        let run_id = body["run_id"]
            .as_str()
            .filter(|f| valid_id(f))
            .context("run_id required")?;
        let incoming = body["events"].as_array().context("events required")?;
        ensure!(incoming.len() <= 1000, "At most 1000 events per call");
        {
            let mut inner = self.inner.lock().unwrap();
            let generation = match body.get("generation") {
                Some(g) => g.as_u64().context("generation must be a number")?,
                None => inner.forks.get(fork_id).map_or(0, |f| f.generation),
            };
            let f = Self::reported(&mut inner, host, node, epoch, fork_id, generation)?;
            ensure!(f.runs.iter().any(|r| r.id == run_id), "Unknown run");
        }
        let known = self.known_for(fork_id);
        let _guard = self.events_lock.lock().unwrap();
        let mut stored = self.read_events(fork_id, run_id);
        let mut seq = stored.last().and_then(|e| e["seq"].as_u64()).unwrap_or(0);
        let t = now();
        for e in incoming {
            let kind = e["kind"].as_str().unwrap_or("");
            ensure!(
                ["text", "tool", "status", "usage"].contains(&kind),
                "Event kind must be text, tool, status or usage"
            );
            let mut data = e.get("data").cloned().unwrap_or(Value::Null);
            let mut text = serde_json::to_string(&data)?;
            if text.len() > MAX_EVENT_BYTES {
                let mut cut = MAX_EVENT_BYTES;
                let s = serde_json::to_string(&data)?;
                while !s.is_char_boundary(cut) {
                    cut -= 1;
                }
                data = json!({"truncated": true, "text": &s[..cut]});
                text = serde_json::to_string(&data)?;
            }
            let redacted = classify::redact(&text, &known);
            if redacted != text {
                data = serde_json::from_str(&redacted).unwrap_or(json!({"redacted": true}));
            }
            seq += 1;
            stored.push(json!({"seq": seq, "at": e["at"].as_u64().unwrap_or(t), "kind": kind, "data": data}));
        }
        // Keep the newest events within the byte budget.
        let mut total: usize = stored.iter().map(|e| e.to_string().len() + 1).sum();
        while total > MAX_EVENTS_BYTES && !stored.is_empty() {
            total -= stored[0].to_string().len() + 1;
            stored.remove(0);
        }
        state::write_json(
            &self.events_path(fork_id, run_id),
            &json!({"events": stored}),
        )?;
        Ok(json!({"ok": true, "next": seq}))
    }

    /// `fork-artifact {session, epoch, fork, generation?, head, kind:"diff", body}`.
    pub fn artifact(&self, host: &dyn Host, node: &str, epoch: u64, body: &Value) -> Result<Value> {
        crate::keys(
            body,
            &[
                "session",
                "epoch",
                "fork",
                "generation",
                "head",
                "kind",
                "body",
            ],
            &["fork", "head", "kind", "body"],
        )?;
        ensure!(body["kind"] == "diff", "Only diff artifacts are accepted");
        let fork_id = body["fork"]
            .as_str()
            .filter(|f| valid_id(f))
            .context("fork required")?;
        let head = body["head"]
            .as_str()
            .filter(|h| valid_commit(h))
            .context("head required")?;
        let text = body["body"].as_str().context("body required")?;
        ensure!(text.len() <= MAX_DIFF_BYTES, "Diff is larger than 2 MiB");
        let known = self.known_for(fork_id);
        let mut inner = self.inner.lock().unwrap();
        let generation = match body.get("generation") {
            Some(g) => g.as_u64().context("generation must be a number")?,
            None => inner.forks.get(fork_id).map_or(0, |f| f.generation),
        };
        let f = Self::reported(&mut inner, host, node, epoch, fork_id, generation)?;
        // Scan first (patterns and the bearers this fork's runs received),
        // then never store a bearer Engine Box handed out.
        let findings = classify::scan_diff(text, &known);
        let text = if known.is_empty() {
            text.to_owned()
        } else {
            let mut t = text.to_owned();
            for k in known.iter().filter(|k| k.len() >= 12) {
                t = t.replace(k.as_str(), "[redacted]");
            }
            t
        };
        write_private(
            &self.root.join("fork-diffs").join(format!("{fork_id}.diff")),
            text.as_bytes(),
        )?;
        f.diff_head = Some(head.to_owned());
        f.diff_findings = findings;
        f.updated_at = now();
        self.save_fork(f)?;
        Ok(json!({"ok": true}))
    }

    /// `engine-report`'s `detail.dev` for session `id` (already fenced by the
    /// caller): dev head, applied commit, held reason, compile state, live
    /// code edits and publish results. Compile errors right after a merge was
    /// applied revert that merge (owner decision 2026-11-04 #3).
    pub fn dev_report(&self, host: &dyn Host, session: &SessionInfo, dev: &Value) -> Result<()> {
        ensure!(dev.is_object(), "dev must be an object");
        ensure!(
            serde_json::to_vec(dev)?.len() <= 64 * 1024,
            "dev is too large"
        );
        let t = now();
        let mut audits = vec![];
        let mut inner = self.inner.lock().unwrap();
        let mut d = inner
            .devs
            .get(&session.id)
            .cloned()
            .unwrap_or_else(|| Dev::new(session));
        if let Some(head) = commit(dev, "head")? {
            d.head = Some(head);
        }
        if let Some(applied) = commit(dev, "applied")? {
            if d.applied.as_deref() != Some(applied.as_str()) {
                d.applied_at = Some(t);
            }
            d.applied = Some(applied);
        }
        match dev.get("held") {
            None => {}
            Some(Value::Null) => d.held = None,
            Some(v) => d.held = v.as_str().map(|h| h.chars().take(200).collect()),
        }
        if let Some(paths) = paths_of(&dev["live_code_pending"]) {
            d.live_code_pending = paths;
        }
        if let Some(publish) = dev.get("publish").filter(|p| p.is_object()) {
            let op_id = publish["op_id"].as_str().unwrap_or("");
            if let Some(at) = d.ops.iter().position(|o| o.op_id == op_id) {
                let st = publish["state"].as_str().unwrap_or("");
                if ["done", "failed"].contains(&st) {
                    let op = d.ops.remove(at);
                    d.done_ops.push(op.op_id.clone());
                    self.publish_tokens.lock().unwrap().remove(&op.op_id);
                    d.published = Some(json!({
                        "branch": op.body["publish"]["branch"], "state": st,
                        "commit": commit(publish, "commit")?, "at": t,
                        "error": clean_error(publish.get("error"))?,
                    }));
                }
            }
        }
        let compile = &dev["compile"];
        if compile.is_object() {
            let state = compile["state"].as_str().unwrap_or("");
            ensure!(
                ["ok", "compiling", "errors"].contains(&state),
                "compile.state must be ok, compiling or errors"
            );
            let errors: Vec<Value> = compile["errors"]
                .as_array()
                .map(|l| {
                    l.iter()
                        .take(50)
                        .map(|e| json!({
                            "file": e["file"].as_str().map(|s| s.chars().take(300).collect::<String>()),
                            "line": e["line"].as_u64(),
                            "message": e["message"].as_str().map(|s| s.chars().take(500).collect::<String>()),
                        }))
                        .collect()
                })
                .unwrap_or_default();
            let compiled = commit(compile, "head")?.or_else(|| d.applied.clone());
            d.compile = json!({"state": state, "errors": errors, "head": compiled});
            if state == "ok" {
                d.last_ok = compiled.clone();
            }
            if state == "errors"
                && let Some(applied) = compiled.clone()
                && d.last_ok.as_deref() != Some(applied.as_str())
            {
                // The merge that produced the applied head, if one did.
                let culprit = inner
                    .subs
                    .values()
                    .find(|s| {
                        s.session == session.id
                            && matches!(s.state, SubState::Applied | SubState::Held)
                            && s.dev_head_after.as_deref() == Some(applied.as_str())
                            && s.reverted.is_none()
                    })
                    .cloned();
                if let Some(s) = culprit {
                    let reason = "the merge broke compilation";
                    self.queue_revert(
                        &mut inner,
                        &s,
                        d.head.clone(),
                        true,
                        "enginebox",
                        reason,
                        errors.clone(),
                    )?;
                    d.notice(json!({
                        "at": t, "kind": "auto_revert", "submission": s.id, "fork": s.fork,
                        "author": {"principal": s.owner.principal, "name": s.owner.name},
                        "errors": errors,
                    }));
                    audits.push((
                        s.org_id.clone(),
                        s.id.clone(),
                        json!({"auto": true, "reason": "compile_errors", "errors": errors.len()}),
                    ));
                }
            }
        }
        // A held merge reached the disk.
        if d.held.is_none()
            && let Some(applied) = d.applied.clone()
        {
            for s in inner.subs.values_mut().filter(|s| {
                s.session == session.id
                    && s.state == SubState::Held
                    && s.dev_head_after.as_deref() == Some(applied.as_str())
            }) {
                s.state = SubState::Applied;
                s.held_reason = None;
                s.applied_at = Some(t);
                s.updated_at = t;
                self.save_sub(s)?;
            }
        }
        d.updated_at = t;
        self.save_dev(&d)?;
        inner.devs.insert(session.id.clone(), d);
        drop(inner);
        for (org, target, detail) in audits {
            host.audit(
                &org,
                "enginebox",
                "agent_fork.revert",
                &target,
                true,
                detail,
            );
        }
        Ok(())
    }
}

/// Who is calling, for one session.
struct Access<'a> {
    room_owner: bool,
    caller: &'a Caller,
    session: &'a SessionInfo,
}

/// A run's validated request.
struct RunSpec {
    prompt: String,
    provider: String,
    model: Option<String>,
    budget_usd: f64,
    resume: bool,
}
impl RunSpec {
    /// From a fork or run body; runs inherit provider and model from
    /// `previous`.
    fn parse(body: &Value, previous: Option<(String, Option<String>)>) -> Result<Self> {
        let prompt = body["prompt"]
            .as_str()
            .filter(|p| !p.trim().is_empty() && p.len() <= MAX_PROMPT_BYTES && !p.contains('\0'))
            .ok_or_else(|| refuse(400, "prompt must be 1-16384 bytes of text."))?
            .to_owned();
        let (provider, model) = match previous {
            Some(p) => p,
            None => {
                let provider = body["provider"].as_str().unwrap_or("");
                ensure!(
                    ["claude", "codex"].contains(&provider),
                    refuse(400, "provider must be claude or codex.")
                );
                let model = match body.get("model") {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(
                        v.as_str()
                            .filter(|m| {
                                !m.is_empty()
                                    && m.len() <= 100
                                    && m.bytes()
                                        .all(|b| b.is_ascii_alphanumeric() || b"._/:-".contains(&b))
                            })
                            .ok_or_else(|| refuse(400, "model must be a model id."))?
                            .to_owned(),
                    ),
                };
                (provider.to_owned(), model)
            }
        };
        let budget_usd = match body.get("budget_usd") {
            None | Some(Value::Null) => DEFAULT_BUDGET_USD,
            Some(v) => v
                .as_f64()
                .filter(|b| (MIN_BUDGET_USD..=MAX_BUDGET_USD).contains(b))
                .ok_or_else(|| {
                    refuse(
                        400,
                        format!("budget_usd must be {MIN_BUDGET_USD}-{MAX_BUDGET_USD}."),
                    )
                })?,
        };
        let resume = match body.get("resume") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            _ => return Err(refuse(400, "resume must be true or false.")),
        };
        Ok(Self {
            prompt,
            provider,
            model,
            budget_usd,
            resume,
        })
    }
    fn run(&self, request_id: &str, digest: &str, t: u64) -> Run {
        Run {
            id: new_id(),
            request_id: request_id.to_owned(),
            request_digest: digest.to_owned(),
            state: RunState::Queued,
            provider: self.provider.clone(),
            model: self.model.clone(),
            budget_usd: self.budget_usd,
            timeout_seconds: RUN_SECONDS,
            resume: self.resume,
            created_at: t,
            dispatched_at: None,
            started_at: None,
            ended_at: None,
            spent_usd: 0.0,
            requests: 0,
            exit: None,
            error: None,
        }
    }
    fn op(&self, run: &Run, resume_id: Option<String>) -> Op {
        let mut op = Op::new(
            OpKind::Run,
            json!({"run": {
                "run_id": run.id, "provider": self.provider, "model": self.model,
                "prompt": self.prompt, "resume_id": resume_id,
                "timeout_seconds": run.timeout_seconds, "budget_usd": run.budget_usd,
            }}),
        );
        op.run_id = Some(run.id.clone());
        op
    }
}

fn merge_op(s: &Submission, expected_dev_head: Option<&str>, approved_by: Option<&str>) -> Op {
    let mut op = Op::new(
        OpKind::Merge,
        json!({"merge": {
            "submission": s.id,
            "fork_head": s.fork_head,
            "expected_dev_head": expected_dev_head,
            "message": format!("{} ({})", s.message, s.owner.name),
            "approved_by": approved_by,
        }}),
    );
    op.submission = Some(s.id.clone());
    op
}

/// Classify a frozen fork head and decide: rejected, awaiting approval or
/// merging.
fn evaluate(s: &mut Submission, changed: &[Change], node_findings: &[Finding]) {
    let mut findings: Vec<Finding> = node_findings.to_vec();
    let mut classes = vec![];
    let mut added: u64 = 0;
    for c in changed {
        classes.push(c.class);
        if c.class == Class::Refused {
            findings.push(Finding::new(
                c.rule.as_deref().unwrap_or("refused_path"),
                &c.path,
                None,
            ));
        }
        if c.status != "D" {
            added = added.saturating_add(c.bytes);
            if !c.lfs && c.bytes > classify::MAX_PLAIN_BYTES {
                findings.push(Finding::new("large_file_not_lfs", &c.path, None));
            }
        }
    }
    if changed.len() > classify::MAX_CHANGES {
        findings.push(Finding::new("too_many_changes", "", None));
    }
    if added > classify::MAX_ADDED_BYTES {
        findings.push(Finding::new("too_large", "", None));
    }
    findings.sort();
    findings.dedup();
    findings.truncate(200);
    s.classes = classify::counts(&classes);
    s.findings = findings;
    s.needs_approval = classes.iter().any(|c| {
        matches!(c, Class::Code | Class::Protected) || (c.automatic() && !s.auto_merge.contains(c))
    });
    s.updated_at = now();
    if changed.is_empty() {
        s.state = SubState::Rejected;
        s.reason = Some("Nothing changed since the fork's base.".into());
    } else if !s.findings.is_empty() {
        s.state = SubState::Rejected;
        s.reason = Some("The submission failed its checks; see findings.".into());
    } else if s.needs_approval {
        s.state = SubState::AwaitingApproval;
    } else {
        s.state = SubState::Merging;
    }
}

/// The sections of a unified diff for one path.
fn only_path(diff: &str, path: &str) -> String {
    let mut out = String::new();
    let mut keep = false;
    for line in diff.split_inclusive('\n') {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            keep = rest.trim_end().ends_with(&format!(" b/{path}"));
        }
        if keep {
            out.push_str(line);
        }
    }
    out
}

fn clean_error(value: Option<&Value>) -> Result<Option<String>> {
    Ok(match value {
        None | Some(Value::Null) => None,
        Some(Value::String(e)) => Some(e.chars().filter(|c| !c.is_control()).take(500).collect()),
        _ => anyhow::bail!("error must be a string"),
    })
}
fn commit(detail: &Value, key: &str) -> Result<Option<String>> {
    Ok(match detail.get(key) {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .filter(|c| valid_commit(c))
                .with_context(|| format!("{key} must be a commit id"))?
                .to_owned(),
        ),
    })
}
fn paths_of(value: &Value) -> Option<Vec<String>> {
    value.as_array().map(|l| {
        l.iter()
            .filter_map(Value::as_str)
            .filter(|p| classify::valid_path(p))
            .take(500)
            .map(str::to_owned)
            .collect()
    })
}
/// A Git mode as the node may send it: 33188, 100644 (octal digits as a
/// number) or "100644".
fn mode_of(v: &Value) -> u32 {
    match v {
        Value::String(s) => u32::from_str_radix(s, 8).unwrap_or(0),
        Value::Number(n) => {
            let n = n.as_u64().unwrap_or(0);
            if n > 0o177777 {
                u32::from_str_radix(&n.to_string(), 8).unwrap_or(0)
            } else {
                n as u32
            }
        }
        _ => 0,
    }
}
/// `detail.changed`, classified here (the node's `class` is ignored).
fn parse_changes(detail: &Value, project: &str) -> Result<Option<Vec<Change>>> {
    let Some(list) = detail.get("changed") else {
        return Ok(None);
    };
    let list = list.as_array().context("changed must be a list")?;
    ensure!(
        list.len() <= classify::MAX_CHANGES + 1,
        "changed is too long"
    );
    let mut out = Vec::with_capacity(list.len());
    for c in list {
        let path = c["path"].as_str().context("changed[].path required")?;
        ensure!(path.len() <= 1024, "changed[].path is too long");
        let status: String = c["status"]
            .as_str()
            .unwrap_or("M")
            .chars()
            .take(1)
            .collect();
        ensure!(
            ["A", "M", "D", "R", "C", "T"].contains(&status.as_str()),
            "changed[].status must be A, M, D or R"
        );
        let mode = mode_of(&c["mode"]);
        let (class, rule) = classify::classify(path, mode, project);
        out.push(Change {
            path: path.to_owned(),
            status,
            mode,
            lfs: c["lfs"].as_bool().unwrap_or(false),
            bytes: c["bytes"].as_u64().unwrap_or(0),
            class,
            rule: rule.map(str::to_owned),
        });
    }
    Ok(Some(out))
}
fn parse_findings(detail: &Value) -> Result<Option<Vec<Finding>>> {
    let Some(list) = detail.get("findings") else {
        return Ok(None);
    };
    let list = list.as_array().context("findings must be a list")?;
    Ok(Some(
        list.iter()
            .take(500)
            .filter_map(|f| {
                let rule = f["rule"].as_str().filter(|r| {
                    !r.is_empty()
                        && r.len() <= 64
                        && r.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                })?;
                let path = f["path"]
                    .as_str()
                    .filter(|p| p.len() <= 1024 && !p.chars().any(char::is_control))?;
                Some(Finding::new(rule, path, f["line"].as_u64()))
            })
            .collect(),
    ))
}

#[cfg(test)]
mod tests;
