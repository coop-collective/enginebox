use crate::{
    auth, delegation,
    launch_events::{self, LaunchEvent, LaunchEvents, LaunchFailure, Stage},
    native, pools, process, profile,
    queue::{self, Entry, EntryState, HostLoad, Placement, QueueLimits, StoredCallback, Target},
    repository, run_details, security, usage,
};
use anyhow::{Result, bail, ensure};
use enginebox_core::{Fleet, Host};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
/// Largest private queue entry: a 16 KiB request plus callback and bookkeeping.
/// How long a run waits for the host operation lock (pool creations hold it
/// while a workspace is cloned and prepared).
const HOST_LOCK_WAIT: Duration = Duration::from_secs(600);
const QUEUE_ENTRY_LIMIT: usize = 128 * 1024;
/// Finished runs retained (records and unplaced pool runs each).
const RETAINED_FINISHED: usize = 100;
/// Wait after a failed pool workspace creation before trying again.
const CREATE_BACKOFF: Duration = Duration::from_secs(60);
/// How long a workspace's guest helper, once seen at this release, is
/// trusted before placement checks it again.
const GUEST_CHECK_TTL: Duration = Duration::from_secs(600);
#[derive(Clone)]
pub struct AgentService {
    inner: Arc<Inner>,
}
struct Inner {
    root: PathBuf,
    runs_root: PathBuf,
    details_root: PathBuf,
    details: Mutex<BTreeMap<String, Value>>,
    fleet: Fleet,
    binaries: BTreeMap<String, Option<String>>,
    sessions: Mutex<BTreeMap<String, Session>>,
    records: Mutex<BTreeMap<String, Value>>,
    leases: Mutex<BTreeSet<(String, String, String)>>,
    pending: AtomicUsize,
    closed: AtomicBool,
    usage: usage::Cache,
    status_cache: Mutex<BTreeMap<String, (std::time::Instant, Value)>>,
    receipt_lock: Mutex<()>,
    /// Callback targets and keys of runs still in flight. Memory only: the
    /// key never reaches a record, and a restart ends delivery for that run.
    callbacks: Mutex<BTreeMap<String, Callback>>,
    finish_hook: Mutex<Option<FinishHook>>,
    queue_root: PathBuf,
    limits: Mutex<QueueLimits>,
    pools: pools::Pools,
    /// Scheduler state. Lock order: `sched`, then `leases`, `records`,
    /// `details`, `callbacks`. Nothing takes `sched` while holding another.
    sched: Mutex<Sched>,
    wake: Condvar,
    admitting: AtomicBool,
    dispatcher: AtomicBool,
    measures: Mutex<BTreeMap<String, Measure>>,
    /// Lease probes of workspaces fenced by an uncertain run, per
    /// (host, workspace). A leaf lock: nothing else is taken while held.
    probes: Mutex<BTreeMap<(String, String), Probe>>,
    /// The last guest helper check per workspace: when, and why it is
    /// outdated (`None`: it is this release's). A leaf lock.
    guests: Mutex<BTreeMap<(String, String), GuestCheck>>,
    hooks: Hooks,
    /// Runs interrupted by a restart whose callback is still owed.
    owed_callbacks: Mutex<Vec<String>>,
    /// Bounded history of launch failures and pool member creations.
    launch_events: LaunchEvents,
}

#[derive(Default)]
struct Sched {
    entries: BTreeMap<String, Queued>,
    next_seq: u64,
    cursor: Option<String>,
    /// Admitted runs and where they run.
    running: BTreeMap<String, Placement>,
    /// Per host: runs admitted since its last measurement and still running.
    admitted_since: BTreeMap<String, BTreeSet<String>>,
    creating: BTreeMap<String, usize>,
    backoff: BTreeMap<String, (Instant, String)>,
    waiting: BTreeMap<String, (usize, String)>,
    dirty: bool,
}
struct Queued {
    entry: Entry,
    /// The profile resolved at acceptance. After a restart it is resolved
    /// again at admission through the profile resolver.
    resolved: Option<ResolvedProfile>,
}
struct Measure {
    at: Instant,
    load: Option<HostLoad>,
}
/// When a workspace's guest helper was checked, and why it is outdated.
type GuestCheck = (Instant, Option<String>);
/// The last lease probe of a fenced workspace.
struct Probe {
    /// When the last probe finished; `None` before the first.
    last: Option<Instant>,
    in_flight: bool,
    /// Why the workspace stays fenced, for waiting reasons.
    reason: String,
}

/// Measures one host's free resources (default: the fleet inventory).
pub type MeasureHook = Arc<dyn Fn(&Host) -> Result<HostLoad> + Send + Sync>;
/// Creates one pool workspace: (host, name, template, size).
pub type CreateHook = Arc<dyn Fn(&str, &str, &str, Option<&str>) -> Result<()> + Send + Sync>;
/// Resolves a profile by name for a provider at admission.
/// Arguments: profile name, provider, the run's kind (when it states one).
pub type ProfileResolver =
    Arc<dyn Fn(&str, &str, Option<&str>) -> Result<ResolvedProfile> + Send + Sync>;
/// Replaces workspace preparation and the guest helper (tests only).
pub type RunExecutor = Arc<dyn Fn(&AdmittedRun) -> Result<Value> + Send + Sync>;
/// Mints a run's repository write token at admission: (`<project>/<name>`,
/// lifetime in seconds).
pub type RepositoryMint = Arc<dyn Fn(&str, u64) -> Result<repository::Credential> + Send + Sync>;
/// Revokes a run's repository token when the run ends (best effort).
pub type RepositoryRevoke = Arc<dyn Fn(&str, &repository::Credential) + Send + Sync>;
/// Probes a workspace's native session lease for a provider: `Ok(true)` when
/// active, `Ok(false)` only on a definite `{"active":false}` (default: the
/// guest helper's `enginebox-guest lease <provider>`).
pub type LeaseProbe = Arc<dyn Fn(&Host, &str, &str) -> Result<bool> + Send + Sync>;
#[derive(Default)]
struct Hooks {
    lease_probe: Mutex<Option<LeaseProbe>>,
    measure: Mutex<Option<MeasureHook>>,
    create: Mutex<Option<CreateHook>>,
    resolver: Mutex<Option<ProfileResolver>>,
    executor: Mutex<Option<RunExecutor>>,
    repository: Mutex<Option<(RepositoryMint, RepositoryRevoke)>>,
}
/// A run the scheduler admitted, as handed to its executor.
#[derive(Debug, Clone)]
pub struct AdmittedRun {
    pub id: String,
    pub host_id: String,
    pub name: String,
    pub pool: Option<String>,
    pub request: native::RunRequest,
    pub profile: Option<String>,
    /// The kind of run the start request stated (`planner`, `builder`...).
    pub run_kind: Option<String>,
    /// Milliseconds between acceptance and admission.
    pub queue_wait_ms: u64,
    /// `<project>/<name>` of the run's hosted repository.
    pub repository: Option<String>,
    /// The repository access minted at admission (set before the executor
    /// runs). `Debug` never prints the token.
    pub repository_access: Option<repository::Access>,
}

/// A profile resolved by the controller for one launch: the provider block and
/// every referenced file's bytes (base64) keyed by digest.
#[derive(Clone)]
pub struct ResolvedProfile {
    pub name: String,
    pub revision: u64,
    pub template: Option<String>,
    pub block: profile::ProviderProfile,
    pub files: BTreeMap<String, String>,
    /// The run kind the profile was resolved for.
    pub run_kind: Option<String>,
    /// The catalog connections merged into `block`: `{name, revision, kind}`
    /// plus `skipped` when a provider cannot use one. Never values.
    pub connections: Vec<Value>,
}
/// Where to POST a run's terminal summary, and the HMAC key that signs it.
#[derive(Clone)]
pub struct Callback {
    pub url: String,
    pub secret: String,
}
impl std::fmt::Debug for Callback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Callback")
            .field("origin", &callback_origin(&self.url))
            .finish()
    }
}
/// Optional v2 launch features. `Default` is exactly the original launch.
#[derive(Clone, Default)]
pub struct LaunchOptions {
    pub profile: Option<ResolvedProfile>,
    pub idempotency_key: Option<String>,
    pub callback: Option<Callback>,
    /// `<project>/<name>` of a hosted repository. The controller checks the
    /// caller's grant; a write token is minted at admission.
    pub repository: Option<String>,
    /// The kind of run (docs/CONNECTIONS.md): selects which of the profile's
    /// connections apply.
    pub run_kind: Option<String>,
}
/// Handed to the finish hook once a run with a callback reaches a terminal state.
#[derive(Debug, Clone)]
pub struct FinishedRun {
    pub id: String,
    pub callback: Callback,
    pub payload: Value,
}
pub type FinishHook = Arc<dyn Fn(FinishedRun) + Send + Sync>;

/// Scheme, host and port only: a callback path or query may carry a token.
pub fn callback_origin(url: &str) -> String {
    url::Url::parse(url)
        .map(|u| u.origin().ascii_serialization())
        .unwrap_or_else(|_| "invalid".into())
}
pub fn valid_idempotency_key(key: &str) -> bool {
    (1..=128).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
}
fn idempotency_key_digest(started_by: &str, key: &str) -> String {
    profile::sha256_hex(format!("{started_by}\n{key}").as_bytes())
}
/// Workspace runs keep the original digest; pool runs scope the key to the
/// pool as well, so one key names one run per principal and pool.
fn scoped_key_digest(started_by: &str, target: &Target, key: &str) -> String {
    match target {
        Target::Workspace { .. } => idempotency_key_digest(started_by, key),
        Target::Pool { pool, .. } => {
            profile::sha256_hex(format!("{started_by}\npool:{pool}\n{key}").as_bytes())
        }
    }
}
/// What a replay must match: the request with env values replaced by names
/// (values are secrets and are never hashed to disk), plus the v2 choices.
fn idempotency_request_digest(request: &native::RunRequest, options: &LaunchOptions) -> String {
    let mut value = json!({
        "provider":request.provider,
        "prompt":request.prompt,
        "resume_id":request.resume_id,
        "workdir":request.workdir,
        "timeout_seconds":request.timeout_seconds,
        "model":request.model,
        "env_keys":request.env_keys(),
        "profile":options.profile.as_ref().map(|p| &p.name),
        "callback":options.callback.as_ref().map(|c| &c.url),
        "repository":options.repository,
    });
    // Added only when set, so digests stored before run kinds still match.
    if let Some(kind) = &options.run_kind {
        value["run_kind"] = json!(kind);
    }
    security::digest(&value)
}
struct Session {
    public: Value,
    control: mpsc::Sender<auth::Control>,
    cancel: Arc<AtomicBool>,
}
struct Pending {
    inner: Arc<Inner>,
}
impl Drop for Pending {
    fn drop(&mut self) {
        self.inner.pending.fetch_sub(1, Ordering::SeqCst);
    }
}
impl AgentService {
    pub fn new(root: &Path, fleet: Fleet) -> Result<Self> {
        Self::with_binaries(root, fleet, BTreeMap::new())
    }
    pub fn with_binaries(
        root: &Path,
        fleet: Fleet,
        provided: BTreeMap<String, String>,
    ) -> Result<Self> {
        Self::with_limits(root, fleet, provided, QueueLimits::from_env()?)
    }
    pub fn with_limits(
        root: &Path,
        fleet: Fleet,
        provided: BTreeMap<String, String>,
        limits: QueueLimits,
    ) -> Result<Self> {
        let controller_root = if root.is_absolute() {
            root.to_path_buf()
        } else {
            std::env::current_dir()?.join(root)
        };
        security::private_dir(&controller_root)?;
        let runs_root = controller_root.join("agent-runs");
        security::private_dir(&runs_root)?;
        let details_root = controller_root.join("agent-run-details");
        security::private_dir(&details_root)?;
        let queue_root = controller_root.join("agent-run-queue");
        security::private_dir(&queue_root)?;
        let pools = pools::Pools::open(&controller_root)?;
        let launch_events = LaunchEvents::open(&controller_root)?;
        let mut entries: BTreeMap<String, Entry> = BTreeMap::new();
        for item in std::fs::read_dir(&queue_root)? {
            let filename = item?.file_name().to_string_lossy().into_owned();
            let Some(id) = filename.strip_suffix(".json") else {
                continue;
            };
            native::validate_run_id(id)?;
            let raw = security::read_bytes(&queue_root, &filename, QUEUE_ENTRY_LIMIT, true)?
                .ok_or_else(|| anyhow::anyhow!("Queue entry vanished."))?;
            let entry: Entry = serde_json::from_slice(&raw)?;
            ensure!(entry.id == id, "Invalid queue entry.");
            entry.validate()?;
            entries.insert(id.to_owned(), entry);
        }
        let root = controller_root.join("agents");
        security::private_dir(&root)?;
        for dir in ["homes", "attachments"] {
            security::private_dir(&root.join(dir))?;
        }
        let mut binaries = BTreeMap::new();
        for p in crate::PROVIDERS {
            let home = root.join("homes").join(p);
            security::private_dir(&home)?;
            security::private_dir(&home.join(".codex"))?;
            security::private_dir(&home.join(".claude"))?;
            let env = BTreeMap::from([(
                "PATH".into(),
                format!(
                    "{}:/usr/local/bin:/usr/bin:/bin",
                    root.join("bin").display()
                ),
            )]);
            let path = provided
                .get(p)
                .cloned()
                .or_else(|| native::find_binary(native::binary(p), &env));
            if let Some(path) = &path {
                ensure!(
                    Path::new(path).is_absolute() && !path.chars().any(char::is_control),
                    "Native binary path must be absolute."
                );
            }
            binaries.insert(p.into(), path);
        }
        let mut records: BTreeMap<String, Value> = BTreeMap::new();
        let record_pattern = regex::Regex::new(r"^[a-f0-9]{32}\.json$")?;
        for item in std::fs::read_dir(&runs_root)? {
            let item = item?;
            let filename = item.file_name().to_string_lossy().into_owned();
            if !filename.ends_with(".json") {
                continue;
            }
            ensure!(
                record_pattern.is_match(&filename),
                "Invalid native run record name."
            );
            let raw =
                security::read_bytes(&runs_root, &filename, native::MAX_OUTPUT * 7, true)?.unwrap();
            let mut v: Value = serde_json::from_slice(&raw)?;
            let id = filename.trim_end_matches(".json");
            validate_record(&v, id)?;
            let still_queued = v["status"] == "queued"
                && entries.get(id).is_some_and(|e| {
                    e.state == EntryState::Queued && matches!(e.target, Target::Workspace { .. })
                });
            if !still_queued && ["running", "queued"].contains(&v["status"].as_str().unwrap_or(""))
            {
                v["status"] = json!("failed");
                v["execution_uncertain"] = json!(true);
                v["error"] = json!(
                    "Controller restarted; its native session lease must be checked before another run or lifecycle change."
                );
                security::write_bytes(
                    &runs_root,
                    &filename,
                    &serde_json::to_vec(&v)?,
                    native::MAX_OUTPUT * 7,
                )?;
            }
            records.insert(id.into(), v);
        }
        // Reconcile entries with records: a queued workspace entry needs its
        // queued record; an admitted entry's run was interrupted and may still
        // owe its callback; queued pool entries and finished ones stand alone.
        let mut owed = Vec::new();
        let mut callbacks = BTreeMap::new();
        entries.retain(|id, e| {
            let keep = match (e.state, &e.target) {
                (EntryState::Queued, Target::Workspace { .. }) => {
                    records.get(id).is_some_and(|r| r["status"] == "queued")
                }
                (EntryState::Queued, Target::Pool { .. }) => !records.contains_key(id),
                (EntryState::Admitted, _) => {
                    if records.contains_key(id) && e.callback.is_some() {
                        owed.push(id.clone());
                        true
                    } else {
                        false
                    }
                }
                (EntryState::Finished, _) => true,
            };
            if !keep {
                let _ = std::fs::remove_file(queue_root.join(format!("{id}.json")));
            } else if let Some(c) = &e.callback
                && e.state != EntryState::Finished
            {
                callbacks.insert(
                    id.clone(),
                    Callback {
                        url: c.url.clone(),
                        secret: c.secret.clone(),
                    },
                );
            }
            keep
        });
        let mut details = BTreeMap::new();
        for id in records.keys().chain(entries.keys()) {
            if let Some(raw) =
                security::read_bytes(&details_root, &format!("{id}.json"), security::LIMIT, true)?
            {
                let detail: Value = serde_json::from_slice(&raw)?;
                run_details::validate(&detail)?;
                details.insert(id.clone(), detail);
            }
        }
        Ok(Self {
            inner: Arc::new(Inner {
                root,
                runs_root,
                details_root,
                details: Mutex::new(details),
                fleet,
                binaries,
                sessions: Mutex::new(BTreeMap::new()),
                records: Mutex::new(records),
                leases: Mutex::new(BTreeSet::new()),
                pending: AtomicUsize::new(0),
                closed: AtomicBool::new(false),
                usage: usage::Cache::default(),
                status_cache: Mutex::new(BTreeMap::new()),
                receipt_lock: Mutex::new(()),
                callbacks: Mutex::new(callbacks),
                finish_hook: Mutex::new(None),
                queue_root,
                limits: Mutex::new(limits),
                pools,
                sched: Mutex::new(Sched {
                    next_seq: entries.values().map(|e| e.seq + 1).max().unwrap_or(1),
                    entries: entries
                        .into_iter()
                        .filter(|(_, e)| e.state != EntryState::Admitted)
                        .map(|(id, entry)| {
                            (
                                id,
                                Queued {
                                    entry,
                                    resolved: None,
                                },
                            )
                        })
                        .collect(),
                    dirty: true,
                    ..Default::default()
                }),
                wake: Condvar::new(),
                admitting: AtomicBool::new(true),
                dispatcher: AtomicBool::new(false),
                measures: Mutex::new(BTreeMap::new()),
                probes: Mutex::new(BTreeMap::new()),
                guests: Mutex::new(BTreeMap::new()),
                hooks: Hooks::default(),
                owed_callbacks: Mutex::new(owed),
                launch_events,
            }),
        })
    }
    fn pending_guard(&self) -> Result<Pending> {
        ensure!(
            !self.inner.closed.load(Ordering::SeqCst),
            "Agent service is draining."
        );
        self.inner.pending.fetch_add(1, Ordering::SeqCst);
        Ok(Pending {
            inner: self.inner.clone(),
        })
    }
    pub fn pending(&self) -> usize {
        self.inner.pending.load(Ordering::SeqCst)
    }
    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::SeqCst);
        for session in self.inner.sessions.lock().unwrap().values() {
            session.cancel.store(true, Ordering::SeqCst);
            let _ = session.control.send(auth::Control::Cancel);
        }
    }
    fn home(&self, p: &str) -> Result<PathBuf> {
        crate::provider(p)?;
        let home = self.inner.root.join("homes").join(p);
        security::private_dir(&home)?;
        Ok(home)
    }
    fn provider_lock(&self, p: &str) -> Result<security::Lease> {
        security::Lease::acquire(
            &self.home(p)?,
            &format!(".local/state/enginebox/auth-{p}.lock"),
        )
    }
    fn material(&self, p: &str) -> Result<Value> {
        let home = self.home(p)?;
        ensure!(
            !home.join("pending-adoption.json").exists(),
            "Finish pending controller adoption first."
        );
        if let Some(v) = security::read_json(&home, delegation::TOKEN_FILE)?
            && security::token(&v["value"])
            && ["claude", "cursor"].contains(&p)
        {
            let key = if p == "claude" {
                "CLAUDE_CODE_OAUTH_TOKEN"
            } else {
                "CURSOR_API_KEY"
            };
            return Ok(
                json!({"kind":"environment","content":{key:v["value"]},"expires_at":v["expires_at"]}),
            );
        }
        delegation::material(
            p,
            &security::read_json(&home, delegation::native_file(p))?.unwrap_or(Value::Null),
        )
    }
    fn credential_digest(&self, p: &str) -> Result<String> {
        let home = self.home(p)?;
        let v = security::read_json(&home, delegation::TOKEN_FILE)?
            .or(security::read_json(&home, delegation::native_file(p))?)
            .unwrap_or(Value::Null);
        Ok(security::legacy_digest(&v))
    }
    fn refresh_blocked(&self, p: &str) -> Result<bool> {
        Ok(
            security::read_json(&self.home(p)?, "refresh-failure.json")?.is_some_and(|v| {
                v["credential_digest"] == self.credential_digest(p).unwrap_or_default()
            }),
        )
    }
    fn record_refresh_failure(&self, p: &str) -> Result<()> {
        security::write_json(
            &self.home(p)?,
            "refresh-failure.json",
            &json!({"credential_digest":self.credential_digest(p)?,"failed_at":chrono::Utc::now().timestamp()}),
        )
    }
    pub fn status(&self) -> Result<Value> {
        let _pending = self.pending_guard()?;
        let providers = thread::scope(|scope| {
            crate::PROVIDERS
                .into_iter()
                .map(|p| scope.spawn(move || self.provider_status(p)))
                .collect::<Vec<_>>()
                .into_iter()
                .map(|j| {
                    j.join()
                        .unwrap_or_else(|_| auth::provider_status("unknown", false))
                })
                .collect::<Vec<_>>()
        });
        Ok(json!({"providers":providers,"scope":"controller","credential_storage":"private_files"}))
    }
    /// Compatibility observation of the assigned guest's real native login state.
    /// Only normalized fields cross the guest/controller boundary.
    pub fn workspace_status(&self, host_id: &str, name: &str) -> Result<Value> {
        let _pending = self.pending_guard()?;
        let host = self.inner.fleet.account_workspace(host_id, name)?;
        let command = guest_command(&host, name, &["status".into(), "all".into()])?;
        let result = process::checked(&command, None, None, b"", 25, 65536)
            .and_then(|raw| Ok(serde_json::from_str::<Value>(&raw)?));
        let supplied = result.ok().filter(|value| value["type"] == "statuses");
        let providers = crate::PROVIDERS
            .iter()
            .map(|provider| {
                let mut safe = auth::provider_status(provider, false);
                if let Some(value) = supplied
                    .as_ref()
                    .and_then(|s| s["providers"].as_array())
                    .and_then(|list| list.iter().find(|p| p["id"] == *provider))
                    && value["installed"].is_boolean()
                    && ["connected", "disconnected", "unknown", "unavailable"]
                        .contains(&value["status"].as_str().unwrap_or(""))
                    && ["subscription", "account", "api_key", "unknown", "none"]
                        .contains(&value["auth_method"].as_str().unwrap_or(""))
                {
                    for field in ["installed", "status", "auth_method"] {
                        safe[field] = value[field].clone();
                    }
                }
                safe
            })
            .collect::<Vec<_>>();
        Ok(json!({"providers":providers}))
    }
    fn provider_status(&self, p: &str) -> Value {
        let cached = self.inner.status_cache.lock().unwrap().get(p).cloned();
        let fresh = cached
            .as_ref()
            .is_some_and(|(time, _)| time.elapsed() < std::time::Duration::from_secs(2));
        let mut result = cached
            .map(|(_, v)| v)
            .unwrap_or_else(|| auth::provider_status(p, self.inner.binaries[p].is_some()));
        if !fresh
            && let Ok(lease) = self.provider_lock(p)
            && let Ok(home) = self.home(p)
            && let Ok(mut env) = auth::controller_environment(&home, &self.inner.binaries)
        {
            let mut saved_token = false;
            if let Ok(Some(saved)) = security::read_json(&home, delegation::TOKEN_FILE)
                && let Some(token) = saved["value"].as_str()
            {
                saved_token = true;
                env.insert(
                    if p == "claude" {
                        "CLAUDE_CODE_OAUTH_TOKEN"
                    } else {
                        "CURSOR_API_KEY"
                    }
                    .into(),
                    token.into(),
                );
            }
            result =
                auth::probe_with_lease(p, self.inner.binaries[p].as_deref(), &env, Some(&lease));
            if saved_token && result["status"] == "connected" {
                result["auth_method"] = json!(if p == "claude" {
                    "subscription"
                } else {
                    "api_key"
                });
            }
            self.inner
                .status_cache
                .lock()
                .unwrap()
                .insert(p.into(), (std::time::Instant::now(), result.clone()));
        }
        result["scope"] = json!("controller");
        result["delegation_status"] = json!("unknown");
        match self.material(p) {
            Ok(m) => {
                let expiry = m["expires_at"].as_f64();
                if let Some(expiry) = expiry {
                    result["expires_at"] = json!(usage::iso(expiry));
                }
                result["delegation_status"] = json!(if self.refresh_blocked(p).unwrap_or(true)
                    || expiry.is_some_and(|e| e <= chrono::Utc::now().timestamp() as f64)
                {
                    "reauth_required"
                } else {
                    "ready"
                });
                if ["codex", "cursor"].contains(&p) && m["kind"] == "native" {
                    result["detail"] = json!(
                        "Access-only delegation uses the tested native client format; refresh credentials remain on the controller."
                    );
                }
            }
            Err(_) => {
                result["delegation_status"] = json!(if result["status"] == "disconnected" {
                    "reauth_required"
                } else {
                    "unsupported"
                })
            }
        }
        let sessions = self.inner.sessions.lock().unwrap();
        if let Some(s) = sessions
            .values()
            .filter(|s| s.public["provider"] == p)
            .max_by_key(|s| s.public["expires_at"].as_str().unwrap_or(""))
        {
            result["session"] = s.public.clone();
        }
        result
    }
    pub fn usage(&self, p: &str) -> Result<Value> {
        crate::provider(p)?;
        let _pending = self.pending_guard()?;
        if p != "codex" {
            return Ok(usage::unavailable(p, None));
        }
        let result = (|| -> Result<Value> {
            let lease = self.provider_lock(p)?;
            let home = self.home(p)?;
            let native =
                security::read_json(&home, delegation::native_file(p))?.unwrap_or(Value::Null);
            if !native["OPENAI_API_KEY"].is_null() || !native["tokens"].is_object() {
                return Ok(usage::unavailable(p, None));
            }
            let Some(exe) = &self.inner.binaries[p] else {
                return Ok(usage::unavailable(
                    p,
                    Some("Install the native Codex client to read account allowance."),
                ));
            };
            let env = auth::controller_environment(&home, &self.inner.binaries)?;
            Ok(self.inner.usage.read(p, &self.credential_digest(p)?, || {
                usage::read_codex_with_lease(exe, &env, Some(&lease))
            }))
        })();
        Ok(result.unwrap_or_else(|_| {
            usage::unavailable(
                p,
                Some("Usage is temporarily unavailable. Check the connection or retry shortly."),
            )
        }))
    }
    pub fn start(&self, p: &str) -> Result<Value> {
        crate::provider(p)?;
        let home = self.home(p)?;
        ensure!(
            !home.join("pending-adoption.json").exists(),
            "Finish pending controller adoption first."
        );
        let mut sessions = self.inner.sessions.lock().unwrap();
        if let Some(s) = sessions
            .values()
            .find(|s| s.public["provider"] == p && active_session(&s.public))
        {
            return Ok(s.public.clone());
        }
        while sessions.len() >= 16 {
            let key = sessions
                .iter()
                .filter(|(_, s)| !active_session(&s.public))
                .min_by_key(|(_, s)| s.public["expires_at"].as_str().unwrap_or(""))
                .map(|(k, _)| k.clone())
                .ok_or_else(|| anyhow::anyhow!("Too many native connections are active."))?;
            sessions.remove(&key);
        }
        let exe = self.inner.binaries[p]
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Install the native client on the controller."))?;
        let pending = self.pending_guard()?;
        let id = uuid::Uuid::new_v4().simple().to_string();
        let public = json!({"id":id,"provider":p,"scope":"controller","status":"starting","input_required":false,"message":"Starting native sign-in on the controller.","expires_at":(chrono::Utc::now()+chrono::Duration::seconds(600)).to_rfc3339()});
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        sessions.insert(
            id.clone(),
            Session {
                public: public.clone(),
                control: tx,
                cancel: cancel.clone(),
            },
        );
        let this = self.clone();
        let provider = p.to_owned();
        thread::spawn(move || {
            let _pending = pending;
            let result = auth::login(
                &home,
                &provider,
                &exe,
                &this.inner.binaries,
                rx,
                &cancel,
                |event| this.login_event(&id, event),
            );
            if result.is_err() {
                this.login_event(&id, json!({"type":"result","reason":"failed"}));
            }
        });
        Ok(public)
    }
    fn login_event(&self, id: &str, event: Value) {
        let mut sessions = self.inner.sessions.lock().unwrap();
        let Some(s) = sessions.get_mut(id) else {
            return;
        };
        match event["type"].as_str() {
            Some("started") => {
                s.public["status"] = json!("waiting");
                s.public["message"] = json!("Complete sign-in with the native provider.");
            }
            Some("authorization") => {
                s.public["authorization_url"] = event["authorization_url"].clone()
            }
            Some("code") => s.public["user_code"] = event["user_code"].clone(),
            Some("input_required") => s.public["input_required"] = json!(true),
            Some("input_accepted") => s.public["input_required"] = json!(false),
            Some("result") => {
                if let Some(provider) = s.public["provider"].as_str() {
                    self.inner.status_cache.lock().unwrap().remove(provider);
                }
                let state = match event["reason"].as_str() {
                    Some("completed") => "succeeded",
                    Some("cancelled") => "cancelled",
                    Some("expired") => "expired",
                    _ => "failed",
                };
                s.public["status"] = json!(state);
                s.public["input_required"] = json!(false);
                s.public["message"] = json!(match state {
                    "succeeded" => "Native account connected on the controller.",
                    "cancelled" => "Sign-in cancelled.",
                    "expired" => "Sign-in expired; start again.",
                    _ => "Native sign-in could not be verified; retry with the installed client.",
                });
            }
            _ => {}
        }
    }
    pub fn session(&self, p: &str, id: &str) -> Result<Value> {
        crate::provider(p)?;
        let sessions = self.inner.sessions.lock().unwrap();
        let s = sessions
            .get(id)
            .filter(|s| s.public["provider"] == p)
            .ok_or_else(|| anyhow::anyhow!("Unknown native connection session."))?;
        Ok(s.public.clone())
    }
    pub fn input(&self, p: &str, id: &str, text: &str) -> Result<Value> {
        ensure!(
            p == "claude" && auth::valid_input(text),
            "Only a bounded Claude authorization code is accepted."
        );
        let mut sessions = self.inner.sessions.lock().unwrap();
        let s = sessions
            .get_mut(id)
            .filter(|s| s.public["provider"] == p)
            .ok_or_else(|| anyhow::anyhow!("Unknown native connection session."))?;
        ensure!(
            s.public["status"] == "waiting" && s.public["input_required"] == true,
            "Sign-in is not waiting for an authorization code."
        );
        s.control.send(auth::Control::Input(text.into()))?;
        s.public["input_required"] = json!(false);
        s.public["message"] = json!("Checking the authorization code with the native client.");
        Ok(s.public.clone())
    }
    pub fn cancel(&self, p: &str, id: &str) -> Result<Value> {
        let mut sessions = self.inner.sessions.lock().unwrap();
        let s = sessions
            .get_mut(id)
            .filter(|s| s.public["provider"] == p)
            .ok_or_else(|| anyhow::anyhow!("Unknown native connection session."))?;
        if active_session(&s.public) {
            s.cancel.store(true, Ordering::SeqCst);
            let _ = s.control.send(auth::Control::Cancel);
            s.public["status"] = json!("cancelled");
            s.public["input_required"] = json!(false);
            s.public["message"] = json!("Sign-in cancelled.");
        }
        Ok(s.public.clone())
    }
    pub fn connect_key(&self, p: &str, text: &str) -> Result<Value> {
        ensure!(
            p == "cursor" && security::token(&json!(text)),
            "Only a bounded native Cursor API key is accepted here."
        );
        let _pending = self.pending_guard()?;
        let _lease = self.provider_lock(p)?;
        security::write_json(
            &self.home(p)?,
            delegation::TOKEN_FILE,
            &json!({"kind":"personal_api_key","value":text,"expires_at":null}),
        )?;
        self.inner.status_cache.lock().unwrap().remove(p);
        Ok(json!({"provider":p,"scope":"controller","status":"saved"}))
    }
    fn refresh(&self, p: &str, lease: &security::Lease) -> Result<()> {
        let home = self.home(p)?;
        let native = security::read_json(&home, delegation::native_file(p))?
            .ok_or_else(|| anyhow::anyhow!("Reconnect provider to renew access."))?;
        let mut env = auth::controller_environment(&home, &self.inner.binaries)?;
        let exe = self.inner.binaries[p]
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Install native client to refresh connection."))?;
        match p {
            "codex" => {
                process::native_rpc_with_lease(
                    &[
                        exe.clone(),
                        "-c".into(),
                        "cli_auth_credentials_store=\"file\"".into(),
                        "app-server".into(),
                    ],
                    &env,
                    "account/read",
                    Some(json!({"refreshToken":true})),
                    20,
                    65536,
                    Some(lease),
                )?;
            }
            "claude" => {
                let oauth = &native["claudeAiOauth"];
                ensure!(
                    security::token(&oauth["refreshToken"]),
                    "Reconnect provider to renew access."
                );
                let scopes = oauth["scopes"]
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("Unsupported native scopes."))?;
                ensure!(
                    !scopes.is_empty()
                        && scopes
                            .iter()
                            .all(|s| s.as_str().is_some_and(|s| regex::Regex::new(
                                r"^[a-zA-Z0-9_:.-]{1,128}$"
                            )
                            .unwrap()
                            .is_match(s))),
                    "Unsupported native scopes."
                );
                env.insert(
                    "CLAUDE_CODE_OAUTH_REFRESH_TOKEN".into(),
                    oauth["refreshToken"].as_str().unwrap().into(),
                );
                env.insert(
                    "CLAUDE_CODE_OAUTH_SCOPES".into(),
                    scopes
                        .iter()
                        .map(|s| s.as_str().unwrap())
                        .collect::<Vec<_>>()
                        .join(" "),
                );
                process::checked_with_lease(
                    &[
                        exe.clone(),
                        "auth".into(),
                        "login".into(),
                        "--claudeai".into(),
                    ],
                    Some(&env),
                    Some(&home),
                    b"",
                    20,
                    65536,
                    Some(lease),
                )?;
            }
            _ => {
                process::checked_with_lease(
                    &[
                        exe.clone(),
                        "status".into(),
                        "--format".into(),
                        "json".into(),
                    ],
                    Some(&env),
                    Some(&home),
                    b"",
                    15,
                    65536,
                    Some(lease),
                )?;
            }
        }
        Ok(())
    }
    fn transfer(&self, host: &Host, name: &str, request: &Value) -> Result<Value> {
        let command = guest_command(host, name, &["transfer".into()])?;
        let out = process::checked(
            &command,
            None,
            None,
            &serde_json::to_vec(request)?,
            30,
            65536 * 3,
        )?;
        let value: Value = serde_json::from_str(&out)?;
        ensure!(value["ok"] == true, "Native credential transfer failed.");
        Ok(value)
    }
    pub fn attach_after_start(&self, host_id: &str, name: &str) -> Result<Value> {
        let _pending = self.pending_guard()?;
        self.attach_locked(host_id, name, None, None, None)
    }
    pub fn attach(
        &self,
        host_id: &str,
        name: &str,
        providers: Option<Vec<String>>,
    ) -> Result<Value> {
        let _pending = self.pending_guard()?;
        self.inner.fleet.assert_admission(host_id)?;
        let host = self.inner.fleet.account_workspace(host_id, name)?;
        let _hostlock = enginebox_core::operation_lock(&host)?;
        self.attach_locked(host_id, name, providers, None, None)
    }
    fn attach_locked(
        &self,
        host_id: &str,
        name: &str,
        providers: Option<Vec<String>>,
        expected_identity: Option<&str>,
        expected_binding: Option<&str>,
    ) -> Result<Value> {
        self.inner.fleet.assert_admission(host_id)?;
        let host = self.inner.fleet.account_workspace(host_id, name)?;
        let binding = host_binding(&host)?;
        let key = receipt_key(host_id, name);
        let prior =
            security::read_json(&self.inner.root.join("attachments"), &format!("{key}.json"))?
                .unwrap_or(json!({}));
        let expected_binding = expected_binding.or(prior["host_binding"].as_str());
        ensure!(
            expected_binding.is_none_or(|b| b == binding),
            "Registered workspace host changed; attach it explicitly."
        );
        let expected_identity = expected_identity.or(prior["instance_id"].as_str());
        let selected =
            providers.unwrap_or_else(|| crate::PROVIDERS.iter().map(|p| p.to_string()).collect());
        ensure!(
            !selected.is_empty()
                && selected.len() <= 3
                && selected.iter().collect::<BTreeSet<_>>().len() == selected.len(),
            "Select distinct native providers."
        );
        for p in &selected {
            crate::provider(p)?;
        }
        let mut results = Vec::new();
        for p in selected {
            let mut result = json!({"provider":p,"status":"unavailable"});
            let outcome = (|| -> Result<()> {
                let lease = self.provider_lock(&p)?;
                let mut material = self.material(&p)?;
                let mut expiry = material["expires_at"].as_f64();
                let now = chrono::Utc::now().timestamp() as f64;
                if expiry.is_some_and(|e| e <= now + 120.) {
                    result["status"] = json!("reauth_required");
                    ensure!(
                        !self.refresh_blocked(&p)?,
                        "Reconnect provider after failed refresh."
                    );
                    if self.refresh(&p, &lease).is_err() {
                        self.record_refresh_failure(&p)?;
                        bail!("Native refresh failed.")
                    }
                    material = self.material(&p)?;
                    expiry = material["expires_at"].as_f64();
                }
                if expiry.is_some_and(|e| e <= now + 30.) {
                    self.record_refresh_failure(&p)?;
                    bail!("Reconnect provider before delegating.")
                }
                let mut request = delegation::attachment(&p, &material, false);
                request["expected_instance_id"] = json!(expected_identity);
                self.inner.fleet.assert_admission(host_id)?;
                let delivered = self.transfer(&host, name, &request)?;
                result["status"] = json!(if delivered["unchanged"] == true {
                    "unchanged"
                } else {
                    "attached"
                });
                result["generation"] = request["generation"].clone();
                result["refresh_credentials"] = json!(false);
                if let Some(expiry) = expiry {
                    result["expires_at"] = json!(usage::iso(expiry));
                }
                let id = delivered["instance_id"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("Workspace identity not returned."))?;
                self.register_attachment(host_id, name, &p, id, &binding)?;
                Ok(())
            })();
            if outcome.is_err() {
                result["detail"] =
                    json!("This provider could not be delegated; check its controller connection.");
            }
            results.push(result);
        }
        Ok(json!({"host_id":host_id,"name":name,"providers":results}))
    }
    fn register_attachment(
        &self,
        host: &str,
        name: &str,
        p: &str,
        id: &str,
        binding: &str,
    ) -> Result<()> {
        ensure!(
            regex::Regex::new(r"^[0-9a-f-]{32,36}$")?.is_match(id),
            "Invalid workspace identity."
        );
        let _lock = self.inner.receipt_lock.lock().unwrap();
        let relative = format!("{}.json", receipt_key(host, name));
        let root = self.inner.root.join("attachments");
        let prior = security::read_json(&root, &relative)?.unwrap_or(json!({}));
        let mut providers: BTreeSet<String> =
            if prior["instance_id"] == id && prior["host_binding"] == binding {
                prior["providers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            } else {
                BTreeSet::new()
            };
        providers.insert(p.into());
        security::write_json(
            &root,
            &relative,
            &json!({"host_id":host,"name":name,"instance_id":id,"host_binding":binding,"providers":providers}),
        )
    }
    pub fn adopt(&self, host_id: &str, name: &str, p: &str) -> Result<Value> {
        crate::provider(p)?;
        let _pending = self.pending_guard()?;
        self.inner.fleet.assert_admission(host_id)?;
        let host = self.inner.fleet.account_workspace(host_id, name)?;
        let _hostlock = enginebox_core::operation_lock(&host)?;
        let _lease = self.provider_lock(p)?;
        self.inner.fleet.assert_admission(host_id)?;
        let home = self.home(p)?;
        let binding = host_binding(&host)?;
        let pending = security::read_json(&home, "pending-adoption.json")?;
        let (native, id) = if let Some(pending) = pending {
            ensure!(
                pending["host_binding"] == binding && pending["name"] == name,
                "Finish adoption from its original workspace first."
            );
            (
                pending["native"].clone(),
                pending["instance_id"].as_str().unwrap_or("").to_owned(),
            )
        } else {
            let exported =
                self.transfer(&host, name, &json!({"operation":"export","provider":p}))?;
            let native = exported["native"].clone();
            delegation::material(p, &native)?;
            let id = exported["instance_id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Workspace identity unavailable."))?
                .to_owned();
            security::write_json(
                &home,
                "pending-adoption.json",
                &json!({"host_binding":binding,"name":name,"instance_id":id,"native":native}),
            )?;
            (native, id)
        };
        let mut request = delegation::attachment(p, &delegation::material(p, &native)?, true);
        request["expected_instance_id"] = json!(id);
        self.inner.fleet.assert_admission(host_id)?;
        self.transfer(&host, name, &request)?;
        security::write_json(&home, delegation::native_file(p), &native)?;
        security::remove(&home, delegation::TOKEN_FILE)?;
        security::remove(&home, "pending-adoption.json")?;
        self.register_attachment(host_id, name, p, &id, &binding)?;
        Ok(json!({"provider":p,"scope":"controller","status":"adopted"}))
    }
    pub fn reconcile(&self) -> Result<Value> {
        let _pending = self.pending_guard()?;
        let mut results = Vec::new();
        let mut paths = std::fs::read_dir(self.inner.root.join("attachments"))?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|s| s.ends_with(".json"))
            .collect::<Vec<_>>();
        paths.sort();
        for file in paths.into_iter().take(256) {
            let result = (|| -> Result<Value> {
                let v = security::read_json(&self.inner.root.join("attachments"), &file)?.unwrap();
                let host_id = v["host_id"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("Invalid receipt."))?;
                let name = v["name"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("Invalid receipt."))?;
                let host = self.inner.fleet.account_workspace(host_id, name)?;
                let _hostlock = enginebox_core::operation_lock(&host)?;
                self.attach_locked(
                    host_id,
                    name,
                    Some(serde_json::from_value(v["providers"].clone())?),
                    v["instance_id"].as_str(),
                    v["host_binding"].as_str(),
                )
            })();
            if let Ok(v) = result {
                results.push(v)
            }
        }
        Ok(json!({"workspaces":results}))
    }
    pub fn active(&self, host_id: &str, name: &str) -> bool {
        if self
            .inner
            .leases
            .lock()
            .unwrap()
            .iter()
            .any(|(h, n, _)| h == host_id && n == name)
        {
            return true;
        }
        let uncertain = self
            .inner
            .records
            .lock()
            .unwrap()
            .values()
            .filter(|v| {
                v["execution_uncertain"] == true && v["host_id"] == host_id && v["name"] == name
            })
            .cloned()
            .collect::<Vec<_>>();
        for value in uncertain {
            let result = self.probe_lease(host_id, name, value["provider"].as_str().unwrap_or(""));
            if result.unwrap_or(true) {
                return true;
            }
            if !self.clear_uncertain(value["id"].as_str().unwrap_or("")) {
                return true;
            }
        }
        self.inner
            .probes
            .lock()
            .unwrap()
            .remove(&(host_id.to_owned(), name.to_owned()));
        false
    }
    /// One bounded guest lease probe. Only a definite `{"active":false}`
    /// yields `Ok(false)`; anything else is active or an error.
    fn probe_lease(&self, host_id: &str, name: &str, provider: &str) -> Result<bool> {
        let host = self.inner.fleet.host(host_id)?;
        let hook = self.inner.hooks.lease_probe.lock().unwrap().clone();
        if let Some(probe) = hook {
            return probe(&host, name, provider);
        }
        let command = guest_command(&host, name, &["lease".into(), provider.into()])?;
        let output = process::checked(&command, None, None, b"", 15, 1024)?;
        let v: Value = serde_json::from_str(&output)?;
        v["active"]
            .as_bool()
            .ok_or_else(|| anyhow::anyhow!("Invalid lease probe."))
    }
    /// Clear one record's `execution_uncertain` after a definite inactive
    /// lease. False when the record could not be saved (the fence stays).
    fn clear_uncertain(&self, id: &str) -> bool {
        let mut records = self.inner.records.lock().unwrap();
        let Some(v) = records.get_mut(id) else {
            return true;
        };
        if v["execution_uncertain"] != true {
            return true;
        }
        v["execution_uncertain"] = json!(false);
        if self.save_record(v).is_err() {
            v["execution_uncertain"] = json!(true);
            return false;
        }
        true
    }
    /// Probe, in the background and at most once per
    /// `uncertain_probe_seconds` per workspace, each workspace that an
    /// uncertain run fences, and clear the fence on a definite inactive
    /// lease. Probe failures and active leases keep the fence. Returns why
    /// each fenced workspace still waits.
    fn probe_uncertain(&self) -> BTreeMap<(String, String), String> {
        let mut fenced: BTreeMap<(String, String), Vec<(String, String)>> = BTreeMap::new();
        for v in self.inner.records.lock().unwrap().values() {
            if v["execution_uncertain"] == true {
                fenced
                    .entry((
                        v["host_id"].as_str().unwrap_or("").to_owned(),
                        v["name"].as_str().unwrap_or("").to_owned(),
                    ))
                    .or_default()
                    .push((
                        v["id"].as_str().unwrap_or("").to_owned(),
                        v["provider"].as_str().unwrap_or("").to_owned(),
                    ));
            }
        }
        let held: BTreeSet<(String, String)> = self
            .inner
            .leases
            .lock()
            .unwrap()
            .iter()
            .map(|(h, n, _)| (h.clone(), n.clone()))
            .collect();
        let interval = Duration::from_secs(self.limits().uncertain_probe_seconds);
        let mut reasons = BTreeMap::new();
        let mut due = Vec::new();
        {
            let mut probes = self.inner.probes.lock().unwrap();
            probes.retain(|k, _| fenced.contains_key(k));
            for (key, runs) in fenced {
                let probe = probes.entry(key.clone()).or_insert(Probe {
                    last: None,
                    in_flight: false,
                    reason: "execution uncertain; checking its native session lease".into(),
                });
                // A workspace running a new run is busy anyway; probe later.
                if !probe.in_flight
                    && !held.contains(&key)
                    && probe.last.is_none_or(|at| at.elapsed() >= interval)
                {
                    probe.in_flight = true;
                    due.push((key.clone(), runs, probe.reason.clone()));
                }
                reasons.insert(key, probe.reason.clone());
            }
        }
        for ((host_id, name), runs, previous) in due {
            let this = self.clone();
            thread::spawn(move || {
                let key = (host_id.clone(), name.clone());
                let mut reason = None;
                for (id, provider) in runs {
                    let started = Instant::now();
                    let result = this.probe_lease(&host_id, &name, &provider);
                    let (outcome, why) = match &result {
                        Ok(false) if this.clear_uncertain(&id) => ("cleared", None),
                        Ok(false) => (
                            "failed",
                            Some("execution uncertain; its record could not be saved".to_owned()),
                        ),
                        Ok(true) => (
                            "blocked",
                            Some(
                                "execution uncertain; the native session lease is still active"
                                    .to_owned(),
                            ),
                        ),
                        Err(e) => (
                            "failed",
                            Some(format!(
                                "execution uncertain; lease probe failed: {}",
                                launch_events::clip_message(&format!("{e:#}"), &BTreeMap::new())
                            )),
                        ),
                    };
                    // One event per change, not one per probe interval.
                    if why.as_ref() == Some(&previous) {
                        reason = why;
                        break;
                    }
                    this.record_launch(LaunchEvent {
                        kind: "uncertain_lease_probe".into(),
                        outcome: if outcome == "cleared" {
                            "succeeded"
                        } else {
                            "failed"
                        }
                        .into(),
                        run_id: Some(id.clone()),
                        pool: this
                            .inner
                            .pools
                            .all()
                            .into_iter()
                            .find(|p| {
                                p.members()
                                    .iter()
                                    .any(|m| m.host_id == host_id && m.name == name)
                            })
                            .map(|p| p.name),
                        host_id: Some(host_id.clone()),
                        workspace: Some(name.clone()),
                        provider: Some(provider.clone()),
                        stage: Stage::Place.as_str().into(),
                        reason: match outcome {
                            "cleared" => "lease_inactive",
                            "blocked" => "lease_active",
                            _ => "lease_probe_failed",
                        }
                        .into(),
                        message: why.clone().unwrap_or_else(|| {
                            "Native session lease is inactive; the workspace is released.".into()
                        }),
                        duration_ms: started.elapsed().as_millis() as u64,
                        ..Default::default()
                    });
                    if why.is_some() {
                        reason = why;
                        break;
                    }
                }
                {
                    let mut probes = this.inner.probes.lock().unwrap();
                    match reason {
                        Some(reason) => {
                            if let Some(p) = probes.get_mut(&key) {
                                p.in_flight = false;
                                p.last = Some(Instant::now());
                                p.reason = reason;
                            }
                        }
                        None => {
                            probes.remove(&key);
                        }
                    }
                }
                this.kick();
            });
        }
        reasons
    }
    pub fn get_run(&self, host_id: &str, name: &str, id: &str) -> Result<Value> {
        ensure!(
            regex::Regex::new(r"^[a-f0-9]{32}$")?.is_match(id),
            "Invalid native run ID."
        );
        let waiting = self.waiting_for(id);
        let records = self.inner.records.lock().unwrap();
        let record = records
            .get(id)
            .filter(|v| v["host_id"] == host_id && v["name"] == name)
            .ok_or_else(|| anyhow::anyhow!("Unknown native run for this workspace."))?;
        let details = self.inner.details.lock().unwrap();
        let mut view = run_details::enrich(record, details.get(id), chrono::Utc::now());
        view["pool"] = details.get(id).map_or(Value::Null, |d| d["pool"].clone());
        queue::decorate(&mut view, waiting.as_ref());
        Ok(view)
    }
    /// A run started on `pool`, whether or not it has been placed yet.
    pub fn get_pool_run(&self, pool: &str, id: &str) -> Result<Value> {
        native::validate_run_id(id)?;
        let waiting = self.waiting_for(id);
        let entry = self
            .inner
            .sched
            .lock()
            .unwrap()
            .entries
            .get(id)
            .map(|q| q.entry.clone());
        let records = self.inner.records.lock().unwrap();
        let details = self.inner.details.lock().unwrap();
        let detail = details
            .get(id)
            .filter(|d| d["pool"] == pool)
            .ok_or_else(|| anyhow::anyhow!("Unknown run for this pool."))?;
        let mut view = match records.get(id) {
            Some(record) => run_details::enrich(record, Some(detail), chrono::Utc::now()),
            None => {
                let entry = entry.ok_or_else(|| anyhow::anyhow!("Unknown run for this pool."))?;
                entry_view(&entry, Some(detail))
            }
        };
        view["pool"] = json!(pool);
        queue::decorate(&mut view, waiting.as_ref());
        Ok(view)
    }
    fn waiting_for(&self, id: &str) -> Option<(usize, String)> {
        self.inner.sched.lock().unwrap().waiting.get(id).cloned()
    }
    pub fn list_runs(&self) -> Value {
        self.list_runs_visible(|_, _, _| true)
    }
    /// List only runs whose (host, workspace) the caller may see.
    pub fn list_runs_where(&self, visible: impl Fn(&str, &str) -> bool) -> Value {
        self.list_runs_visible(|host, name, _| match (host, name) {
            (Some(h), Some(n)) => visible(h, n),
            _ => false,
        })
    }
    /// List runs the caller may see by (host, workspace) once placed, or by
    /// pool. Unplaced pool runs have neither host nor workspace.
    pub fn list_runs_visible(
        &self,
        visible: impl Fn(Option<&str>, Option<&str>, Option<&str>) -> bool,
    ) -> Value {
        let (waiting, unplaced) = {
            let sched = self.inner.sched.lock().unwrap();
            (
                sched.waiting.clone(),
                sched
                    .entries
                    .values()
                    .map(|q| q.entry.clone())
                    .collect::<Vec<_>>(),
            )
        };
        let records = self.inner.records.lock().unwrap();
        let details = self.inner.details.lock().unwrap();
        let summarize = |id: &str, mut summary: Value| {
            let pool = details.get(id).map_or(Value::Null, |d| d["pool"].clone());
            summary["pool"] = pool;
            queue::decorate(&mut summary, waiting.get(id));
            summary
        };
        let mut summaries: Vec<_> = records
            .iter()
            .filter(|(id, record)| {
                visible(
                    record["host_id"].as_str(),
                    record["name"].as_str(),
                    details.get(*id).and_then(|d| d["pool"].as_str()),
                )
            })
            .map(|(id, record)| summarize(id, run_details::summary(record, details.get(id))))
            .collect();
        for entry in unplaced
            .iter()
            .filter(|e| !records.contains_key(&e.id) && visible(None, None, e.pool()))
        {
            let view = entry_view(entry, details.get(&entry.id));
            let mut summary = run_details::summary(&view, details.get(&entry.id));
            summary["host_id"] = Value::Null;
            summary["name"] = Value::Null;
            summaries.push(summarize(&entry.id, summary));
        }
        summaries.sort_by(|a, b| {
            let active =
                |v: &Value| ["queued", "running"].contains(&v["status"].as_str().unwrap_or(""));
            active(b)
                .cmp(&active(a))
                .then_with(|| b["created_at"].as_str().cmp(&a["created_at"].as_str()))
        });
        let total = summaries.len();
        summaries.truncate(256);
        json!({"runs":summaries,"total":total,"truncated":total>256})
    }
    fn save_detail(&self, id: &str, detail: &Value) -> Result<()> {
        security::write_json(&self.inner.details_root, &format!("{id}.json"), detail)
    }
    fn save_entry(&self, entry: &Entry) -> Result<()> {
        security::write_bytes(
            &self.inner.queue_root,
            &format!("{}.json", entry.id),
            &serde_json::to_vec(entry)?,
            QUEUE_ENTRY_LIMIT,
        )
    }
    fn remove_entry_file(&self, id: &str) {
        let _ = std::fs::remove_file(self.inner.queue_root.join(format!("{id}.json")));
    }
    pub fn cancel_run(&self, host_id: &str, name: &str, id: &str) -> Result<Value> {
        native::validate_run_id(id)?;
        let _pending = self.pending_guard()?;
        // A run still in the durable queue leaves it at once, never starting.
        let dequeued = {
            let mut sched = self.inner.sched.lock().unwrap();
            let queued_here = sched.entries.get(id).is_some_and(|q| {
                q.entry.state == EntryState::Queued
                    && q.entry.target
                        == Target::Workspace {
                            host_id: host_id.into(),
                            name: name.into(),
                        }
            });
            if queued_here {
                sched.entries.remove(id);
                sched.waiting.remove(id);
                sched.dirty = true;
                self.inner.wake.notify_all();
            }
            queued_here
        };
        if dequeued {
            self.remove_entry_file(id);
            self.mark_cancelled(id)?;
            self.finish(id, |v| {
                v["status"] = json!("failed");
                v["result"] = cancelled_result();
            });
            return self.get_run(host_id, name, id);
        }
        let (provider, running) = {
            let records = self.inner.records.lock().unwrap();
            let record = records
                .get(id)
                .filter(|v| v["host_id"] == host_id && v["name"] == name)
                .ok_or_else(|| anyhow::anyhow!("Unknown native run for this workspace."))?;
            if !["queued", "running"].contains(&record["status"].as_str().unwrap_or("")) {
                return Ok(run_details::enrich(
                    record,
                    self.inner.details.lock().unwrap().get(id),
                    chrono::Utc::now(),
                ));
            }
            let mut details = self.inner.details.lock().unwrap();
            let detail = details.get_mut(id).ok_or_else(|| {
                anyhow::anyhow!("This legacy run does not support targeted cancellation.")
            })?;
            detail["cancel_requested"] = json!(true);
            self.save_detail(id, detail)?;
            (
                record["provider"].as_str().unwrap().to_owned(),
                record["status"] == "running",
            )
        };
        // An admitted run that has not started observes the request before
        // launch; a running cancellation may arrive before the remote helper
        // and remains recorded until that exact run observes it.
        if running {
            let host = self.inner.fleet.account_workspace(host_id, name)?;
            let command = guest_command(&host, name, &["cancel".into(), provider, id.into()])?;
            let output = process::checked(&command, None, None, b"", 20, 4096)?;
            let value: Value = serde_json::from_str(&output)?;
            ensure!(
                value["ok"] == true && value["cancel_requested"] == true,
                "Guest did not accept cancellation; update its helper."
            );
        }
        self.get_run(host_id, name, id)
    }
    /// Cancel a run started on `pool`: an unplaced run ends at once; a
    /// placed one is cancelled on its workspace.
    pub fn cancel_pool_run(&self, pool: &str, id: &str) -> Result<Value> {
        native::validate_run_id(id)?;
        let _pending = self.pending_guard()?;
        let ended = {
            let mut sched = self.inner.sched.lock().unwrap();
            match sched.entries.get_mut(id) {
                Some(q)
                    if q.entry.state == EntryState::Queued
                        && q.entry.pool() == Some(pool)
                        && !self.inner.records.lock().unwrap().contains_key(id) =>
                {
                    q.entry.state = EntryState::Finished;
                    q.entry.outcome = Some("cancelled".into());
                    q.entry.finished_at = Some(crate::now());
                    strip_env(&mut q.entry.request);
                    q.entry.callback = None;
                    q.resolved = None;
                    let entry = q.entry.clone();
                    sched.waiting.remove(id);
                    sched.dirty = true;
                    self.inner.wake.notify_all();
                    Some(entry)
                }
                _ => None,
            }
        };
        if let Some(entry) = ended {
            self.save_entry(&entry)?;
            self.mark_cancelled(id)?;
            self.finish_unplaced(&entry);
            return self.get_pool_run(pool, id);
        }
        let placement = {
            let records = self.inner.records.lock().unwrap();
            let details = self.inner.details.lock().unwrap();
            details
                .get(id)
                .filter(|d| d["pool"] == pool)
                .and_then(|_| records.get(id))
                .map(|r| {
                    (
                        r["host_id"].as_str().unwrap_or("").to_owned(),
                        r["name"].as_str().unwrap_or("").to_owned(),
                    )
                })
        };
        match placement {
            Some((host_id, name)) => {
                self.cancel_run(&host_id, &name, id)?;
                self.get_pool_run(pool, id)
            }
            None => self.get_pool_run(pool, id),
        }
    }
    fn mark_cancelled(&self, id: &str) -> Result<()> {
        let mut details = self.inner.details.lock().unwrap();
        if let Some(detail) = details.get_mut(id) {
            detail["cancel_requested"] = json!(true);
            detail["cancelled"] = json!(true);
            self.save_detail(id, detail)?;
        }
        Ok(())
    }
    pub fn launch(&self, host_id: &str, name: &str, value: Value) -> Result<Value> {
        self.launch_as(host_id, name, value, run_details::OWNER)
    }
    /// Launch on behalf of a principal: `owner` or `service-token:<id>`. The
    /// principal and the caller's env key names (never values) are recorded.
    pub fn launch_as(
        &self,
        host_id: &str,
        name: &str,
        value: Value,
        started_by: &str,
    ) -> Result<Value> {
        Ok(self
            .launch_with(host_id, name, value, started_by, LaunchOptions::default())?
            .0)
    }
    /// Register the controller's callback deliverer. Callbacks owed by runs a
    /// restart interrupted are handed to it now, once.
    pub fn set_finish_hook(&self, hook: FinishHook) {
        *self.inner.finish_hook.lock().unwrap() = Some(hook.clone());
        let owed = std::mem::take(&mut *self.inner.owed_callbacks.lock().unwrap());
        for id in owed {
            let finished = {
                let records = self.inner.records.lock().unwrap();
                let details = self.inner.details.lock().unwrap();
                let callback = self.inner.callbacks.lock().unwrap().get(&id).cloned();
                let pending = details
                    .get(&id)
                    .is_some_and(|d| d["callback"]["state"] == "pending");
                match (records.get(&id), callback) {
                    (Some(record), Some(callback)) if pending => Some(FinishedRun {
                        id: id.clone(),
                        callback,
                        payload: callback_payload(record, details.get(&id)),
                    }),
                    _ => None,
                }
            };
            if let Some(finished) = finished {
                let _ = self.record_callback(&id, "delivering", 0, None);
                hook(finished);
            } else {
                self.inner.callbacks.lock().unwrap().remove(&id);
            }
            self.remove_entry_file(&id);
        }
    }
    pub fn set_measure_hook(&self, hook: MeasureHook) {
        *self.inner.hooks.measure.lock().unwrap() = Some(hook);
        self.inner.measures.lock().unwrap().clear();
        self.kick();
    }
    /// Replace the guest lease probe (tests).
    pub fn set_lease_probe(&self, hook: LeaseProbe) {
        *self.inner.hooks.lease_probe.lock().unwrap() = Some(hook);
        self.inner.probes.lock().unwrap().clear();
        self.kick();
    }
    pub fn set_create_hook(&self, hook: CreateHook) {
        *self.inner.hooks.create.lock().unwrap() = Some(hook);
    }
    pub fn set_profile_resolver(&self, hook: ProfileResolver) {
        *self.inner.hooks.resolver.lock().unwrap() = Some(hook);
    }
    /// Replace workspace preparation and the guest helper. For tests that
    /// exercise scheduling without a live workspace.
    #[doc(hidden)]
    /// Install the controller's repository token minter and revoker.
    pub fn set_repository_hooks(&self, mint: RepositoryMint, revoke: RepositoryRevoke) {
        *self.inner.hooks.repository.lock().unwrap() = Some((mint, revoke));
    }
    pub fn set_run_executor(&self, hook: RunExecutor) {
        *self.inner.hooks.executor.lock().unwrap() = Some(hook);
    }
    pub fn limits(&self) -> QueueLimits {
        self.inner.limits.lock().unwrap().clone()
    }
    /// Replace the queue bounds and admission defaults at runtime.
    pub fn set_limits(&self, limits: QueueLimits) {
        *self.inner.limits.lock().unwrap() = limits;
        self.kick();
    }
    pub fn pools(&self) -> &pools::Pools {
        &self.inner.pools
    }
    /// Stop admitting queued runs (controller drain). Queued runs stay
    /// durable; running ones finish.
    pub fn pause_admission(&self) {
        self.inner.admitting.store(false, Ordering::SeqCst);
        self.kick();
    }
    pub fn resume_admission(&self) {
        self.inner.admitting.store(true, Ordering::SeqCst);
        self.kick();
    }
    /// Start the scheduler. Launches start it too; the controller starts it
    /// after installing its hooks so restored runs see them.
    pub fn start_scheduler(&self) {
        if self.inner.dispatcher.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        thread::spawn(move || {
            loop {
                let Some(inner) = weak.upgrade() else { break };
                let service = AgentService { inner };
                if service.inner.closed.load(Ordering::SeqCst) {
                    break;
                }
                service.dispatch();
                let sched = service.inner.sched.lock().unwrap();
                if !sched.dirty {
                    let _ = service
                        .inner
                        .wake
                        .wait_timeout(sched, Duration::from_secs(2))
                        .unwrap();
                }
            }
        });
    }
    fn kick(&self) {
        self.inner.sched.lock().unwrap().dirty = true;
        self.inner.wake.notify_all();
    }
    /// The run an idempotency key already names for this principal, if any.
    fn replay(&self, key_digest: &str, request_digest: &str) -> Result<Option<String>> {
        let details = self.inner.details.lock().unwrap();
        let Some((id, detail)) = details
            .iter()
            .find(|(_, d)| d["idempotency"]["key_digest"] == key_digest)
        else {
            return Ok(None);
        };
        ensure!(
            detail["idempotency"]["request_digest"] == request_digest,
            "idempotency_key was already used with a different request."
        );
        Ok(Some(id.clone()))
    }
    /// Launch with the optional v2 features. Returns the run and whether it is
    /// an existing run returned for a repeated idempotency key. The run is
    /// accepted into the durable queue (`queued`) and starts when its
    /// workspace and host have room.
    pub fn launch_with(
        &self,
        host_id: &str,
        name: &str,
        value: Value,
        started_by: &str,
        options: LaunchOptions,
    ) -> Result<(Value, bool)> {
        enginebox_core::identifier(host_id, false)?;
        enginebox_core::identifier(name, true)?;
        let template = options.profile.as_ref().and_then(|p| p.template.clone());
        self.enqueue(
            Target::Workspace {
                host_id: host_id.into(),
                name: name.into(),
            },
            value,
            started_by,
            options,
            template,
        )
    }
    /// Launch on any free workspace of `pool`. A `resume_id` lands on the
    /// workspace that holds that session.
    pub fn launch_pool(
        &self,
        pool: &str,
        value: Value,
        started_by: &str,
        options: LaunchOptions,
    ) -> Result<(Value, bool)> {
        let definition = self
            .inner
            .pools
            .get(pool)
            .ok_or_else(|| anyhow::anyhow!("Unknown pool."))?;
        ensure!(
            definition.profile.is_none()
                || options.profile.as_ref().map(|p| &p.name) == definition.profile.as_ref(),
            "Runs in this pool use the pool's profile."
        );
        let profile_template = options.profile.as_ref().and_then(|p| p.template.clone());
        ensure!(
            definition.template.is_none()
                || profile_template.is_none()
                || definition.template == profile_template,
            "The profile's template differs from the pool's."
        );
        let request = native::RunRequest::parse(value.clone())?;
        let pinned = match &request.resume_id {
            None => None,
            Some(session) => Some(self.pool_session_home(pool, session).ok_or_else(|| {
                anyhow::anyhow!("resume_id is not a session started in this pool.")
            })?),
        };
        self.enqueue(
            Target::Pool {
                pool: pool.into(),
                pinned,
            },
            value,
            started_by,
            options,
            definition.template.or(profile_template),
        )
    }
    fn pool_session_home(&self, pool: &str, session: &str) -> Option<Placement> {
        if let Some(home) = self.inner.pools.session_home(session, pool) {
            return Some(home);
        }
        let records = self.inner.records.lock().unwrap();
        let details = self.inner.details.lock().unwrap();
        records
            .iter()
            .filter(|(id, r)| {
                r["native_session_id"] == session
                    && details.get(*id).is_some_and(|d| d["pool"] == pool)
            })
            .max_by_key(|(_, r)| r["created_at"].as_str().unwrap_or("").to_owned())
            .map(|(_, r)| Placement {
                host_id: r["host_id"].as_str().unwrap_or("").to_owned(),
                name: r["name"].as_str().unwrap_or("").to_owned(),
            })
    }
    fn enqueue(
        &self,
        target: Target,
        value: Value,
        started_by: &str,
        options: LaunchOptions,
        template: Option<String>,
    ) -> Result<(Value, bool)> {
        run_details::validate_principal(started_by)?;
        let request = native::RunRequest::parse(value)?;
        if let Some(resolved) = &options.profile {
            ensure!(profile::valid_name(&resolved.name), "Invalid profile name.");
            let envelope = profile::Envelope {
                request: request.clone(),
                profile: resolved.name.clone(),
                block: resolved.block.clone(),
                files: resolved.files.clone(),
            };
            envelope.validate()?;
        }
        if let Some(r) = &options.repository {
            repository::parse_ref(r)?;
            let profile_env = options.profile.as_ref().map(|p| &p.block.env);
            ensure!(
                !request
                    .env
                    .keys()
                    .chain(profile_env.into_iter().flat_map(|e| e.keys()))
                    .any(|k| repository::reserved_env_key(k)),
                "env cannot set git configuration variables in a repository run."
            );
        }
        if let Some(key) = &options.idempotency_key {
            ensure!(
                valid_idempotency_key(key),
                "idempotency_key must be 1-128 characters of A-Z a-z 0-9 . _ : -"
            );
        }
        let request_digest = idempotency_request_digest(&request, &options);
        // Acceptance is serialized by the scheduler lock, so two concurrent
        // launches with one key cannot both create a run.
        let mut sched = self.inner.sched.lock().unwrap();
        if let Some(key) = &options.idempotency_key
            && let Some(id) = self.replay(
                &scoped_key_digest(started_by, &target, key),
                &request_digest,
            )?
        {
            drop(sched);
            let view = match &target {
                Target::Workspace { host_id, name } => self.get_run(host_id, name, &id),
                Target::Pool { pool, .. } => self.get_pool_run(pool, &id),
            };
            return Ok((
                view.map_err(|_| {
                    anyhow::anyhow!("idempotency_key was already used with a different request.")
                })?,
                true,
            ));
        }
        ensure!(
            !self.inner.closed.load(Ordering::SeqCst),
            "Agent service is draining."
        );
        let queued = |principal: Option<&str>| {
            sched
                .entries
                .values()
                .filter(|q| {
                    q.entry.state == EntryState::Queued
                        && principal.is_none_or(|p| q.entry.principal == p)
                })
                .count()
        };
        if queued(None) >= self.limits().max_queued_total {
            return Err(queue::QueueFull {
                scope: "controller",
                retry_after_seconds: self.limits().retry_after_seconds,
            }
            .into());
        }
        if queued(Some(started_by)) >= self.limits().max_queued_per_principal {
            return Err(queue::QueueFull {
                scope: "caller's",
                retry_after_seconds: self.limits().retry_after_seconds,
            }
            .into());
        }
        match &target {
            Target::Workspace { host_id, .. } => {
                self.inner.fleet.host(host_id)?;
            }
            Target::Pool {
                pinned: Some(p), ..
            } => {
                self.inner.fleet.host(&p.host_id)?;
            }
            Target::Pool { .. } => {}
        }
        ensure!(
            self.material(&request.provider).is_ok(),
            "Connect this agent on the controller before launching a session."
        );
        let id = uuid::Uuid::new_v4().simple().to_string();
        let seq = sched.next_seq;
        sched.next_seq += 1;
        let (record, mut detail) = match &target {
            Target::Workspace { host_id, name } => {
                new_run_entries(&id, host_id, name, &request, started_by)
            }
            // Placeholder workspace; the record is written at placement.
            Target::Pool { .. } => {
                new_run_entries(&id, "unplaced", "ebox-unplaced", &request, started_by)
            }
        };
        if let Target::Pool { pool, .. } = &target {
            detail["pool"] = json!(pool);
        }
        if let Some(resolved) = &options.profile {
            detail["profile"] = json!({"name":resolved.name,"revision":resolved.revision});
        }
        if let Some(r) = &options.repository {
            detail["repository"] = json!(r);
        }
        if let Some(key) = &options.idempotency_key {
            detail["idempotency"] = json!({
                "key_digest":scoped_key_digest(started_by, &target, key),
                "request_digest":request_digest,
            });
        }
        if let Some(callback) = &options.callback {
            detail["callback"] = json!({
                "origin":callback_origin(&callback.url),
                "state":"pending",
                "attempts":0,
                "last_status":null,
                "updated_at":crate::now(),
            });
        }
        let entry = Entry {
            schema: 1,
            id: id.clone(),
            seq,
            principal: started_by.into(),
            created_at: record["created_at"].as_str().unwrap_or("").into(),
            target: target.clone(),
            state: EntryState::Queued,
            request: serde_json::to_value(&request)?,
            profile: options.profile.as_ref().map(|p| p.name.clone()),
            template,
            callback: options.callback.as_ref().map(|c| StoredCallback {
                url: c.url.clone(),
                secret: c.secret.clone(),
            }),
            placement: None,
            finished_at: None,
            outcome: None,
            repository: options.repository.clone(),
            run_kind: options.run_kind.clone(),
        };
        // Order: sidecar, record, queue entry. A crash before the entry leaves
        // a queued record that the next start marks failed, never a lost run.
        self.save_detail(&id, &detail)?;
        self.inner
            .details
            .lock()
            .unwrap()
            .insert(id.clone(), detail);
        if matches!(target, Target::Workspace { .. }) {
            self.save_record(&record)?;
            self.inner
                .records
                .lock()
                .unwrap()
                .insert(id.clone(), record);
        }
        self.save_entry(&entry)?;
        if let Some(callback) = &options.callback {
            self.inner
                .callbacks
                .lock()
                .unwrap()
                .insert(id.clone(), callback.clone());
        }
        sched.entries.insert(
            id.clone(),
            Queued {
                entry,
                resolved: options.profile,
            },
        );
        sched.dirty = true;
        drop(sched);
        self.inner.wake.notify_all();
        self.start_scheduler();
        let view = match &target {
            Target::Workspace { host_id, name } => self.get_run(host_id, name, &id)?,
            Target::Pool { pool, .. } => self.get_pool_run(pool, &id)?,
        };
        Ok((view, false))
    }
    /// One scheduler pass: refresh stale host measurements, plan, admit.
    fn dispatch(&self) {
        let needed: BTreeSet<String> = {
            let mut sched = self.inner.sched.lock().unwrap();
            sched.dirty = false;
            let queued: Vec<&Entry> = sched
                .entries
                .values()
                .map(|q| &q.entry)
                .filter(|e| e.state == EntryState::Queued)
                .collect();
            if queued.is_empty() {
                sched.waiting.clear();
                return;
            }
            let mut hosts = BTreeSet::new();
            for e in queued {
                match &e.target {
                    Target::Workspace { host_id, .. } => {
                        hosts.insert(host_id.clone());
                    }
                    Target::Pool {
                        pinned: Some(p), ..
                    } => {
                        hosts.insert(p.host_id.clone());
                    }
                    Target::Pool { pool, pinned: None } => {
                        if let Some(def) = self.inner.pools.get(pool) {
                            hosts.extend(def.members().into_iter().map(|m| m.host_id));
                            if let Some(a) = def.autoscale {
                                hosts.extend(a.hosts);
                            }
                        }
                    }
                }
            }
            hosts
        };
        self.refresh_measurements(&needed);
        let uncertain = self.probe_uncertain();
        let mut sched = self.inner.sched.lock().unwrap();
        let leases = self.inner.leases.lock().unwrap().clone();
        let mut busy: BTreeSet<(String, String)> = leases
            .iter()
            .map(|(h, n, _)| (h.clone(), n.clone()))
            .collect();
        busy.extend(
            sched
                .running
                .values()
                .map(|p| (p.host_id.clone(), p.name.clone())),
        );
        busy.extend(
            self.inner
                .records
                .lock()
                .unwrap()
                .values()
                .filter(|v| v["execution_uncertain"] == true)
                .map(|v| {
                    (
                        v["host_id"].as_str().unwrap_or("").to_owned(),
                        v["name"].as_str().unwrap_or("").to_owned(),
                    )
                }),
        );
        let measures = self.inner.measures.lock().unwrap();
        let mut hosts = BTreeMap::new();
        for host in self.inner.fleet.hosts() {
            hosts.insert(
                host.id.clone(),
                queue::HostState {
                    max_runs: host
                        .max_agent_runs
                        .map_or(self.limits().default_host_max_runs, |n| n as usize),
                    running: sched
                        .running
                        .values()
                        .filter(|p| p.host_id == host.id)
                        .count(),
                    load: self
                        .inner
                        .fleet
                        .assert_admission(&host.id)
                        .ok()
                        .and_then(|()| measures.get(&host.id).and_then(|m| m.load)),
                    admitted_since: sched.admitted_since.get(&host.id).map_or(0, BTreeSet::len),
                },
            );
        }
        drop(measures);
        let now = Instant::now();
        let mut pool_states = BTreeMap::new();
        for def in self.inner.pools.all() {
            pool_states.insert(
                def.name.clone(),
                queue::PoolState {
                    members: def.members(),
                    creating: sched.creating.get(&def.name).copied().unwrap_or(0),
                    create_hosts: def
                        .autoscale
                        .as_ref()
                        .map(|a| a.hosts.clone())
                        .unwrap_or_default(),
                    max_workspaces: def.max_workspaces(),
                    backoff: sched
                        .backoff
                        .get(&def.name)
                        .is_some_and(|(until, _)| *until > now),
                    backoff_reason: sched
                        .backoff
                        .get(&def.name)
                        .filter(|(until, _)| *until > now)
                        .map(|(_, why)| why.clone()),
                },
            );
        }
        let candidates: Vec<queue::Candidate> = sched
            .entries
            .values()
            .filter(|q| q.entry.state == EntryState::Queued)
            .map(|q| queue::Candidate {
                id: q.entry.id.clone(),
                principal: q.entry.principal.clone(),
                seq: q.entry.seq,
                provider: q.entry.provider().to_owned(),
                target: q.entry.target.clone(),
            })
            .collect();
        let plan = queue::plan(queue::PlanInput {
            candidates: &candidates,
            cursor: sched.cursor.as_deref(),
            hosts,
            leases,
            busy_workspaces: busy,
            uncertain,
            pools: pool_states,
            headroom_bytes: self.limits().run_memory_headroom_bytes,
            admitting: self.inner.admitting.load(Ordering::SeqCst)
                && !self.inner.closed.load(Ordering::SeqCst),
        });
        sched.waiting = plan.waiting;
        sched.cursor = plan.cursor;
        for (id, placement) in plan.admit {
            let started = Instant::now();
            if let Err(error) = self.admit(&mut sched, &id, placement.clone()) {
                // The run stays queued and is retried on a later pass.
                let entry = sched.entries.get(&id).map(|q| q.entry.clone());
                self.record_launch(LaunchEvent {
                    kind: "run_admission_failed".into(),
                    outcome: "failed".into(),
                    run_id: Some(id.clone()),
                    pool: entry.as_ref().and_then(|e| e.pool().map(str::to_owned)),
                    host_id: Some(placement.host_id.clone()),
                    workspace: Some(placement.name.clone()),
                    template: entry.as_ref().and_then(|e| e.template.clone()),
                    profile: entry.as_ref().and_then(|e| e.profile.clone()),
                    provider: entry.as_ref().map(|e| e.provider().to_owned()),
                    stage: Stage::Place.as_str().into(),
                    reason: "admission_error".into(),
                    message: format!("{error:#}"),
                    duration_ms: started.elapsed().as_millis() as u64,
                    ..Default::default()
                });
            }
        }
        for (pool, host) in plan.create {
            *sched.creating.entry(pool.clone()).or_default() += 1;
            self.spawn_create(pool, host);
        }
    }
    fn refresh_measurements(&self, hosts: &BTreeSet<String>) {
        let ttl = Duration::from_secs(self.limits().measure_ttl_seconds);
        let stale: Vec<Host> = {
            let measures = self.inner.measures.lock().unwrap();
            hosts
                .iter()
                .filter(|h| measures.get(*h).is_none_or(|m| m.at.elapsed() >= ttl))
                .filter_map(|h| self.inner.fleet.host(h).ok())
                .collect()
        };
        if stale.is_empty() {
            return;
        }
        let hook = self.inner.hooks.measure.lock().unwrap().clone();
        let results: Vec<(String, Option<HostLoad>)> = thread::scope(|scope| {
            stale
                .iter()
                .map(|host| {
                    let hook = hook.clone();
                    scope.spawn(move || {
                        let load = match hook {
                            Some(measure) => measure(host),
                            None => enginebox_core::capacity::inventory(host)
                                .and_then(|facts| HostLoad::from_inventory(host, &facts)),
                        };
                        (host.id.clone(), load.ok())
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .filter_map(|j| j.join().ok())
                .collect()
        });
        let mut sched = self.inner.sched.lock().unwrap();
        let mut measures = self.inner.measures.lock().unwrap();
        for (host, load) in results {
            if load.is_some() {
                sched.admitted_since.remove(&host);
            }
            measures.insert(
                host,
                Measure {
                    at: Instant::now(),
                    load,
                },
            );
        }
    }
    /// Move one queued run to running on `placement` and start its worker.
    fn admit(&self, sched: &mut Sched, id: &str, placement: Placement) -> Result<()> {
        self.inner.fleet.assert_admission(&placement.host_id)?;
        let pending = self.pending_guard()?;
        let queued = sched
            .entries
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("Queued run vanished."))?;
        let request = native::RunRequest::parse(queued.entry.request.clone())?;
        if let Target::Pool { .. } = queued.entry.target {
            let (record, _) = new_run_entries(
                id,
                &placement.host_id,
                &placement.name,
                &request,
                &queued.entry.principal,
            );
            let mut record = record;
            record["created_at"] = json!(queued.entry.created_at);
            self.save_record(&record)?;
            self.inner
                .records
                .lock()
                .unwrap()
                .insert(id.to_owned(), record);
        }
        let mut admitted = queued.entry.clone();
        admitted.state = EntryState::Admitted;
        admitted.placement = Some(placement.clone());
        // Env values leave the queue file once the run is handed to its worker.
        strip_env(&mut admitted.request);
        self.save_entry(&admitted)?;
        queued.entry = admitted;
        let resolved = queued.resolved.take();
        let queue_wait_ms = chrono::DateTime::parse_from_rfc3339(&queued.entry.created_at)
            .map(|at| {
                chrono::Utc::now()
                    .signed_duration_since(at)
                    .num_milliseconds()
                    .max(0) as u64
            })
            .unwrap_or(0);
        let run = AdmittedRun {
            id: id.to_owned(),
            host_id: placement.host_id.clone(),
            name: placement.name.clone(),
            pool: queued.entry.pool().map(str::to_owned),
            request,
            profile: queued.entry.profile.clone(),
            run_kind: queued.entry.run_kind.clone(),
            queue_wait_ms,
            repository: queued.entry.repository.clone(),
            repository_access: None,
        };
        let template = queued.entry.template.clone();
        let lease = (
            placement.host_id.clone(),
            placement.name.clone(),
            run.request.provider.clone(),
        );
        self.inner.leases.lock().unwrap().insert(lease.clone());
        sched.running.insert(id.to_owned(), placement.clone());
        sched
            .admitted_since
            .entry(placement.host_id.clone())
            .or_default()
            .insert(id.to_owned());
        sched.waiting.remove(id);
        let this = self.clone();
        thread::spawn(move || {
            let _pending = pending;
            let id = run.id.clone();
            let pool = run.pool.clone();
            this.execute(run, resolved, template);
            this.inner.leases.lock().unwrap().remove(&lease);
            let session = this
                .inner
                .records
                .lock()
                .unwrap()
                .get(&id)
                .and_then(|r| r["native_session_id"].as_str().map(str::to_owned));
            if let (Some(pool), Some(session)) = (pool, session) {
                let _ = this
                    .inner
                    .pools
                    .remember_session(&session, &pool, &placement);
            }
            {
                let mut sched = this.inner.sched.lock().unwrap();
                sched.running.remove(&id);
                sched.entries.remove(&id);
                for reserved in sched.admitted_since.values_mut() {
                    reserved.remove(&id);
                }
                sched.dirty = true;
            }
            this.remove_entry_file(&id);
            this.inner.wake.notify_all();
        });
        Ok(())
    }
    /// Prepare the workspace and run the native CLI, or fail the run cleanly
    /// (and log a launch event) when it cannot start.
    fn execute(
        &self,
        run: AdmittedRun,
        resolved: Option<ResolvedProfile>,
        template: Option<String>,
    ) {
        let started = Instant::now();
        if let Err(error) = self.inner.fleet.assert_admission(&run.host_id) {
            let failure = LaunchFailure::new(
                Stage::Place,
                "managed_host_unavailable",
                format!("{error:#}"),
            );
            self.launch_failed(&run, template.as_deref(), None, &failure, started);
            return;
        }
        let resolved = match (&run.profile, resolved) {
            (None, _) => None,
            (Some(_), Some(r)) => Some(r),
            (Some(name), None) => {
                let resolver = self.inner.hooks.resolver.lock().unwrap().clone();
                match resolver.map(|r| r(name, &run.request.provider, run.run_kind.as_deref())) {
                    Some(Ok(r)) => Some(r),
                    other => {
                        let message = match other {
                            Some(Err(error)) => format!("{error:#}"),
                            _ => "No profile resolver is installed.".into(),
                        };
                        let failure =
                            LaunchFailure::new(Stage::Resolve, "profile_unavailable", message);
                        self.launch_failed(&run, template.as_deref(), None, &failure, started);
                        return;
                    }
                }
            }
        };
        if let Some(r) = &resolved {
            self.inner
                .details
                .lock()
                .unwrap()
                .entry(run.id.clone())
                .and_modify(|d| {
                    d["profile"] = json!({"name":r.name,"revision":r.revision});
                    if let Some(kind) = &r.run_kind {
                        d["profile"]["run_kind"] = json!(kind);
                    }
                    if !r.connections.is_empty() {
                        d["profile"]["connections"] = json!(r.connections);
                    }
                    let _ = security::write_json(
                        &self.inner.details_root,
                        &format!("{}.json", run.id),
                        d,
                    );
                });
        }
        let template = template.or_else(|| resolved.as_ref().and_then(|p| p.template.clone()));
        // The repository token is minted now, at admission (never at
        // acceptance), and revoked when the run ends.
        let mut run = run;
        let credential = match &run.repository {
            None => None,
            Some(r) => match self.mint_repository(r, run.request.timeout_seconds) {
                Ok(credential) => Some(credential),
                Err(failure) => {
                    self.launch_failed(
                        &run,
                        template.as_deref(),
                        resolved.as_ref(),
                        &failure,
                        started,
                    );
                    return;
                }
            },
        };
        run.repository_access = credential.as_ref().map(repository::Credential::access);
        self.execute_admitted(&run, resolved, template, started);
        if let (Some(r), Some(credential)) = (&run.repository, &credential) {
            let revoke = self.inner.hooks.repository.lock().unwrap().clone();
            if let Some((_, revoke)) = revoke {
                revoke(r, credential);
            }
        }
    }
    fn mint_repository(
        &self,
        r: &str,
        timeout_seconds: u64,
    ) -> std::result::Result<repository::Credential, LaunchFailure> {
        let hooks = self.inner.hooks.repository.lock().unwrap().clone();
        let Some((mint, _)) = hooks else {
            return Err(LaunchFailure::new(
                Stage::Resolve,
                "repository_unavailable",
                "No repository backend is installed.",
            ));
        };
        let credential = mint(r, repository::run_token_ttl(timeout_seconds))
            .map_err(|e| LaunchFailure::from_error(Stage::Resolve, "repository_unavailable", &e))?;
        credential
            .access()
            .validate()
            .map_err(|e| LaunchFailure::from_error(Stage::Resolve, "repository_unavailable", &e))?;
        Ok(credential)
    }
    fn execute_admitted(
        &self,
        run: &AdmittedRun,
        resolved: Option<ResolvedProfile>,
        template: Option<String>,
        started: Instant,
    ) {
        let executor = self.inner.hooks.executor.lock().unwrap().clone();
        if let Some(executor) = executor {
            let request = run.request.clone();
            let outcome = self.run_with(&run.id, &request, || executor(run));
            if let Err(failure) = outcome {
                self.launch_failed(
                    run,
                    template.as_deref(),
                    resolved.as_ref(),
                    &failure,
                    started,
                );
            }
            return;
        }
        let prepared = self
            .prepare(run, template.as_deref(), resolved.as_ref())
            .and_then(|(host, guest_current)| {
                if run.repository_access.is_some() {
                    self.check_guest_supports_repository(&host, &run.name)?;
                }
                Ok((host, guest_current))
            });
        match prepared {
            Ok((host, guest_current)) => {
                let request = run.request.clone();
                // Checkouts need a guest helper from this release; an older
                // one keeps the previous in-workspace behavior.
                let checkout = run
                    .repository
                    .as_deref()
                    .filter(|_| run.repository_access.is_some() && guest_current)
                    .and_then(|r| repository::parse_ref(r).ok())
                    .map(|(_, name)| name);
                if let Err(failure) = self.run_worker_with(
                    host,
                    &run.name,
                    request,
                    &run.id,
                    resolved.clone(),
                    run.repository_access.clone().map(|a| (a, checkout)),
                ) {
                    self.launch_failed(
                        run,
                        template.as_deref(),
                        resolved.as_ref(),
                        &failure,
                        started,
                    );
                }
            }
            Err(failure) => self.launch_failed(
                run,
                template.as_deref(),
                resolved.as_ref(),
                &failure,
                started,
            ),
        }
    }
    /// Make sure the workspace's guest helper is this release's, refreshing
    /// it through the host helper when it is not. Returns whether it is
    /// current. A repository run with an outdated helper fails here, before
    /// anything starts; other runs proceed on the old helper. Each refresh is
    /// a `guest_refresh` launch event. The caller holds the host lock.
    fn ensure_guest(
        &self,
        host: &Host,
        run: &AdmittedRun,
        template: Option<&str>,
        resolved: Option<&ResolvedProfile>,
    ) -> std::result::Result<bool, LaunchFailure> {
        let key = (host.id.clone(), run.name.clone());
        let outdated = |why: String| -> std::result::Result<bool, LaunchFailure> {
            if run.repository_access.is_some() {
                Err(LaunchFailure::new(Stage::Bootstrap, "guest_outdated", why))
            } else {
                Ok(false)
            }
        };
        // Checked recently: no new probe or refresh attempt (and no event).
        let cached = self
            .inner
            .guests
            .lock()
            .unwrap()
            .get(&key)
            .filter(|(at, _)| at.elapsed() < GUEST_CHECK_TTL)
            .map(|(_, why)| why.clone());
        match cached {
            Some(None) => return Ok(true),
            Some(Some(why)) => return outdated(why),
            None => {}
        }
        let started = Instant::now();
        let bx = host.engine_box();
        let name = run.name.as_str();
        let outcome = refresh_guest(
            &expected_guest_version(),
            || {
                let bytes = bx.execute(
                    name,
                    &["enginebox-guest".into(), "--version".into()],
                    Duration::from_secs(15),
                )?;
                Ok(String::from_utf8_lossy(&bytes).trim().to_owned())
            },
            || {
                let dir = release_bin_dir(host);
                enginebox_core::guest::ensure_remote_guest_tools_from(
                    &bx,
                    name,
                    true,
                    dir.as_deref(),
                )
                .or_else(|error| match dir {
                    // The host helper may refuse this release's directory
                    // (ownership); its own release is the fallback.
                    Some(_) => {
                        enginebox_core::guest::ensure_remote_guest_tools_from(&bx, name, true, None)
                    }
                    None => Err(error),
                })
                .map(|_| ())
            },
        );
        let why = (!outcome.current).then(|| outcome.event().unwrap_or_default());
        self.inner
            .guests
            .lock()
            .unwrap()
            .insert(key, (Instant::now(), why.clone()));
        if let Some(event) = outcome.event() {
            self.record_launch(LaunchEvent {
                kind: "guest_refresh".into(),
                outcome: if outcome.current {
                    "succeeded"
                } else {
                    "failed"
                }
                .into(),
                run_id: Some(run.id.clone()),
                pool: run.pool.clone(),
                host_id: Some(host.id.clone()),
                workspace: Some(run.name.clone()),
                template: template.map(str::to_owned),
                template_revision: template.and_then(|t| self.template_revision(&host.id, t)),
                profile: resolved
                    .map(|p| p.name.clone())
                    .or_else(|| run.profile.clone()),
                profile_revision: resolved.map(|p| p.revision),
                provider: Some(run.request.provider.clone()),
                stage: Stage::Bootstrap.as_str().into(),
                reason: if outcome.current {
                    "ok"
                } else {
                    "guest_outdated"
                }
                .into(),
                message: launch_events::clip_message(&event, &BTreeMap::new()),
                duration_ms: started.elapsed().as_millis() as u64,
                queue_wait_ms: Some(run.queue_wait_ms),
                ..Default::default()
            });
        }
        match why {
            None => Ok(true),
            Some(why) => outdated(why),
        }
    }
    /// Older guest helpers do not know `run-repository`; refuse the run with
    /// a clear reason before anything starts instead of a generic failure.
    fn check_guest_supports_repository(
        &self,
        host: &Host,
        name: &str,
    ) -> std::result::Result<(), LaunchFailure> {
        let help = guest_command(host, name, &["--help".into()])
            .and_then(|command| process::checked(&command, None, None, b"", 20, 65536))
            .map_err(|e| LaunchFailure::from_error(Stage::Bootstrap, "guest_outdated", &e))?;
        if repository::guest_supports(&help) {
            Ok(())
        } else {
            Err(LaunchFailure::new(
                Stage::Bootstrap,
                "guest_outdated",
                "The workspace's guest helper does not list run-repository; update enginebox-guest.",
            ))
        }
    }
    /// Check each launch step in order; the first that fails names its stage.
    fn prepare(
        &self,
        run: &AdmittedRun,
        template: Option<&str>,
        resolved: Option<&ResolvedProfile>,
    ) -> std::result::Result<(Host, bool), LaunchFailure> {
        let (host_id, name) = (run.host_id.as_str(), run.name.as_str());
        let host = self
            .inner
            .fleet
            .account_workspace(host_id, name)
            .map_err(|e| LaunchFailure::from_error(Stage::Place, "workspace_unavailable", &e))?;
        if let Some(template) = template {
            let actual = self
                .inner
                .fleet
                .workspace_template(host_id, name)
                .map_err(|e| {
                    LaunchFailure::from_error(Stage::Place, "workspace_unavailable", &e)
                })?;
            if actual.as_deref() != Some(template) {
                return Err(LaunchFailure::new(
                    Stage::Place,
                    "template_mismatch",
                    format!(
                        "Workspace template is {}; the run requires {template}.",
                        actual.as_deref().unwrap_or("unrecorded")
                    ),
                ));
            }
        }
        // Pool scale-ups hold the host lock for minutes while they create a
        // workspace; a run placed meanwhile waits for it instead of failing.
        let _hostlock = enginebox_core::operation_lock_wait(&host, HOST_LOCK_WAIT)
            .map_err(|e| LaunchFailure::from_error(Stage::Bootstrap, "workspace_locked", &e))?;
        let uncertain = self.inner.records.lock().unwrap().values().any(|v| {
            v["execution_uncertain"] == true && v["host_id"] == host_id && v["name"] == name
        });
        if uncertain && self.active(host_id, name) {
            return Err(LaunchFailure::new(
                Stage::Place,
                "lease_unresolved",
                "Previous native run must release its guest lease.",
            ));
        }
        let guest_current = self.ensure_guest(&host, run, template, resolved)?;
        let attached = self
            .attach_locked(
                host_id,
                name,
                Some(vec![run.request.provider.clone()]),
                None,
                None,
            )
            .map_err(|e| LaunchFailure::from_error(Stage::Delegate, "delegation_failed", &e))?;
        if !attached["providers"].as_array().is_some_and(|items| {
            items.len() == 1
                && items.iter().all(|item| {
                    ["attached", "unchanged"].contains(&item["status"].as_str().unwrap_or(""))
                })
        }) {
            return Err(LaunchFailure::new(
                Stage::Delegate,
                "provider_not_delegable",
                "The provider could not be delegated to this workspace.",
            ));
        }
        Ok((host, guest_current))
    }
    /// Record a launch failure: a structured log line and event, the run's
    /// `failure` (stage and reason code), and its terminal state. A failure
    /// before the guest started is not `execution_uncertain`; a guest launch
    /// failure keeps the original uncertain handling.
    fn launch_failed(
        &self,
        run: &AdmittedRun,
        template: Option<&str>,
        resolved: Option<&ResolvedProfile>,
        failure: &LaunchFailure,
        started: Instant,
    ) {
        let mut secrets = run.request.env.clone();
        if let Some(p) = resolved {
            secrets.extend(p.block.env.clone());
        }
        if let Some(access) = &run.repository_access {
            secrets.insert("ENGINEBOX_REPO_TOKEN".into(), access.token.clone());
        }
        let profile_name = resolved
            .map(|p| p.name.clone())
            .or_else(|| run.profile.clone());
        self.record_launch(LaunchEvent {
            kind: "run_admission_failed".into(),
            outcome: "failed".into(),
            run_id: Some(run.id.clone()),
            pool: run.pool.clone(),
            host_id: Some(run.host_id.clone()),
            workspace: Some(run.name.clone()),
            template: template.map(str::to_owned),
            template_revision: template.and_then(|t| self.template_revision(&run.host_id, t)),
            profile: profile_name,
            profile_revision: resolved.map(|p| p.revision),
            provider: Some(run.request.provider.clone()),
            stage: failure.stage.as_str().into(),
            reason: failure.reason.into(),
            message: launch_events::clip_message(&failure.message, &secrets),
            duration_ms: started.elapsed().as_millis() as u64,
            queue_wait_ms: Some(run.queue_wait_ms),
            ..Default::default()
        });
        if let Some(detail) = self.inner.details.lock().unwrap().get_mut(&run.id) {
            detail["failure"] = json!({"stage":failure.stage.as_str(),"reason":failure.reason});
            let _ = self.save_detail(&run.id, detail);
        }
        let uncertain = failure.stage == Stage::Start;
        let message = failure.run_error();
        self.finish(&run.id, |v| {
            v["status"] = json!("failed");
            v["error"] = json!(message);
            if uncertain {
                v["execution_uncertain"] = json!(true);
            }
        });
    }
    /// Fill in the pool's project, then log and retain the event.
    fn record_launch(&self, mut event: LaunchEvent) {
        if event.project.is_none()
            && let Some(pool) = &event.pool
        {
            event.project = self.inner.pools.get(pool).map(|p| p.project);
        }
        self.inner.launch_events.record(event);
    }
    /// The installed revision of `template` on `host`, when the registry knows it.
    fn template_revision(&self, host: &str, template: &str) -> Option<Value> {
        let host = self.inner.fleet.host(host).ok()?;
        let record = self.inner.fleet.template(&host, template).ok()?;
        let revision = record.get("revision").cloned().unwrap_or(Value::Null);
        (!revision.is_null()).then_some(revision)
    }
    /// Owner view of the launch history (newest first).
    pub fn launch_events(&self, filter: &launch_events::Filter) -> Value {
        self.inner.launch_events.list(filter)
    }
    fn spawn_create(&self, pool: String, host: String) {
        let this = self.clone();
        thread::spawn(move || {
            let started = Instant::now();
            let mut event = LaunchEvent {
                kind: "pool_member_created".into(),
                outcome: "succeeded".into(),
                pool: Some(pool.clone()),
                host_id: Some(host.clone()),
                stage: Stage::Create.as_str().into(),
                reason: "ok".into(),
                ..Default::default()
            };
            let result = (|| -> std::result::Result<(), LaunchFailure> {
                let _pending = this
                    .pending_guard()
                    .map_err(|e| LaunchFailure::from_error(Stage::Create, "create_failed", &e))?;
                let def = this.inner.pools.get(&pool).ok_or_else(|| {
                    LaunchFailure::new(Stage::Resolve, "pool_unavailable", "Unknown pool.")
                })?;
                event.project = Some(def.project.clone());
                event.profile = def.profile.clone();
                let template = def.template.clone().ok_or_else(|| {
                    LaunchFailure::new(
                        Stage::Create,
                        "pool_has_no_template",
                        "Pool has no template.",
                    )
                })?;
                event.template = Some(template.clone());
                event.template_revision = this.template_revision(&host, &template);
                let size = def.autoscale.as_ref().and_then(|a| a.size.clone());
                let name = this.inner.pools.reserve_name(&pool).map_err(|e| {
                    LaunchFailure::from_error(Stage::Create, "name_unavailable", &e)
                })?;
                event.workspace = Some(name.clone());
                let hook = this.inner.hooks.create.lock().unwrap().clone();
                match hook {
                    Some(create) => create(&host, &name, &template, size.as_deref()),
                    None => {
                        let mut params = json!({"name":name,"template":template});
                        if let Some(size) = &size {
                            params["size"] = json!(size);
                        }
                        this.inner
                            .fleet
                            .operate(&host, "create", &params)
                            .map(|_| ())
                    }
                }
                .map_err(|e| match e.downcast::<LaunchFailure>() {
                    Ok(failure) => failure,
                    Err(e) => LaunchFailure::from_error(Stage::Create, "create_failed", &e),
                })?;
                this.inner
                    .pools
                    .add_created(
                        &pool,
                        Placement {
                            host_id: host.clone(),
                            name,
                        },
                    )
                    .map_err(|e| {
                        LaunchFailure::from_error(Stage::Bootstrap, "member_record_failed", &e)
                    })
            })();
            event.duration_ms = started.elapsed().as_millis() as u64;
            if let Err(failure) = &result {
                event.kind = "pool_member_create_failed".into();
                event.outcome = "failed".into();
                event.stage = failure.stage.as_str().into();
                event.reason = failure.reason.into();
                event.message = launch_events::clip_message(&failure.message, &BTreeMap::new());
            }
            let backoff_message = event.message.clone();
            this.record_launch(event);
            let mut sched = this.inner.sched.lock().unwrap();
            if let Some(n) = sched.creating.get_mut(&pool) {
                *n = n.saturating_sub(1);
            }
            match result {
                Ok(()) => {
                    sched.backoff.remove(&pool);
                }
                Err(_) => {
                    sched.backoff.insert(
                        pool.clone(),
                        (Instant::now() + CREATE_BACKOFF, backoff_message),
                    );
                }
            }
            sched.dirty = true;
            drop(sched);
            this.inner.wake.notify_all();
        });
    }
    /// Owner view of a pool: definition, members and their state, queue.
    pub fn pool_status(&self, name: &str) -> Result<Value> {
        let def = self
            .inner
            .pools
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("Unknown pool."))?;
        let sched = self.inner.sched.lock().unwrap();
        let leases = self.inner.leases.lock().unwrap();
        let members: Vec<Value> = def
            .members()
            .into_iter()
            .map(|m| {
                let busy = leases
                    .iter()
                    .any(|(h, n, _)| *h == m.host_id && *n == m.name);
                json!({"host_id":m.host_id,"name":m.name,"busy":busy})
            })
            .collect();
        let in_pool = |state: EntryState| {
            sched
                .entries
                .values()
                .filter(|q| q.entry.state == state && q.entry.pool() == Some(name))
                .count()
        };
        let mut view = def.public();
        view["member_status"] = json!(members);
        view["queued_runs"] = json!(in_pool(EntryState::Queued));
        view["running_runs"] = json!(in_pool(EntryState::Admitted));
        view["creating"] = json!(sched.creating.get(name).copied().unwrap_or(0));
        view["last_create_error"] = json!(sched.backoff.get(name).map(|(_, m)| m));
        Ok(json!({"pool":view}))
    }
    /// Every pool, plus `projects`: pool names grouped by project.
    pub fn list_pools(&self) -> Value {
        Self::pool_listing(self.inner.pools.all())
    }
    /// The pools of one project, in the same shape as [`Self::list_pools`].
    pub fn list_project_pools(&self, project: &str) -> Result<Value> {
        ensure!(pools::valid_project(project), "Invalid project.");
        let mut listing = Self::pool_listing(self.inner.pools.in_project(project));
        listing["project"] = json!(project);
        Ok(listing)
    }
    fn pool_listing(all: Vec<pools::Pool>) -> Value {
        let mut projects: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for pool in &all {
            projects
                .entry(pool.project.clone())
                .or_default()
                .push(pool.name.clone());
        }
        json!({
            "pools":all.iter().map(pools::Pool::public).collect::<Vec<_>>(),
            "projects":projects
                .into_iter()
                .map(|(project, pools)| json!({"project":project,"pools":pools}))
                .collect::<Vec<_>>(),
        })
    }
    /// Delete a pool with no queued or running runs. Its workspaces are kept.
    pub fn delete_pool(&self, name: &str) -> Result<Value> {
        let sched = self.inner.sched.lock().unwrap();
        ensure!(
            !sched
                .entries
                .values()
                .any(|q| q.entry.state != EntryState::Finished && q.entry.pool() == Some(name)),
            "This pool still has queued or running runs."
        );
        self.inner.pools.delete(name)?;
        Ok(json!({"deleted":name}))
    }
    /// Record a callback delivery attempt on the run's sidecar.
    pub fn record_callback(
        &self,
        id: &str,
        state: &str,
        attempts: u32,
        last_status: Option<u16>,
    ) -> Result<()> {
        ensure!(
            ["pending", "delivering", "delivered", "failed"].contains(&state),
            "Invalid callback state."
        );
        let mut details = self.inner.details.lock().unwrap();
        let detail = details
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("Unknown native run."))?;
        ensure!(detail["callback"].is_object(), "Run has no callback.");
        detail["callback"]["state"] = json!(state);
        detail["callback"]["attempts"] = json!(attempts);
        detail["callback"]["last_status"] = json!(last_status);
        detail["callback"]["updated_at"] = json!(crate::now());
        self.save_detail(id, detail)?;
        if ["delivered", "failed"].contains(&state) {
            self.inner.callbacks.lock().unwrap().remove(id);
        }
        Ok(())
    }
    fn save_record(&self, value: &Value) -> Result<()> {
        security::write_bytes(
            &self.inner.runs_root,
            &format!("{}.json", value["id"].as_str().unwrap_or("invalid")),
            &serde_json::to_vec(value)?,
            native::MAX_OUTPUT * 7,
        )
    }
    #[cfg(test)]
    fn run_worker(&self, host: Host, name: &str, request: native::RunRequest, id: &str) {
        let _ = self.run_worker_with(host, name, request, id, None, None);
    }
    /// Run the guest helper. A guest launch failure is returned (stage
    /// `start`) after the run is marked failed and uncertain.
    fn run_worker_with(
        &self,
        host: Host,
        name: &str,
        request: native::RunRequest,
        id: &str,
        resolved: Option<ResolvedProfile>,
        // The repository access and the checkout name, for repository runs.
        repository: Option<(repository::Access, Option<String>)>,
    ) -> std::result::Result<(), LaunchFailure> {
        let guest = || -> Result<Value> {
            self.inner
                .fleet
                .assert_admission(&host.id)
                .map_err(|error| {
                    LaunchFailure::from_error(Stage::Place, "managed_host_unavailable", &error)
                })?;
            // Runs without a profile keep the original guest command and
            // payload, so older guest helpers still serve them.
            let (mode, input) = match (&resolved, repository) {
                (p, Some((access, checkout))) => (
                    repository::GUEST_MODE,
                    serde_json::to_vec(&repository::Envelope {
                        request: request.clone(),
                        profile: p.as_ref().map(|p| repository::ProfilePart {
                            profile: p.name.clone(),
                            block: p.block.clone(),
                            files: p.files.clone(),
                        }),
                        repository: access,
                        checkout,
                    })?,
                ),
                (None, None) => ("run", serde_json::to_vec(&request)?),
                (Some(p), None) => (
                    "run-profile",
                    serde_json::to_vec(&profile::Envelope {
                        request: request.clone(),
                        profile: p.name.clone(),
                        block: p.block.clone(),
                        files: p.files.clone(),
                    })?,
                ),
            };
            let command = guest_command(
                &host,
                name,
                &[mode.into(), host.guest.workspace.clone(), id.into()],
            )?;
            let output = process::checked(
                &command,
                None,
                None,
                &input,
                request.timeout_seconds + 20,
                native::MAX_OUTPUT * 7,
            )?;
            let result: Value = serde_json::from_str(&output)?;
            native::validate_result(&result)?;
            Ok(result)
        };
        self.run_with(id, &request, guest)
    }
    /// Mark the run running (unless cancelled first), execute it, and record
    /// its terminal state. An execution error is returned as a launch failure
    /// for the caller to record: a [`LaunchFailure`] from the executor as is,
    /// anything else as a guest launch failure (stage `start`). The run is
    /// left unfinished in that case; the caller finishes it.
    fn run_with(
        &self,
        id: &str,
        request: &native::RunRequest,
        exec: impl FnOnce() -> Result<Value>,
    ) -> std::result::Result<(), LaunchFailure> {
        let outcome = (|| -> Result<Value> {
            if let Some(v) = self.inner.records.lock().unwrap().get_mut(id) {
                let mut details = self.inner.details.lock().unwrap();
                let detail = details
                    .get_mut(id)
                    .ok_or_else(|| anyhow::anyhow!("Missing run metadata."))?;
                if detail["cancel_requested"] == true {
                    return Ok(cancelled_result());
                }
                detail["started_at"] = json!(crate::now());
                self.save_detail(id, detail)?;
                v["status"] = json!("running");
                self.save_record(v)?;
            }
            exec()
        })();
        let result = match outcome {
            Ok(result) => result,
            Err(error) => {
                return Err(match error.downcast::<LaunchFailure>() {
                    Ok(failure) => failure,
                    Err(error) => {
                        LaunchFailure::from_error(Stage::Start, "guest_launch_failed", &error)
                    }
                });
            }
        };
        self.finish(id, |v| {
            v["status"] = json!(if result["exit_code"] == 0
                && result["timed_out"] == false
                && result["output_truncated"] == false
            {
                "succeeded"
            } else {
                "failed"
            });
            if let Some(session) = native::session_id(result["output"].as_str().unwrap_or(""))
                .or_else(|| request.resume_id.clone())
            {
                v["native_session_id"] = json!(session);
            }
            if result["exit_code"] == process::CANCELLED_EXIT_CODE
                && let Some(detail) = self.inner.details.lock().unwrap().get_mut(id)
            {
                detail["cancelled"] = json!(detail["cancel_requested"] == true);
                let _ = self.save_detail(id, detail);
            }
            v["result"] = result;
        });
        Ok(())
    }
    /// Apply the terminal state, persist, retain the newest finished runs and
    /// hand the run's callback (if any) to the deliverer, once.
    fn finish(&self, id: &str, terminal: impl FnOnce(&mut Value)) {
        let mut records = self.inner.records.lock().unwrap();
        if let Some(v) = records.get_mut(id) {
            v["finished_at"] = json!(crate::now());
            terminal(v);
            let _ = self.save_record(v);
        }
        let finished = records.get(id).and_then(|record| {
            let callback = self.inner.callbacks.lock().unwrap().get(id).cloned()?;
            let details = self.inner.details.lock().unwrap();
            Some(FinishedRun {
                id: id.to_owned(),
                callback,
                payload: callback_payload(record, details.get(id)),
            })
        });
        self.prune(&mut records, id);
        drop(records);
        if let Some(finished) = finished {
            self.deliver(finished);
        }
    }
    fn deliver(&self, finished: FinishedRun) {
        let hook = self.inner.finish_hook.lock().unwrap().clone();
        if let Some(hook) = hook {
            let _ = self.record_callback(&finished.id, "delivering", 0, None);
            hook(finished);
        }
    }
    /// An unplaced pool run ended: deliver its callback from the entry view.
    fn finish_unplaced(&self, entry: &Entry) {
        let callback = self.inner.callbacks.lock().unwrap().get(&entry.id).cloned();
        if let Some(callback) = callback {
            let details = self.inner.details.lock().unwrap();
            let view = entry_view(entry, details.get(&entry.id));
            let payload = callback_payload(&view, details.get(&entry.id));
            drop(details);
            self.deliver(FinishedRun {
                id: entry.id.clone(),
                callback,
                payload,
            });
        }
        // Retain the newest finished unplaced runs only.
        let stale: Vec<String> = {
            let mut sched = self.inner.sched.lock().unwrap();
            let mut finished: Vec<(String, String)> = sched
                .entries
                .values()
                .filter(|q| q.entry.state == EntryState::Finished)
                .map(|q| (q.entry.created_at.clone(), q.entry.id.clone()))
                .collect();
            finished.sort();
            let excess = finished.len().saturating_sub(RETAINED_FINISHED);
            let stale: Vec<String> = finished
                .into_iter()
                .take(excess)
                .map(|(_, id)| id)
                .collect();
            for id in &stale {
                sched.entries.remove(id);
            }
            stale
        };
        for id in stale {
            self.remove_entry_file(&id);
            self.inner.details.lock().unwrap().remove(&id);
            let _ = std::fs::remove_file(self.inner.details_root.join(format!("{id}.json")));
        }
    }
    fn prune(&self, records: &mut BTreeMap<String, Value>, keep: &str) {
        let terminal =
            |v: &Value| !["queued", "running"].contains(&v["status"].as_str().unwrap_or(""));
        while records.values().filter(|v| terminal(v)).count() > RETAINED_FINISHED {
            let oldest = records
                .iter()
                .filter(|(k, v)| {
                    terminal(v) && v["execution_uncertain"] != true && k.as_str() != keep
                })
                .min_by_key(|(_, v)| v["created_at"].as_str().unwrap_or(""))
                .map(|(k, _)| k.clone());
            let Some(key) = oldest else { break };
            records.remove(&key);
            self.inner.details.lock().unwrap().remove(&key);
            let _ = std::fs::remove_file(self.inner.details_root.join(format!("{key}.json")));
            let _ = std::fs::remove_file(self.inner.runs_root.join(format!("{key}.json")));
        }
    }
}

fn cancelled_result() -> Value {
    json!({"exit_code":process::CANCELLED_EXIT_CODE,"output":"","timed_out":false,"output_truncated":false})
}
fn strip_env(request: &mut Value) {
    if let Some(object) = request.as_object_mut() {
        object.remove("env");
    }
}
/// The public view of a pool run that has no workspace record.
fn entry_view(entry: &Entry, detail: Option<&Value>) -> Value {
    let status = match entry.state {
        EntryState::Finished => "failed",
        EntryState::Admitted => "running",
        EntryState::Queued => "queued",
    };
    let mut record = json!({
        "id":entry.id,
        "host_id":entry.placement.as_ref().map(|p| &p.host_id),
        "name":entry.placement.as_ref().map(|p| &p.name),
        "provider":entry.provider(),
        "scope":"controller-owner",
        "state_scope":"private-workspace",
        "status":status,
        "workdir":entry.request["workdir"],
        "resume_id":entry.request["resume_id"],
        "created_at":entry.created_at,
    });
    if let Some(at) = &entry.finished_at {
        record["finished_at"] = json!(at);
    }
    if entry.outcome.as_deref() == Some("failed") {
        record["error"] = json!("The run could not be placed.");
    }
    run_details::enrich(&record, detail, chrono::Utc::now())
}

/// The signed callback body: identity, status and timing only. Never output,
/// prompts, env names or values, or error text.
pub fn callback_payload(record: &Value, detail: Option<&Value>) -> Value {
    let run = run_details::enrich(record, detail, chrono::Utc::now());
    json!({
        "schema":1,
        "event":"agent_run.finished",
        "run_id":run["id"],
        "host_id":run["host_id"],
        "name":run["name"],
        "provider":run["provider"],
        "profile":detail.and_then(|d| d["profile"]["name"].as_str()),
        "pool":detail.and_then(|d| d["pool"].as_str()),
        "repository":detail.and_then(|d| d["repository"].as_str()),
        "status":run["status"],
        "exit_code":run["result"]["exit_code"],
        "timed_out":run["result"]["timed_out"],
        "execution_uncertain":run["execution_uncertain"] == true,
        // Codes only (`{stage, reason}`), never the failure message.
        "failure":detail.and_then(|d| d.get("failure")).map(|f| json!({"stage":f["stage"],"reason":f["reason"]})),
        "created_at":run["created_at"],
        "started_at":run["started_at"],
        "finished_at":run["finished_at"],
    })
}
fn active_session(v: &Value) -> bool {
    ["starting", "waiting"].contains(&v["status"].as_str().unwrap_or(""))
}
fn receipt_key(host: &str, name: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(format!("{host}:{name}")))
}
fn host_binding(host: &Host) -> Result<String> {
    Ok(security::legacy_digest(
        &json!({"target":host.target,"project":host.project,"pool":host.pool,"guest":host.guest}),
    ))
}
/// The durable record and sidecar for a new run. The record keeps its legacy
/// shape so retained releases can still read it; the principal and env key
/// names live in the sidecar. Env values never enter either.
pub fn new_run_entries(
    id: &str,
    host_id: &str,
    name: &str,
    request: &native::RunRequest,
    started_by: &str,
) -> (Value, Value) {
    let record = json!({"id":id,"host_id":host_id,"name":name,"provider":request.provider,"scope":"controller-owner","state_scope":"private-workspace","status":"queued","workdir":request.workdir,"resume_id":request.resume_id,"created_at":crate::now()});
    let detail = json!({"schema":1,"task":run_details::task_preview(&request.prompt),"started_at":null,"cancel_requested":false,"cancelled":false,"started_by":started_by,"env_keys":request.env_keys()});
    (record, detail)
}
/// What `enginebox-guest --version` prints for this release.
pub fn expected_guest_version() -> String {
    format!("enginebox-guest {}", env!("CARGO_PKG_VERSION"))
}
/// This controller release's `bin` directory, when the host is this machine
/// and the release ships the guest helpers there. The host helper copies
/// from it (it accepts only root-owned, non-writable sources), so a host
/// helper linked to an older release still installs this release's guest.
fn release_bin_dir(host: &Host) -> Option<std::path::PathBuf> {
    if host.target != "local" {
        return None;
    }
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    ["enginebox-host", "enginebox-guest", "enginebox-mcp"]
        .iter()
        .all(|b| dir.join(b).is_file())
        .then_some(dir)
}
/// What placement found and did about a workspace's guest helper.
#[derive(Debug, PartialEq)]
pub struct GuestRefresh {
    pub expected: String,
    /// `--version` before any refresh (`None`: it could not be read).
    pub before: Option<String>,
    /// The refresh error, when one was attempted and failed.
    pub sync_error: Option<String>,
    /// `--version` after a refresh, when one was attempted.
    pub after: Option<String>,
    pub current: bool,
}
impl GuestRefresh {
    /// The launch event message, when a refresh was attempted.
    pub fn event(&self) -> Option<String> {
        if self.before.as_deref() == Some(self.expected.as_str()) {
            return None;
        }
        let found = |v: &Option<String>| v.clone().unwrap_or_else(|| "unreadable".into());
        Some(if self.current {
            format!(
                "Refreshed the workspace's guest helper from {} to {}.",
                found(&self.before),
                self.expected
            )
        } else {
            let why = match &self.sync_error {
                Some(error) => format!("the refresh failed: {error}"),
                None => "the host helper supplied an older release; update the host's \
                    enginebox-host (enginebox-install expose-host --apply)"
                    .into(),
            };
            format!(
                "Workspace guest helper is {} but this controller needs {}; {why}.",
                found(if self.after.is_some() {
                    &self.after
                } else {
                    &self.before
                }),
                self.expected
            )
        })
    }
}
/// Compare the guest helper's version with `expected` and refresh it once
/// when they differ.
pub fn refresh_guest(
    expected: &str,
    probe: impl Fn() -> Result<String>,
    sync: impl FnOnce() -> Result<()>,
) -> GuestRefresh {
    let before = probe().ok();
    let mut outcome = GuestRefresh {
        expected: expected.to_owned(),
        current: before.as_deref() == Some(expected),
        before,
        sync_error: None,
        after: None,
    };
    if outcome.current {
        return outcome;
    }
    if let Err(error) = sync() {
        outcome.sync_error = Some(format!("{error:#}"));
        return outcome;
    }
    outcome.after = probe().ok();
    outcome.current = outcome.after.as_deref() == Some(expected);
    outcome
}
pub fn guest_command(host: &Host, name: &str, args: &[String]) -> Result<Vec<String>> {
    let mut argv = vec![
        "exec".into(),
        name.into(),
        "--mode=non-interactive".into(),
        format!("--user={}", host.guest.uid),
        format!("--group={}", host.guest.gid),
        format!("--cwd={}", host.guest.workspace),
        format!("--env=HOME={}", host.guest.home),
        format!("--env=USER={}", host.guest.user),
        format!("--env=DISPLAY={}", host.guest.display),
        "--".into(),
        "/usr/local/bin/enginebox-guest".into(),
    ];
    argv.extend_from_slice(args);
    Ok(host.engine_box().command(&argv))
}
pub fn validate_record(v: &Value, id: &str) -> Result<()> {
    let required = [
        "id",
        "host_id",
        "name",
        "provider",
        "scope",
        "state_scope",
        "status",
        "workdir",
        "resume_id",
        "created_at",
    ];
    let optional = [
        "finished_at",
        "result",
        "native_session_id",
        "error",
        "execution_uncertain",
    ];
    ensure!(
        v.as_object()
            .is_some_and(|map| required.iter().all(|key| map.contains_key(*key))
                && map.keys().all(
                    |key| required.contains(&key.as_str()) || optional.contains(&key.as_str())
                )),
        "Invalid native run record fields."
    );
    ensure!(
        v.is_object()
            && v["id"] == id
            && v["scope"] == "controller-owner"
            && v["state_scope"] == "private-workspace"
            && ["queued", "running", "failed", "succeeded"]
                .contains(&v["status"].as_str().unwrap_or("")),
        "Invalid native run record."
    );
    enginebox_core::identifier(v["host_id"].as_str().unwrap_or(""), false)?;
    enginebox_core::identifier(v["name"].as_str().unwrap_or(""), true)?;
    native::RunRequest::parse(
        json!({"provider":v["provider"],"prompt":"record validation","workdir":v["workdir"],"resume_id":v["resume_id"]}),
    )?;
    for key in ["created_at", "finished_at"] {
        if let Some(s) = v[key].as_str() {
            chrono::DateTime::parse_from_rfc3339(s)?;
        } else if key == "created_at" || !v[key].is_null() {
            bail!("Invalid native run timestamp.")
        }
    }
    if let Some(result) = v.get("result") {
        native::validate_result(result)?;
    }
    if let Some(session) = v["native_session_id"].as_str() {
        ensure!(security::exact_uuid(session), "Invalid native session ID.");
    }
    ensure!(
        v["native_session_id"].is_null() || v["native_session_id"].is_string(),
        "Invalid native session ID."
    );
    if let Some(error) = v.get("error") {
        ensure!(
            error.as_str().is_some_and(|s| s.len() <= 1024),
            "Invalid native run error."
        );
    }
    if let Some(uncertain) = v.get("execution_uncertain") {
        ensure!(uncertain.is_boolean(), "Invalid native recovery state.");
    }
    Ok(())
}

#[cfg(test)]
mod run_control_tests {
    use super::*;

    #[test]
    fn a_current_guest_helper_is_left_alone() {
        let expected = expected_guest_version();
        let out = refresh_guest(&expected, || Ok(expected.clone()), || panic!("no refresh"));
        assert!(out.current);
        assert_eq!(out.event(), None);
    }

    #[test]
    fn a_stale_guest_helper_is_refreshed_once() {
        let expected = "enginebox-guest 0.3.1-live.42";
        let version = Mutex::new("enginebox-guest 0.3.1-live.40".to_owned());
        let syncs = AtomicUsize::new(0);
        let out = refresh_guest(
            expected,
            || Ok(version.lock().unwrap().clone()),
            || {
                syncs.fetch_add(1, Ordering::SeqCst);
                *version.lock().unwrap() = expected.into();
                Ok(())
            },
        );
        assert_eq!(syncs.load(Ordering::SeqCst), 1);
        assert!(out.current);
        assert_eq!(
            out.event().unwrap(),
            "Refreshed the workspace's guest helper from enginebox-guest 0.3.1-live.40 to enginebox-guest 0.3.1-live.42."
        );
    }

    #[test]
    fn a_guest_helper_that_stays_stale_is_reported() {
        let expected = "enginebox-guest 0.3.1-live.42";
        // The host helper's own release is older: the copy changes nothing.
        let out = refresh_guest(
            expected,
            || Ok("enginebox-guest 0.3.1-live.10".into()),
            || Ok(()),
        );
        assert!(!out.current);
        let event = out.event().unwrap();
        assert!(
            event.starts_with("Workspace guest helper is enginebox-guest 0.3.1-live.10 but this controller needs enginebox-guest 0.3.1-live.42; the host helper supplied an older release"),
            "{event}"
        );
        // A failed refresh says so, and an unreadable helper is named as such.
        let out = refresh_guest(
            expected,
            || anyhow::bail!("exec failed"),
            || anyhow::bail!("sudo: a password is required"),
        );
        assert!(!out.current);
        assert_eq!(out.before, None);
        assert!(
            out.event()
                .unwrap()
                .contains("is unreadable but this controller needs enginebox-guest 0.3.1-live.42; the refresh failed: sudo: a password is required"),
            "{:?}",
            out.event()
        );
    }

    #[test]
    fn managed_host_fences_delegation_and_last_moment_guest_execution() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("private");
        let host = Host {
            id: "fenced-node".into(),
            target: "must-not-contact.invalid".into(),
            managed_registration: true,
            ..Default::default()
        };
        let fleet = Fleet::from_hosts(vec![host.clone()], &root).unwrap();
        let service = AgentService::new(&root, fleet.clone()).unwrap();
        for result in [
            service.attach("fenced-node", "ebox-private", None),
            service.attach_after_start("fenced-node", "ebox-private"),
            service.adopt("fenced-node", "ebox-private", "codex"),
        ] {
            assert!(result.unwrap_err().to_string().contains("registration"));
        }
        // Execution can have been queued while the lease was live. The final
        // guest launch rechecks after provider preparation has completed.
        fleet.set_admission_guard(Arc::new(|_| anyhow::bail!("Node revoked")));
        let request =
            native::RunRequest::parse(json!({"provider":"codex","prompt":"never sent"})).unwrap();
        let failure = service
            .run_worker_with(
                host,
                "ebox-private",
                request,
                "11111111111111111111111111111111",
                None,
                None,
            )
            .unwrap_err();
        assert_eq!(failure.stage, Stage::Place);
        assert_eq!(failure.reason, "managed_host_unavailable");
        assert!(launch_events::valid_reason(failure.reason));
    }

    #[test]
    fn queued_cancellation_is_scoped_persistent_and_never_starts_guest() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let host = Host {
            id: "test-node".into(),
            target: "offline.invalid".into(),
            ..Default::default()
        };
        let fleet = Fleet::from_hosts(vec![host.clone()], &root).unwrap();
        let service = AgentService::new(&root, fleet.clone()).unwrap();
        let id = "11111111111111111111111111111111";
        let record = json!({"id":id,"host_id":"test-node","name":"ebox-private","provider":"codex","scope":"controller-owner","state_scope":"private-workspace","status":"queued","workdir":".","resume_id":null,"created_at":crate::now()});
        let detail = json!({"schema":1,"task":"fixture task","started_at":null,"cancel_requested":false,"cancelled":false});
        service
            .inner
            .records
            .lock()
            .unwrap()
            .insert(id.into(), record);
        service
            .inner
            .details
            .lock()
            .unwrap()
            .insert(id.into(), detail);
        assert!(
            service
                .cancel_run("other-node", "ebox-private", id)
                .is_err()
        );
        assert!(service.cancel_run("test-node", "ebox-other", id).is_err());
        assert_eq!(
            service.get_run("test-node", "ebox-private", id).unwrap()["cancel_requested"],
            false
        );
        let requested = service.cancel_run("test-node", "ebox-private", id).unwrap();
        assert_eq!(requested["status"], "queued");
        assert_eq!(requested["cancel_requested"], true);
        let request =
            native::RunRequest::parse(json!({"provider":"codex","prompt":"never sent"})).unwrap();
        service.run_worker(host, "ebox-private", request, id);
        let completed = service.get_run("test-node", "ebox-private", id).unwrap();
        assert_eq!(completed["status"], "cancelled");
        assert!(completed["started_at"].is_null());
        assert!(completed["elapsed_seconds"].is_null());
        assert_eq!(
            completed["result"]["exit_code"],
            process::CANCELLED_EXIT_CODE
        );
        let raw = security::read_json(&service.inner.runs_root, &format!("{id}.json"))
            .unwrap()
            .unwrap();
        // A retained controller release can still read and safely classify the record.
        validate_record(&raw, id).unwrap();
        assert_eq!(raw["status"], "failed");
        let restarted = AgentService::new(&root, fleet).unwrap();
        assert_eq!(
            restarted.get_run("test-node", "ebox-private", id).unwrap()["status"],
            "cancelled"
        );
        assert_eq!(restarted.list_runs()["runs"][0]["task"], "fixture task");
    }

    fn fixture() -> (tempfile::TempDir, AgentService, Host) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let host = Host {
            id: "test-node".into(),
            target: "offline.invalid".into(),
            ..Default::default()
        };
        let fleet = Fleet::from_hosts(vec![host.clone()], &root).unwrap();
        (temp, AgentService::new(&root, fleet).unwrap(), host)
    }
    fn insert(service: &AgentService, id: &str, detail: Value) {
        let record = json!({"id":id,"host_id":"test-node","name":"ebox-private","provider":"codex","scope":"controller-owner","state_scope":"private-workspace","status":"queued","workdir":".","resume_id":null,"created_at":crate::now()});
        service
            .inner
            .records
            .lock()
            .unwrap()
            .insert(id.into(), record);
        service
            .inner
            .details
            .lock()
            .unwrap()
            .insert(id.into(), detail);
    }

    #[test]
    fn a_repeated_idempotency_key_returns_the_same_run_for_the_same_principal_only() {
        let (_t, service, _host) = fixture();
        let id = "22222222222222222222222222222222";
        let body = json!({"provider":"codex","prompt":"build it","env":{"TOKEN":"secret-value-1"}});
        let request = native::RunRequest::parse(body.clone()).unwrap();
        let options = LaunchOptions {
            idempotency_key: Some("job-42".into()),
            ..Default::default()
        };
        let (_, mut detail) = new_run_entries(id, "test-node", "ebox-private", &request, "owner");
        detail["idempotency"] = json!({
            "key_digest":idempotency_key_digest("owner", "job-42"),
            "request_digest":idempotency_request_digest(&request, &options),
        });
        run_details::validate(&detail).unwrap();
        // The stored digests never contain env values.
        assert!(!detail.to_string().contains("secret-value-1"));
        insert(&service, id, detail);
        // Same principal, key and request: the existing run, no new launch
        // (the offline fixture host would refuse a real one).
        let (run, replayed) = service
            .launch_with(
                "test-node",
                "ebox-private",
                body.clone(),
                "owner",
                options.clone(),
            )
            .unwrap();
        assert!(replayed);
        assert_eq!(run["id"], id);
        // A changed env value is the same request (values are never hashed).
        let same = json!({"provider":"codex","prompt":"build it","env":{"TOKEN":"rotated-value"}});
        assert!(
            service
                .launch_with("test-node", "ebox-private", same, "owner", options.clone())
                .unwrap()
                .1
        );
        // The same key with a different request is refused.
        let changed = json!({"provider":"codex","prompt":"something else"});
        let error = service
            .launch_with(
                "test-node",
                "ebox-private",
                changed,
                "owner",
                options.clone(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("different request"));
        // Another workspace with the same key is refused, not silently replayed.
        assert!(
            service
                .launch_with(
                    "test-node",
                    "ebox-other",
                    body.clone(),
                    "owner",
                    options.clone()
                )
                .is_err()
        );
        // Another principal's identical key is a different key: it reaches the
        // real launch path (which fails offline) instead of replaying.
        let other = format!("service-token:{}", "a".repeat(32));
        let error = service
            .launch_with(
                "test-node",
                "ebox-private",
                body.clone(),
                &other,
                options.clone(),
            )
            .unwrap_err();
        assert!(!error.to_string().contains("different request"));
        assert_eq!(service.inner.records.lock().unwrap().len(), 1);
        for bad in ["", "has space", &"k".repeat(129), "semi;colon"] {
            let options = LaunchOptions {
                idempotency_key: Some(bad.into()),
                ..Default::default()
            };
            assert!(
                service
                    .launch_with("test-node", "ebox-private", body.clone(), "owner", options)
                    .is_err()
            );
        }
    }

    #[test]
    fn a_terminal_run_hands_its_callback_a_summary_without_output_or_env() {
        let (_t, service, host) = fixture();
        let id = "33333333333333333333333333333333";
        let request = native::RunRequest::parse(
            json!({"provider":"codex","prompt":"private prompt text","env":{"TOKEN":"secret-value-2"}}),
        )
        .unwrap();
        let (_, mut detail) = new_run_entries(id, "test-node", "ebox-private", &request, "owner");
        detail["cancel_requested"] = json!(true);
        detail["profile"] = json!({"name":"verifier","revision":3});
        detail["callback"] = json!({"origin":"https://hooks.example.com","state":"pending","attempts":0,"last_status":null,"updated_at":crate::now()});
        run_details::validate(&detail).unwrap();
        insert(&service, id, detail);
        let key = "k".repeat(40);
        service.inner.callbacks.lock().unwrap().insert(
            id.into(),
            Callback {
                url: "https://hooks.example.com/runs?token=abc".into(),
                secret: key.clone(),
            },
        );
        let seen: Arc<Mutex<Vec<FinishedRun>>> = Arc::default();
        let sink = seen.clone();
        service.set_finish_hook(Arc::new(move |finished| {
            sink.lock().unwrap().push(finished)
        }));
        service.run_worker(host, "ebox-private", request, id);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let payload = &seen[0].payload;
        assert_eq!(payload["run_id"], id);
        assert_eq!(payload["status"], "cancelled");
        assert_eq!(payload["profile"], "verifier");
        assert_eq!(payload["exit_code"], process::CANCELLED_EXIT_CODE);
        let text = payload.to_string();
        for forbidden in [
            "secret-value-2",
            "TOKEN",
            "private prompt",
            "output",
            key.as_str(),
            "token=abc",
        ] {
            assert!(!text.contains(forbidden), "payload leaked {forbidden}");
        }
        assert!(text.len() < 1024);
        assert!(!format!("{:?}", seen[0]).contains(&key));
        // The run records the delivery state and only the callback origin.
        let run = service.get_run("test-node", "ebox-private", id).unwrap();
        assert_eq!(run["callback"]["state"], "delivering");
        assert_eq!(run["callback"]["origin"], "https://hooks.example.com");
        service
            .record_callback(id, "delivered", 1, Some(204))
            .unwrap();
        let run = service.get_run("test-node", "ebox-private", id).unwrap();
        assert_eq!(run["callback"]["state"], "delivered");
        assert_eq!(run["callback"]["last_status"], 204);
        assert!(service.inner.callbacks.lock().unwrap().is_empty());
        assert!(!run.to_string().contains(&key));
        let raw =
            std::fs::read_to_string(service.inner.details_root.join(format!("{id}.json"))).unwrap();
        assert!(
            !raw.contains(&key) && !raw.contains("token=abc") && !raw.contains("secret-value-2")
        );
    }
}
