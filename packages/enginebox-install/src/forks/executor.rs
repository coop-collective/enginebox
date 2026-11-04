//! The node side of the fork protocol (docs/ENGINE-SESSIONS.md "Agent
//! forks", controller PR #128).
//!
//! `engine-poll` session rows carry `forks: [{id, generation, desired, base,
//! keep_snapshot, ops: [Op]}]` and `dev: {capture_on_save, head, ops:
//! [publish]}`. The node creates and closes forks, runs each fork's ops one
//! at a time in order (each `op_id` once), and queues `fork-report`,
//! `fork-events` and `fork-artifact` posts for the node loop, which adds the
//! node's `session` and `epoch`. A session's dev tree registers once its
//! Editor is ready; a watcher captures Editor saves (`saveSeq`) onto the dev
//! head, and `dev` updates (head, applied, held, compile, live code,
//! publish) ride on `ready` engine reports.
//!
//! Report states: without `op_id` the fork's `idle` (created, with `base` and
//! `head`), `failed` or `closed`; with `op_id` the op's `running`,
//! `applying`, `held`, then `done`, `failed`, `conflict` or `cancelled`.
use super::{
    Commit, DevTree, ForkError, ForkId, ForkProvider, ForkTree, SlotAccount,
    git::GitTreeProvider,
    harness::AgentTools,
    relay::{Relay, RelayConfig},
    runner::{self, Gateway, RunOp},
    slots::{Accounts, Limits, SlotPool},
    submit::{self, Bridge, Compile, Dev, DevState, MergeOp},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, watch};

/// Forks per session the node accepts in one poll.
const MAX_FORKS: usize = 32;
/// Events per `fork-events` call (the controller takes up to 1000).
const EVENTS_PER_POST: usize = 200;
/// `fork-artifact` body limit.
const ARTIFACT_BYTES: usize = 2 << 20;
/// How often the dev watcher looks at the bridge.
const WATCH_EVERY: Duration = Duration::from_secs(3);
/// How long a held merge or revert keeps retrying.
const HELD_FOR: Duration = Duration::from_secs(30 * 60);
const HELD_RETRY: Duration = Duration::from_secs(15);

/// A node → controller post (`fork-report`, `fork-events`,
/// `fork-artifact`). The node loop adds `session` (its own) and `epoch`.
#[derive(Clone, Debug)]
pub struct Post {
    pub action: &'static str,
    pub body: Value,
}

/// A `ready` engine report carrying `detail.dev`.
#[derive(Clone, Debug)]
pub struct DevReport {
    pub session: String,
    pub generation: u64,
    pub dev: Value,
}

enum Job {
    Open { base: Option<String> },
    Op(Value),
    Close { keep: bool },
}

struct ForkEntry {
    seen: BTreeSet<String>,
    jobs: mpsc::UnboundedSender<Job>,
    cancel: watch::Sender<bool>,
    gateway: Arc<Mutex<BTreeMap<String, Gateway>>>,
    run: Arc<Mutex<Option<String>>>,
    closed: bool,
}

struct Session {
    generation: u64,
    dev: Arc<Dev>,
    git: Arc<GitTreeProvider>,
    forks: Mutex<BTreeMap<String, ForkEntry>>,
    publishes: Mutex<BTreeSet<String>>,
    stop: watch::Sender<bool>,
}

struct Inner {
    sessions: Mutex<BTreeMap<String, Arc<Session>>>,
    outbox: Mutex<Vec<Post>>,
    dev_reports: Mutex<Vec<DevReport>>,
    relay: tokio::sync::OnceCell<Relay>,
    relay_config: RelayConfig,
    pool: Mutex<SlotPool>,
    tools: Mutex<AgentTools>,
    limits: Limits,
    /// Every account password (redaction).
    account_secrets: Vec<String>,
}

#[derive(Clone)]
pub struct ForkExecutor {
    inner: Arc<Inner>,
}

fn post(inner: &Inner, action: &'static str, body: Value) {
    inner
        .outbox
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Post { action, body });
}

impl ForkExecutor {
    pub fn new(accounts: Option<&Accounts>, tools: AgentTools) -> Self {
        Self::with_relay(accounts, tools, RelayConfig::default())
    }
    pub fn with_relay(accounts: Option<&Accounts>, tools: AgentTools, relay: RelayConfig) -> Self {
        let pool = SlotPool::new(accounts, tools.slots());
        #[cfg(windows)]
        if pool.isolation() == super::slots::Isolation::Accounts {
            // The private desktop is created once; a failure shows at run time.
            let sids: Vec<String> = accounts
                .map(|a| {
                    a.slots
                        .iter()
                        .filter_map(|c| super::windows::account_sid(&c.user).ok())
                        .collect()
                })
                .unwrap_or_default();
            let _ = super::windows::ensure_agent_desktop(&sids);
        }
        Self {
            inner: Arc::new(Inner {
                sessions: Mutex::new(BTreeMap::new()),
                outbox: Mutex::new(Vec::new()),
                dev_reports: Mutex::new(Vec::new()),
                relay: tokio::sync::OnceCell::new(),
                relay_config: relay,
                pool: Mutex::new(pool),
                tools: Mutex::new(tools),
                limits: Limits::default(),
                account_secrets: accounts.map(Accounts::secrets).unwrap_or_default(),
            }),
        }
    }
    /// `engine.json` may change between sessions.
    pub fn set_tools(&self, tools: AgentTools) {
        *self.inner.tools.lock().unwrap_or_else(|e| e.into_inner()) = tools;
    }
    /// `tools.engine.forks` for the heartbeat:
    /// `{provider, slots, free_slots, isolation}`.
    pub fn inventory(&self) -> Value {
        self.inner
            .pool
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .inventory("git")
    }

    /// A session's dev tree is ready (its Editor answers). Starts the save
    /// watcher.
    #[allow(clippy::too_many_arguments)]
    pub async fn register(
        &self,
        session: &str,
        generation: u64,
        tree: DevTree,
        provider: Arc<GitTreeProvider>,
        bridge: Option<Bridge>,
        commit: Commit,
        known: Vec<String>,
    ) -> anyhow::Result<()> {
        let git = provider.clone();
        let p = provider.clone();
        let (t, c) = (tree.clone(), commit.clone());
        tokio::task::spawn_blocking(move || p.start_dev(&t, &c))
            .await?
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut known = known;
        known.extend(self.inner.account_secrets.iter().cloned());
        let dev = Arc::new(Dev {
            tree,
            bridge,
            provider,
            known,
            state: tokio::sync::Mutex::new(DevState {
                applied: commit,
                held: None,
                compile: Compile {
                    state: "unknown".into(),
                    errors: Vec::new(),
                    head: None,
                },
                live_code_pending: Vec::new(),
                saves: None,
                publish: None,
            }),
        });
        let (stop, stopped) = watch::channel(false);
        let s = Arc::new(Session {
            generation,
            dev: dev.clone(),
            git,
            forks: Mutex::new(BTreeMap::new()),
            publishes: Mutex::new(BTreeSet::new()),
            stop,
        });
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session.to_owned(), s);
        tokio::spawn(watch_dev(
            self.inner.clone(),
            session.to_owned(),
            dev,
            stopped,
        ));
        Ok(())
    }

    /// The session ended: stop its runs and watcher, close every fork
    /// (keeping snapshots) and forget it.
    pub async fn unregister(&self, session: &str) {
        let s = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session);
        let Some(s) = s else { return };
        let _ = s.stop.send(true);
        let entries: Vec<ForkEntry> =
            std::mem::take(&mut *s.forks.lock().unwrap_or_else(|e| e.into_inner()))
                .into_values()
                .collect();
        for e in &entries {
            let _ = e.cancel.send(true);
            if !e.closed {
                let _ = e.jobs.send(Job::Close { keep: true });
            }
        }
        // Give workers a moment to end their runs and close.
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    /// Apply one session row (`forks` and `dev`) from `engine-poll`.
    pub fn apply(&self, session: &str, row: &Value) {
        let Some(s) = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session)
            .cloned()
        else {
            // Not ready yet: the controller repeats unprocessed ops.
            return;
        };
        let rows = row["forks"].as_array().cloned().unwrap_or_default();
        let mut forks = s.forks.lock().unwrap_or_else(|e| e.into_inner());
        for f in rows.iter().take(MAX_FORKS) {
            let (Some(id), Some(generation)) = (f["id"].as_str(), f["generation"].as_u64()) else {
                continue;
            };
            let Ok(fid) = ForkId::parse(id) else { continue };
            let open = f["desired"] == "open";
            let keep = f["keep_snapshot"] != false;
            let entry = forks.entry(id.to_owned()).or_insert_with(|| {
                let e = spawn_fork(&self.inner, session, &s, fid.clone(), generation);
                if open {
                    let _ = e.jobs.send(Job::Open {
                        base: f["base"].as_str().map(str::to_owned),
                    });
                }
                e
            });
            let ops: Vec<&Value> = f["ops"].as_array().into_iter().flatten().take(16).collect();
            for op in &ops {
                // The run's xinf token, re-delivered (refreshed) every poll
                // while the run is in flight; memory only.
                if op["kind"] == "run"
                    && let (Some(run_id), Ok(g)) = (
                        op["run"]["run_id"].as_str(),
                        serde_json::from_value::<Gateway>(op["secrets"]["gateway"].clone()),
                    )
                {
                    let active = entry.run.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    if active.as_deref() == Some(run_id)
                        && let Some(relay) = self.inner.relay.get()
                    {
                        relay.refresh(run_id, g.access_token.clone(), g.expiry());
                    }
                    entry
                        .gateway
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(run_id.to_owned(), g);
                }
                let Some(op_id) = op["op_id"].as_str() else {
                    continue;
                };
                if op_id.is_empty() || op_id.len() > 64 || !entry.seen.insert(op_id.to_owned()) {
                    continue;
                }
                if op["kind"] == "cancel" {
                    let active = entry.run.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    let wanted = op["cancel"]["run_id"].as_str();
                    if wanted.is_none() || wanted == active.as_deref() {
                        let _ = entry.cancel.send(true);
                    }
                    post(
                        &self.inner,
                        "fork-report",
                        json!({"fork": id, "generation": generation, "op_id": op_id, "state": "done"}),
                    );
                    continue;
                }
                let _ = entry.jobs.send(Job::Op((*op).clone()));
            }
            if !open && !entry.closed {
                // After any ops already queued (a closed fork's revert).
                entry.closed = true;
                let _ = entry.cancel.send(true);
                let _ = entry.jobs.send(Job::Close { keep });
            }
        }
        drop(forks);
        for op in row["dev"]["ops"].as_array().into_iter().flatten().take(4) {
            let Some(op_id) = op["op_id"].as_str() else {
                continue;
            };
            if op["kind"] != "publish"
                || op_id.is_empty()
                || op_id.len() > 64
                || !s
                    .publishes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(op_id.to_owned())
            {
                continue;
            }
            tokio::spawn(publish(
                self.inner.clone(),
                session.to_owned(),
                s.dev.clone(),
                s.git.clone(),
                op_id.to_owned(),
                op.clone(),
            ));
        }
    }

    /// Posts queued since the last call (events merged per run, in order).
    pub fn drain(&self) -> Vec<Post> {
        let posts =
            std::mem::take(&mut *self.inner.outbox.lock().unwrap_or_else(|e| e.into_inner()));
        let mut out: Vec<Post> = Vec::new();
        for p in posts {
            if p.action == "fork-events"
                && let Some(last) = out.last_mut()
                && last.action == "fork-events"
                && last.body["fork"] == p.body["fork"]
                && last.body["run_id"] == p.body["run_id"]
                && last.body["events"].as_array().map_or(0, Vec::len)
                    + p.body["events"].as_array().map_or(0, Vec::len)
                    <= EVENTS_PER_POST
                && let Some(events) = last.body["events"].as_array_mut()
            {
                events.extend(p.body["events"].as_array().cloned().unwrap_or_default());
                continue;
            }
            out.push(p);
        }
        out
    }
    /// `dev` updates for `engine-report`.
    pub fn drain_dev(&self) -> Vec<DevReport> {
        std::mem::take(
            &mut *self
                .inner
                .dev_reports
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        )
    }
}

fn spawn_fork(
    inner: &Arc<Inner>,
    session: &str,
    s: &Arc<Session>,
    id: ForkId,
    generation: u64,
) -> ForkEntry {
    let (jobs, rx) = mpsc::unbounded_channel();
    let (cancel, cancelled) = watch::channel(false);
    let gateway = Arc::new(Mutex::new(BTreeMap::new()));
    let run = Arc::new(Mutex::new(None));
    tokio::spawn(fork_worker(
        Worker {
            inner: inner.clone(),
            session: session.to_owned(),
            dev: s.dev.clone(),
            id,
            generation,
            gateway: gateway.clone(),
            run: run.clone(),
            cancelled,
            cancel_tx: cancel.clone(),
        },
        rx,
    ));
    ForkEntry {
        seen: BTreeSet::new(),
        jobs,
        cancel,
        gateway,
        run,
        closed: false,
    }
}

struct Worker {
    inner: Arc<Inner>,
    session: String,
    dev: Arc<Dev>,
    id: ForkId,
    generation: u64,
    gateway: Arc<Mutex<BTreeMap<String, Gateway>>>,
    run: Arc<Mutex<Option<String>>>,
    cancelled: watch::Receiver<bool>,
    cancel_tx: watch::Sender<bool>,
}
impl Worker {
    fn report(&self, op_id: Option<&str>, state: &str, error: Option<String>, detail: Value) {
        let known: Vec<&str> = self.dev.known.iter().map(String::as_str).collect();
        let mut body = json!({"fork": self.id.as_str(), "generation": self.generation,
            "state": state, "detail": detail});
        if let Some(op) = op_id {
            body["op_id"] = json!(op);
        }
        if let Some(e) = error {
            let e = super::classify::redact(&e, &known);
            body["error"] = json!(e.chars().take(600).collect::<String>());
        }
        post(&self.inner, "fork-report", body);
    }
    fn tree(&self) -> ForkTree {
        ForkTree::at(&self.dev.tree, &self.id)
    }
    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&dyn ForkProvider, &DevTree, &ForkTree) -> super::Result<T> + Send + 'static,
    ) -> super::Result<T> {
        let (p, s, t) = (
            self.dev.provider.clone(),
            self.dev.tree.clone(),
            self.tree(),
        );
        tokio::task::spawn_blocking(move || f(p.as_ref(), &s, &t))
            .await
            .map_err(|e| ForkError::Other(e.into()))?
    }
    fn changed(&self, snap: &super::Snapshot) -> Value {
        json!(
            snap.changed
                .iter()
                .map(|c| {
                    let (class, _) =
                        super::classify::classify(&c.path, c.mode, &self.dev.tree.project_path);
                    json!({"path": c.path, "status": c.status.to_string(), "mode": format!("{:o}", c.mode),
                        "lfs": c.lfs, "bytes": c.bytes, "class": class})
                })
                .collect::<Vec<_>>()
        )
    }
    fn snapshot_detail(&self, snap: &super::Snapshot) -> Value {
        json!({"head": snap.head, "base": snap.base, "changed": self.changed(snap),
            "added_bytes": snap.added_bytes})
    }
    /// The diff for `head` (before the report naming it, so the
    /// controller's scan of the artifact matches the reported head).
    async fn artifact(&self, head: &Commit) {
        if let Ok(diff) = self
            .blocking(|p, _, t| p.diff(t, None, ARTIFACT_BYTES - 1024))
            .await
        {
            post(
                &self.inner,
                "fork-artifact",
                json!({"fork": self.id.as_str(), "generation": self.generation, "head": head,
                    "kind": "diff", "body": diff}),
            );
        }
    }
}

fn findings(e: &ForkError) -> Value {
    match e {
        ForkError::Refused { findings } => json!(findings),
        _ => json!([]),
    }
}
fn conflict_paths(e: &ForkError) -> Value {
    match e {
        ForkError::Conflict { paths } => json!(paths),
        _ => json!([]),
    }
}
/// The op report state for a failed fork operation.
fn op_state(e: &ForkError) -> &'static str {
    match e {
        ForkError::Conflict { .. } => "conflict",
        ForkError::Held(_) => "held",
        _ => "failed",
    }
}

async fn fork_worker(mut w: Worker, mut jobs: mpsc::UnboundedReceiver<Job>) {
    while let Some(job) = jobs.recv().await {
        match job {
            Job::Open { base } => {
                let base = base.and_then(|b| Commit::parse(&b).ok());
                let r = w
                    .blocking(move |p, s, t| {
                        if !t.exists() {
                            p.create(s, &t.id, base.as_ref(), &SlotAccount::node())?;
                        }
                        p.snapshot(&ForkTree::at(s, &t.id), "Fork created")
                    })
                    .await;
                match r {
                    Ok(snap) => w.report(
                        None,
                        "idle",
                        None,
                        json!({"base": snap.base, "head": snap.head}),
                    ),
                    Err(e) => w.report(None, "failed", Some(e.to_string()), json!({})),
                }
            }
            Job::Close { keep } => {
                let _ = w
                    .blocking(move |p, _, t| if t.exists() { p.close(t, keep) } else { Ok(()) })
                    .await;
                w.report(None, "closed", None, json!({}));
            }
            Job::Op(op) => run_op(&mut w, &op).await,
        }
    }
}

async fn run_op(w: &mut Worker, op: &Value) {
    let op_id = op["op_id"].as_str().unwrap_or("").to_owned();
    let op_id = op_id.as_str();
    let kind = op["kind"].as_str().unwrap_or("");
    if kind != "revert" && !w.tree().exists() {
        w.report(
            Some(op_id),
            "failed",
            Some("The fork does not exist on this machine".into()),
            json!({}),
        );
        return;
    }
    match kind {
        "snapshot" => {
            // Freeze for a submission: the node's checks (pre-scan, classes,
            // secret scan) plus the diff artifact, then the report.
            let checks = submit::check(
                w.dev.provider.clone(),
                w.tree(),
                w.dev.tree.project_path.clone(),
                w.dev.known.clone(),
            )
            .await;
            match checks {
                Ok(c) => {
                    w.artifact(&c.snapshot.head).await;
                    let mut detail = w.snapshot_detail(&c.snapshot);
                    detail["findings"] = json!(c.findings);
                    w.report(Some(op_id), "done", None, detail);
                }
                Err(e) => w.report(
                    Some(op_id),
                    "failed",
                    Some(e.to_string()),
                    json!({"findings": findings(&e)}),
                ),
            }
        }
        "rebase" => {
            let onto = op["rebase"]["onto"]
                .as_str()
                .and_then(|c| Commit::parse(c).ok());
            let r = w
                .blocking(move |p, s, t| {
                    let onto = match onto {
                        Some(c) => c,
                        None => p.dev_head(s)?,
                    };
                    p.rebase(s, t, &onto)
                })
                .await;
            match r {
                Ok(snap) => {
                    w.artifact(&snap.head).await;
                    w.report(Some(op_id), "done", None, w.snapshot_detail(&snap));
                }
                Err(e) => w.report(
                    Some(op_id),
                    op_state(&e),
                    Some(e.to_string()),
                    json!({"conflict_paths": conflict_paths(&e)}),
                ),
            }
        }
        "merge" => {
            let Ok(m) = serde_json::from_value::<MergeOp>(op["merge"].clone()) else {
                w.report(
                    Some(op_id),
                    "failed",
                    Some("merge op without merge".into()),
                    json!({}),
                );
                return;
            };
            let started = std::time::Instant::now();
            loop {
                let out = w.dev.submit(w.tree(), &m).await;
                let state = match out.state.as_str() {
                    "applied" => "done",
                    "held" => "held",
                    "conflict" => "conflict",
                    _ => "failed",
                };
                let mut merge = json!({"conflict_paths": out.conflict_paths});
                if let Some(h) = out.applied.as_ref().or(if out.stale {
                    out.dev_head.as_ref()
                } else {
                    None
                }) {
                    merge["dev_head"] = json!(h);
                }
                if out.stale {
                    merge["stale"] = json!(true);
                }
                let detail = json!({"merge": merge,
                    "apply": {"applied": out.applied.is_some(), "held_reason": out.held_reason},
                    "findings": out.findings, "classes": out.classes});
                w.report(Some(op_id), state, out.error.clone(), detail);
                if out.applied.is_some() {
                    push_dev(&w.inner, &w.session, &w.dev).await;
                    // The compile result follows in `dev`.
                    w.dev.settle(out.code_changed).await;
                    push_dev(&w.inner, &w.session, &w.dev).await;
                }
                if state != "held" || started.elapsed() > HELD_FOR || *w.cancelled.borrow() {
                    return;
                }
                tokio::time::sleep(HELD_RETRY).await;
            }
        }
        "revert" => {
            let commit = op["revert"]["commit"].as_str().unwrap_or("").to_owned();
            let msg = op["revert"]["message"].as_str().unwrap_or("").to_owned();
            let started = std::time::Instant::now();
            loop {
                let out = w.dev.revert(&commit, &msg).await;
                let state = match out.state.as_str() {
                    "applied" => "done",
                    "held" => "held",
                    "conflict" => "conflict",
                    _ => "failed",
                };
                w.report(
                    Some(op_id),
                    state,
                    out.error.clone(),
                    json!({"merge": {"dev_head": out.applied, "conflict_paths": out.conflict_paths},
                        "apply": {"applied": out.applied.is_some(), "held_reason": out.held_reason}}),
                );
                if out.applied.is_some() {
                    push_dev(&w.inner, &w.session, &w.dev).await;
                    w.dev.settle(true).await;
                    push_dev(&w.inner, &w.session, &w.dev).await;
                }
                if state != "held" || started.elapsed() > HELD_FOR {
                    return;
                }
                tokio::time::sleep(HELD_RETRY).await;
            }
        }
        "run" => run_agent(w, op_id, op).await,
        other => w.report(
            Some(op_id),
            "failed",
            Some(format!("unknown op {other}")),
            json!({}),
        ),
    }
}

async fn run_agent(w: &mut Worker, op_id: &str, op: &Value) {
    let Ok(run) = serde_json::from_value::<RunOp>(op["run"].clone()) else {
        w.report(
            Some(op_id),
            "failed",
            Some("run op without run".into()),
            json!({}),
        );
        return;
    };
    let fail = |w: &Worker, msg: &str| {
        w.report(
            Some(op_id),
            "failed",
            Some(msg.into()),
            json!({"run": {"run_id": run.run_id}}),
        );
    };
    if !(0.0..=100.0).contains(&run.budget_usd) || !super::hex32(&run.run_id) {
        return fail(w, "The run's budget or id is invalid");
    }
    let gateway = serde_json::from_value::<Gateway>(op["secrets"]["gateway"].clone())
        .ok()
        .or_else(|| {
            w.gateway
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&run.run_id)
                .cloned()
        });
    let Some(gateway) = gateway else {
        return fail(w, "Connect Xava Inference and add balance to run agents");
    };
    let relay = match w
        .inner
        .relay
        .get_or_try_init(|| Relay::start(w.inner.relay_config.clone()))
        .await
    {
        Ok(r) => r.clone(),
        Err(e) => return fail(w, &format!("The gateway relay did not start: {e:#}")),
    };
    // A cancel that arrived before this run belongs to an earlier one.
    let _ = w.cancel_tx.send(false);
    // Wait for a free slot (the controller schedules at most `slots` runs).
    let slot = loop {
        if let Some(s) = w
            .inner
            .pool
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .acquire()
        {
            break s;
        }
        if *w.cancelled.borrow() {
            w.report(
                Some(op_id),
                "cancelled",
                None,
                json!({"run": {"run_id": run.run_id}}),
            );
            return;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    };
    *w.run.lock().unwrap_or_else(|e| e.into_inner()) = Some(run.run_id.clone());
    w.report(
        Some(op_id),
        "running",
        None,
        json!({"run": {"run_id": run.run_id, "slot": slot.account.name}}),
    );
    let tools = w
        .inner
        .tools
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let ctx = runner::Context {
        provider: w.dev.provider.as_ref(),
        relay: &relay,
        tools: &tools,
        limits: w.inner.limits,
        known: w.dev.known.clone(),
    };
    let (inner, fork, generation, run_id) = (
        w.inner.clone(),
        w.id.as_str().to_owned(),
        w.generation,
        run.run_id.clone(),
    );
    let flush = move |batch: Vec<runner::Event>| {
        let events: Vec<Value> = batch
            .into_iter()
            .map(|e| json!({"kind": e.kind, "data": e.data, "at": e.unix}))
            .collect();
        post(
            &inner,
            "fork-events",
            json!({"fork": fork, "generation": generation, "run_id": run_id, "events": events}),
        );
    };
    let mut batch: Vec<runner::Event> = Vec::new();
    let tree = w.tree();
    let outcome = runner::run(
        &ctx,
        &tree,
        &slot,
        &run,
        &gateway,
        w.cancelled.clone(),
        |e| {
            batch.push(e);
            if batch.len() >= 20 {
                flush(std::mem::take(&mut batch));
            }
        },
    )
    .await;
    if !batch.is_empty() {
        flush(batch);
    }
    w.inner
        .pool
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .release(&slot.account.name);
    *w.run.lock().unwrap_or_else(|e| e.into_inner()) = None;
    w.gateway
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&run.run_id);
    let mut detail = json!({"run": {"run_id": run.run_id, "native_session_id": outcome.native_session_id,
        "spent_usd": outcome.spent_usd, "requests": outcome.requests, "exit": outcome.exit,
        "state": outcome.state}});
    if let Some(snap) = &outcome.snapshot {
        w.artifact(&snap.head).await;
        let s = w.snapshot_detail(snap);
        for k in ["head", "base", "changed"] {
            detail[k] = s[k].clone();
        }
    }
    let state = match outcome.state.as_str() {
        "completed" => "done",
        "cancelled" => "cancelled",
        _ => "failed",
    };
    w.report(Some(op_id), state, outcome.error.clone(), detail);
}

/// `dev.ops` `publish {branch, head}`: push the dev head to `jam/<name>`
/// with the one-shot repository token (memory only), LFS objects first.
async fn publish(
    inner: Arc<Inner>,
    session: String,
    dev: Arc<Dev>,
    git: Arc<GitTreeProvider>,
    op_id: String,
    op: Value,
) {
    let branch = op["publish"]["branch"].as_str().unwrap_or("").to_owned();
    let head = op["publish"]["head"].as_str().unwrap_or("").to_owned();
    let secret = serde_json::from_value::<crate::engine::RepositorySecret>(
        op["secrets"]["repository"].clone(),
    );
    let known: Vec<String> = dev.known.clone();
    let result: anyhow::Result<Commit> = async {
        let access =
            secret.map_err(|_| anyhow::anyhow!("The publish op carries no repository token"))?;
        anyhow::ensure!(
            branch.starts_with("jam/")
                && branch.len() <= 100
                && branch[4..]
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_./".contains(&b))
                && !branch.contains("..")
                && !branch.ends_with('/')
                && !branch.ends_with(".lock"),
            "Invalid publish branch"
        );
        anyhow::ensure!(
            access.remote.starts_with("https://"),
            "Publishing needs an https remote"
        );
        let mut state = dev.state.lock().await;
        let p = git.clone();
        let t = dev.tree.clone();
        let commit = if head.is_empty() {
            let (p, t) = (dev.provider.clone(), t.clone());
            tokio::task::spawn_blocking(move || p.dev_head(&t))
                .await?
                .map_err(|e| anyhow::anyhow!("{e}"))?
        } else {
            Commit::parse(&head).map_err(|e| anyhow::anyhow!("{e}"))?
        };
        let c = commit.clone();
        tokio::task::spawn_blocking(move || {
            let env = crate::engine::git_env(&access);
            let push = |args: &[&str]| -> anyhow::Result<()> {
                let mut cmd = p.command(&t.git_dir, Some(&t.work_tree), Some(&t.index));
                cmd.envs(
                    env.iter()
                        .filter(|(k, _)| k != "GIT_CONFIG_NOSYSTEM")
                        .map(|(k, v)| (k, v)),
                );
                cmd.args(args);
                let out = cmd.output()?;
                anyhow::ensure!(
                    out.status.success(),
                    "git {} failed: {}",
                    args[0],
                    String::from_utf8_lossy(&out.stderr)
                        .lines()
                        .rfind(|l| !l.trim().is_empty())
                        .unwrap_or("")
                );
                Ok(())
            };
            if p.lfs() {
                push(&["lfs", "push", &access.remote, c.as_str()])?;
            }
            push(&[
                "push",
                "--no-verify",
                &access.remote,
                &format!("{}:refs/heads/{branch}", c.as_str()),
            ])
        })
        .await??;
        state.publish = None;
        drop(state);
        Ok(commit)
    }
    .await;
    let refs: Vec<&str> = known.iter().map(String::as_str).collect();
    let report = match result {
        Ok(commit) => json!({"op_id": op_id, "state": "done", "commit": commit}),
        Err(e) => json!({"op_id": op_id, "state": "failed",
            "error": super::classify::redact(&format!("{e:#}"), &refs).chars().take(400).collect::<String>()}),
    };
    dev.state.lock().await.publish = Some(report);
    push_dev(&inner, &session, &dev).await;
}

async fn push_dev(inner: &Inner, session: &str, dev: &Dev) {
    let generation = inner
        .sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(session)
        .map(|s| s.generation);
    if let Some(generation) = generation {
        let dev = dev.report().await;
        inner
            .dev_reports
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(DevReport {
                session: session.to_owned(),
                generation,
                dev,
            });
    }
}

/// Capture Editor saves onto the dev head while the session runs.
async fn watch_dev(
    inner: Arc<Inner>,
    session: String,
    dev: Arc<Dev>,
    mut stop: watch::Receiver<bool>,
) {
    let mut last: Option<Value> = None;
    loop {
        tokio::select! {
            _ = stop.changed() => if *stop.borrow() { return },
            _ = tokio::time::sleep(WATCH_EVERY) => {}
        }
        let _ = dev.capture(false, Vec::new()).await;
        let report = dev.report().await;
        if last.as_ref() != Some(&report) {
            last = Some(report);
            push_dev(&inner, &session, &dev).await;
        }
    }
}

/// The node-private paths of a remote's checkout: (dev tree, git dir, index).
pub fn repo_paths(work: &std::path::Path, key: &str) -> (PathBuf, PathBuf, PathBuf) {
    let engine = work.join("engine");
    (
        engine.join("checkouts").join(key),
        engine.join("repos").join(format!("{key}.git")),
        engine.join("repos").join(format!("{key}.index")),
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::forks::git::tests::fixture;

    async fn wait_for(ex: &ForkExecutor, posts: &mut Vec<Post>, done: impl Fn(&[Post]) -> bool) {
        for _ in 0..150 {
            posts.extend(ex.drain());
            if done(posts) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("timed out: {posts:#?}");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn poll_rows_drive_the_fork_protocol() {
        let f = fixture();
        let bin = f.s.forks_root.parent().unwrap().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let claude = bin.join("claude");
        std::fs::write(
            &claude,
            "#!/bin/sh\ncat >/dev/null\nmkdir -p Assets/Art && echo hi > Assets/Art/x.txt\n\
echo '{\"type\":\"result\",\"session_id\":\"s-9\",\"is_error\":false,\"result\":\"ok\"}'\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        let ex = ForkExecutor::new(
            None,
            AgentTools {
                claude: Some(claude),
                ..Default::default()
            },
        );
        let inv = ex.inventory();
        assert_eq!(
            (
                inv["provider"].as_str(),
                inv["isolation"].as_str(),
                inv["slots"].as_u64()
            ),
            (Some("git"), Some("none"), Some(1))
        );
        let provider = Arc::new(
            GitTreeProvider::new(
                "git".into(),
                &f.s.git_dir.parent().unwrap().join(".node"),
                false,
            )
            .unwrap(),
        );
        let head = f.p.dev_head(&f.s).unwrap();
        let sid = f.s.session.clone();
        ex.register(
            &sid,
            1,
            f.s.clone(),
            provider,
            None,
            head.clone(),
            vec!["session-secret-1234".into()],
        )
        .await
        .unwrap();
        let fid = "d".repeat(32);
        let rid = "e".repeat(32);
        let gateway = json!({"access_token": "xat_member_token_value", "expires_at": 0,
            "base_url": "https://xavainf.com", "kind": "key"});
        let row = json!({"forks": [{"id": fid, "generation": 1, "desired": "open", "base": head, "keep_snapshot": true,
            "ops": [{"op_id": "o1", "kind": "run", "run": {"run_id": rid, "provider": "claude", "model": null,
                        "prompt": "hi", "resume_id": null, "budget_usd": 1.0, "timeout_seconds": 60},
                     "secrets": {"gateway": gateway}}]}],
            "dev": {"capture_on_save": true, "head": head, "ops": []}});
        ex.apply(&sid, &row);
        ex.apply(&sid, &row); // repeated ops run once
        let mut posts = Vec::new();
        wait_for(&ex, &mut posts, |p| {
            p.iter()
                .any(|p| p.body["op_id"] == "o1" && p.body["state"] == "done")
        })
        .await;
        let reports: Vec<&Value> = posts
            .iter()
            .filter(|p| p.action == "fork-report")
            .map(|p| &p.body)
            .collect();
        let created = reports.iter().find(|r| r.get("op_id").is_none()).unwrap();
        assert_eq!(
            (created["state"].as_str(), &created["detail"]["base"]),
            (Some("idle"), &json!(head))
        );
        assert_eq!(
            reports
                .iter()
                .filter(|r| r["op_id"] == "o1" && r["state"] == "running")
                .count(),
            1
        );
        let run = reports
            .iter()
            .find(|r| r["op_id"] == "o1" && r["state"] == "done")
            .unwrap();
        assert_eq!(run["detail"]["run"]["native_session_id"], "s-9");
        assert_eq!(run["detail"]["changed"][0]["status"], "A");
        assert_eq!(run["detail"]["changed"][0]["mode"], "100644");
        let events = posts.iter().find(|p| p.action == "fork-events").unwrap();
        assert!(events.body["events"][0]["at"].is_u64());
        assert_eq!(events.body["run_id"], json!(rid));
        // The artifact for a head comes before the report naming it.
        let art = posts
            .iter()
            .position(|p| p.action == "fork-artifact")
            .unwrap();
        let done = posts
            .iter()
            .position(|p| p.body["op_id"] == "o1" && p.body["state"] == "done")
            .unwrap();
        assert!(art < done);
        // Strict controller keys: no engine session or node fields.
        for p in &posts {
            assert!(p.body.get("session").is_none() && p.body.get("node_session").is_none());
        }
        // Snapshot for a submission, then the merge.
        let fork_head = run["detail"]["head"].as_str().unwrap().to_owned();
        posts.clear();
        ex.apply(&sid, &json!({"forks": [{"id": fid, "generation": 1, "desired": "open",
            "ops": [{"op_id": "o2", "kind": "snapshot", "snapshot": {"expected_head": fork_head, "submission": "a".repeat(32)}},
                    {"op_id": "o3", "kind": "merge", "merge": {"submission": "a".repeat(32), "fork_head": fork_head,
                        "expected_dev_head": head, "message": "Merge", "approved_by": null}}]}]}));
        wait_for(&ex, &mut posts, |p| {
            p.iter()
                .any(|p| p.body["op_id"] == "o3" && p.body["state"] != "running")
        })
        .await;
        let snap = posts.iter().find(|p| p.body["op_id"] == "o2").unwrap();
        assert_eq!(
            (
                snap.body["state"].as_str(),
                snap.body["detail"]["head"].as_str()
            ),
            (Some("done"), Some(fork_head.as_str()))
        );
        assert_eq!(snap.body["detail"]["findings"], json!([]));
        let merge = posts.iter().find(|p| p.body["op_id"] == "o3").unwrap();
        assert_eq!(merge.body["state"], "done", "{merge:?}");
        assert_eq!(merge.body["detail"]["apply"]["applied"], true);
        let merged = merge.body["detail"]["merge"]["dev_head"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            f.s.work_tree
                .join("unity-project/Assets/Art/x.txt")
                .is_file()
        );
        let dev = ex.drain_dev();
        assert!(
            dev.iter()
                .any(|d| d.dev["applied"] == json!(merged) && d.dev["held"].is_null())
        );
        // A revert op on the fork after it closed.
        posts.clear();
        ex.apply(&sid, &json!({"forks": [{"id": fid, "generation": 1, "desired": "closed", "keep_snapshot": true,
            "ops": [{"op_id": "o4", "kind": "revert", "revert": {"submission": "a".repeat(32), "commit": merged,
                "parent": head, "expected_dev_head": merged, "message": "Revert", "auto": true}}]}]}));
        wait_for(&ex, &mut posts, |p| {
            p.iter().any(|p| p.body["state"] == "closed")
        })
        .await;
        let revert = posts.iter().find(|p| p.body["op_id"] == "o4").unwrap();
        assert_eq!(revert.body["state"], "done", "{revert:?}");
        assert!(
            !f.s.work_tree
                .join("unity-project/Assets/Art/x.txt")
                .exists()
        );
        assert!(!ForkTree::at(&f.s, &ForkId::parse(&fid).unwrap()).exists());
        let all = format!("{posts:?}");
        assert!(!all.contains("xat_member") && !all.contains("ebr_"));
        ex.unregister(&sid).await;
    }
}
