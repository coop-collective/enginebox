use crate::{auth, delegation, native, process, profile, run_details, security, usage};
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
};
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
    workers: (Mutex<usize>, Condvar),
    pending: AtomicUsize,
    closed: AtomicBool,
    usage: usage::Cache,
    status_cache: Mutex<BTreeMap<String, (std::time::Instant, Value)>>,
    receipt_lock: Mutex<()>,
    /// Callback targets and keys of runs still in flight. Memory only: the
    /// key never reaches a record, and a restart ends delivery for that run.
    callbacks: Mutex<BTreeMap<String, Callback>>,
    finish_hook: Mutex<Option<FinishHook>>,
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
/// What a replay must match: the request with env values replaced by names
/// (values are secrets and are never hashed to disk), plus the v2 choices.
fn idempotency_request_digest(request: &native::RunRequest, options: &LaunchOptions) -> String {
    security::digest(&json!({
        "provider":request.provider,
        "prompt":request.prompt,
        "resume_id":request.resume_id,
        "workdir":request.workdir,
        "timeout_seconds":request.timeout_seconds,
        "model":request.model,
        "env_keys":request.env_keys(),
        "profile":options.profile.as_ref().map(|p| &p.name),
        "callback":options.callback.as_ref().map(|c| &c.url),
    }))
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
            if ["running", "queued"].contains(&v["status"].as_str().unwrap_or("")) {
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
        let mut details = BTreeMap::new();
        for id in records.keys() {
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
                workers: (Mutex::new(0), Condvar::new()),
                pending: AtomicUsize::new(0),
                closed: AtomicBool::new(false),
                usage: usage::Cache::default(),
                status_cache: Mutex::new(BTreeMap::new()),
                receipt_lock: Mutex::new(()),
                callbacks: Mutex::new(BTreeMap::new()),
                finish_hook: Mutex::new(None),
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
        let host = self.inner.fleet.account_workspace(host_id, name)?;
        let _hostlock = enginebox_core::operation_lock(&host)?;
        let _lease = self.provider_lock(p)?;
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
            let result = (|| -> Result<bool> {
                let host = self.inner.fleet.host(host_id)?;
                let p = value["provider"].as_str().unwrap_or("");
                let command = guest_command(&host, name, &["lease".into(), p.into()])?;
                let output = process::checked(&command, None, None, b"", 15, 1024)?;
                let v: Value = serde_json::from_str(&output)?;
                v["active"]
                    .as_bool()
                    .ok_or_else(|| anyhow::anyhow!("Invalid lease probe."))
            })();
            if result.unwrap_or(true) {
                return true;
            }
            if let Some(v) = self
                .inner
                .records
                .lock()
                .unwrap()
                .get_mut(value["id"].as_str().unwrap_or(""))
            {
                v["execution_uncertain"] = json!(false);
                if self.save_record(v).is_err() {
                    return true;
                }
            }
        }
        false
    }
    pub fn get_run(&self, host_id: &str, name: &str, id: &str) -> Result<Value> {
        ensure!(
            regex::Regex::new(r"^[a-f0-9]{32}$")?.is_match(id),
            "Invalid native run ID."
        );
        let records = self.inner.records.lock().unwrap();
        let record = records
            .get(id)
            .filter(|v| v["host_id"] == host_id && v["name"] == name)
            .ok_or_else(|| anyhow::anyhow!("Unknown native run for this workspace."))?;
        Ok(run_details::enrich(
            record,
            self.inner.details.lock().unwrap().get(id),
            chrono::Utc::now(),
        ))
    }
    pub fn list_runs(&self) -> Value {
        self.list_runs_where(|_, _| true)
    }
    /// List only runs whose (host, workspace) the caller may see.
    pub fn list_runs_where(&self, visible: impl Fn(&str, &str) -> bool) -> Value {
        let records = self.inner.records.lock().unwrap();
        let details = self.inner.details.lock().unwrap();
        let mut summaries: Vec<_> = records
            .iter()
            .filter(|(_, record)| {
                visible(
                    record["host_id"].as_str().unwrap_or(""),
                    record["name"].as_str().unwrap_or(""),
                )
            })
            .map(|(id, record)| run_details::summary(record, details.get(id)))
            .collect();
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
    pub fn cancel_run(&self, host_id: &str, name: &str, id: &str) -> Result<Value> {
        native::validate_run_id(id)?;
        let _pending = self.pending_guard()?;
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
        {
            // Pair notification with the queue mutex so a cancellation between
            // the worker's predicate check and wait cannot be lost.
            let _workers = self.inner.workers.0.lock().unwrap();
            self.inner.workers.1.notify_all();
        }
        // The same records mutex guards queued->running. A queued cancellation
        // is observed before launch; a running cancellation may arrive before
        // the remote helper and remains recorded until that exact run observes it.
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
    /// Register the controller's callback deliverer.
    pub fn set_finish_hook(&self, hook: FinishHook) {
        *self.inner.finish_hook.lock().unwrap() = Some(hook);
    }
    /// The run an idempotency key already names for this principal, if any.
    fn replay(
        &self,
        started_by: &str,
        key: &str,
        request_digest: &str,
    ) -> Result<Option<(String, String, String)>> {
        let key_digest = idempotency_key_digest(started_by, key);
        let details = self.inner.details.lock().unwrap();
        let Some((id, detail)) = details
            .iter()
            .find(|(_, d)| d["idempotency"]["key_digest"] == key_digest.as_str())
        else {
            return Ok(None);
        };
        ensure!(
            detail["idempotency"]["request_digest"] == request_digest,
            "idempotency_key was already used with a different request."
        );
        let records = self.inner.records.lock().unwrap();
        let record = records
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("Missing run for idempotency key."))?;
        Ok(Some((
            record["host_id"].as_str().unwrap_or("").to_owned(),
            record["name"].as_str().unwrap_or("").to_owned(),
            id.clone(),
        )))
    }
    /// Launch with the optional v2 features. Returns the run and whether it is
    /// an existing run returned for a repeated idempotency key.
    pub fn launch_with(
        &self,
        host_id: &str,
        name: &str,
        value: Value,
        started_by: &str,
        options: LaunchOptions,
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
        if let Some(key) = &options.idempotency_key {
            ensure!(
                valid_idempotency_key(key),
                "idempotency_key must be 1-128 characters of A-Z a-z 0-9 . _ : -"
            );
        }
        let request_digest = idempotency_request_digest(&request, &options);
        if let Some(key) = &options.idempotency_key
            && let Some((h, n, id)) = self.replay(started_by, key, &request_digest)?
        {
            ensure!(
                h == host_id && n == name,
                "idempotency_key was already used with a different request."
            );
            return Ok((self.get_run(&h, &n, &id)?, true));
        }
        let pending = self.pending_guard()?;
        let host = self.inner.fleet.account_workspace(host_id, name)?;
        if let Some(template) = options.profile.as_ref().and_then(|p| p.template.as_ref()) {
            ensure!(
                self.inner
                    .fleet
                    .workspace_template(host_id, name)?
                    .as_deref()
                    == Some(template.as_str()),
                "This workspace was not created from the profile's template."
            );
        }
        let _hostlock = enginebox_core::operation_lock(&host)?;
        let key = (
            host_id.to_owned(),
            name.to_owned(),
            request.provider.clone(),
        );
        let uncertain = self.inner.records.lock().unwrap().values().any(|v| {
            v["execution_uncertain"] == true && v["host_id"] == host_id && v["name"] == name
        });
        if uncertain && self.active(host_id, name) {
            bail!("Previous native run must release its guest lease.")
        }
        let attached = self.attach_locked(
            host_id,
            name,
            Some(vec![request.provider.clone()]),
            None,
            None,
        )?;
        ensure!(
            attached["providers"]
                .as_array()
                .is_some_and(|items| items.len() == 1
                    && items.iter().all(|item| ["attached", "unchanged"]
                        .contains(&item["status"].as_str().unwrap_or("")))),
            "Connect this agent on the controller before launching a session."
        );
        let mut leases = self.inner.leases.lock().unwrap();
        // Checked again under the queue lock, which also guards insertion, so
        // two concurrent launches with one key cannot both start a run.
        if let Some(key) = &options.idempotency_key
            && let Some((h, n, id)) = self.replay(started_by, key, &request_digest)?
        {
            drop(leases);
            ensure!(
                h == host_id && n == name,
                "idempotency_key was already used with a different request."
            );
            return Ok((self.get_run(&h, &n, &id)?, true));
        }
        ensure!(
            !leases.contains(&key),
            "This workspace provider already has an active native run."
        );
        ensure!(leases.len() < 16, "Native run queue is full.");
        let id = uuid::Uuid::new_v4().simple().to_string();
        let (value, mut detail) = new_run_entries(&id, host_id, name, &request, started_by);
        if let Some(resolved) = &options.profile {
            detail["profile"] = json!({"name":resolved.name,"revision":resolved.revision});
        }
        if let Some(key) = &options.idempotency_key {
            detail["idempotency"] = json!({
                "key_digest":idempotency_key_digest(started_by, key),
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
            self.inner
                .callbacks
                .lock()
                .unwrap()
                .insert(id.clone(), callback.clone());
        }
        self.save_detail(&id, &detail)?;
        self.save_record(&value)?;
        self.inner
            .details
            .lock()
            .unwrap()
            .insert(id.clone(), detail);
        leases.insert(key.clone());
        self.inner
            .records
            .lock()
            .unwrap()
            .insert(id.clone(), value.clone());
        let this = self.clone();
        let name = name.to_owned();
        let resolved = options.profile;
        thread::spawn(move || {
            let _pending = pending;
            let mut workers = this.inner.workers.0.lock().unwrap();
            while *workers >= 4 && !this.cancel_requested(&id) {
                workers = this.inner.workers.1.wait(workers).unwrap();
            }
            let admitted = !this.cancel_requested(&id);
            if admitted {
                *workers += 1;
            }
            drop(workers);
            this.run_worker_with(host, &name, request, &id, resolved);
            this.inner.leases.lock().unwrap().remove(&key);
            if admitted {
                let mut workers = this.inner.workers.0.lock().unwrap();
                *workers -= 1;
            }
            this.inner.workers.1.notify_one();
        });
        Ok((
            self.get_run(
                host_id,
                value["name"].as_str().unwrap_or(""),
                value["id"].as_str().unwrap_or(""),
            )?,
            false,
        ))
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
    fn cancel_requested(&self, id: &str) -> bool {
        self.inner
            .details
            .lock()
            .unwrap()
            .get(id)
            .is_some_and(|detail| detail["cancel_requested"] == true)
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
        self.run_worker_with(host, name, request, id, None)
    }
    fn run_worker_with(
        &self,
        host: Host,
        name: &str,
        request: native::RunRequest,
        id: &str,
        resolved: Option<ResolvedProfile>,
    ) {
        let outcome = (|| -> Result<Value> {
            if let Some(v) = self.inner.records.lock().unwrap().get_mut(id) {
                let mut details = self.inner.details.lock().unwrap();
                let detail = details
                    .get_mut(id)
                    .ok_or_else(|| anyhow::anyhow!("Missing run metadata."))?;
                if detail["cancel_requested"] == true {
                    return Ok(
                        json!({"exit_code":process::CANCELLED_EXIT_CODE,"output":"","timed_out":false,"output_truncated":false}),
                    );
                }
                detail["started_at"] = json!(crate::now());
                self.save_detail(id, detail)?;
                v["status"] = json!("running");
                self.save_record(v)?;
            }
            // Runs without a profile keep the original guest command and
            // payload, so older guest helpers still serve them.
            let (mode, input) = match &resolved {
                None => ("run", serde_json::to_vec(&request)?),
                Some(p) => (
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
        })();
        let mut records = self.inner.records.lock().unwrap();
        if let Some(v) = records.get_mut(id) {
            v["finished_at"] = json!(crate::now());
            match outcome {
                Ok(result) => {
                    v["status"] = json!(if result["exit_code"] == 0
                        && result["timed_out"] == false
                        && result["output_truncated"] == false
                    {
                        "succeeded"
                    } else {
                        "failed"
                    });
                    if let Some(session) =
                        native::session_id(result["output"].as_str().unwrap_or(""))
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
                }
                Err(_) => {
                    v["status"] = json!("failed");
                    v["execution_uncertain"] = json!(true);
                    v["error"] = json!(
                        "Native launch failed; inspect installation and the workspace session lease."
                    );
                }
            }
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
        while records.len() > 100 {
            let oldest = records
                .iter()
                .filter(|(_, v)| {
                    !["queued", "running"].contains(&v["status"].as_str().unwrap_or(""))
                        && v["execution_uncertain"] != true
                })
                .min_by_key(|(_, v)| v["created_at"].as_str().unwrap_or(""))
                .map(|(k, _)| k.clone());
            let Some(key) = oldest else { break };
            records.remove(&key);
            self.inner.details.lock().unwrap().remove(&key);
            let _ = std::fs::remove_file(self.inner.details_root.join(format!("{key}.json")));
            let _ = std::fs::remove_file(self.inner.runs_root.join(format!("{key}.json")));
        }
        drop(records);
        if let Some(finished) = finished {
            let hook = self.inner.finish_hook.lock().unwrap().clone();
            if let Some(hook) = hook {
                let _ = self.record_callback(&finished.id, "delivering", 0, None);
                hook(finished);
            }
        }
    }
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
        "status":run["status"],
        "exit_code":run["result"]["exit_code"],
        "timed_out":run["result"]["timed_out"],
        "execution_uncertain":run["execution_uncertain"] == true,
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
