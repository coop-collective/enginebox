//! Org agent credentials (docs/ORGS.md, "Agent credentials").
//!
//! Every credential belongs to one org and one provider (`codex`, `claude`,
//! `cursor`):
//!
//! - `personal`: a member's own subscription, connected under their identity
//!   with the provider's native sign-in into a private home of its own. Only
//!   the owner may connect it, and only the owner decides whether it is shared
//!   into the org's pools (`share_with_org_pools`).
//! - `team`: an org-owned subscription (a team or service account), connected
//!   the same way by an org admin.
//! - `api_key`: an org-owned API key. The key is write-only: stored in a
//!   private file, never returned or logged.
//!
//! The org's controller-scope accounts (`GET /v1/agents`) remain the platform
//! org's team accounts. Runs do not select from these records yet:
//! [`Credentials::candidates`] is the selection interface the credential pool
//! work will call, returning eligible credentials in priority order.
//!
//! A member's **Unity account** (provider `unity`, kind `personal` only) is a
//! username and password, at most one per member per org. It is checked with
//! Unity when saved ([`crate::unity`]) and only ever used for its owner's own
//! sessions ([`Credentials::unity_for`], [`Credentials::unity_access_token`]).
//!
//! Records live under `agent-credentials/<id>.json`, homes under
//! `agent-credentials/homes/<id>/`, keys under `agent-credentials/keys/<id>.json`.
//! Unity accounts live under `agent-credentials/unity/<id>.json` (a directory
//! older controllers do not read, so a rollback keeps working), their password
//! in `agent-credentials/keys/<id>.json`.
use crate::unity;
use anyhow::{Context, Result, ensure};
use enginebox_agents::{auth, native, pools, security};
use enginebox_core::state;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};

pub const MAX_CREDENTIALS: usize = 1024;
const MAX_SESSIONS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Personal,
    Team,
    ApiKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Created; native sign-in not completed (personal, team).
    Pending,
    /// Native sign-in completed, or an API key is stored.
    Connected,
    /// The last sign-in failed, was cancelled or expired.
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    pub id: String,
    pub org_id: String,
    pub provider: String,
    pub kind: Kind,
    pub label: String,
    /// The member who owns a personal credential (`usr_…`); none for org-owned ones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_user_id: Option<String>,
    /// Personal only: the owner lets the org's pools use it.
    #[serde(default)]
    pub share_with_org_pools: bool,
    pub status: Status,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connected_at: Option<String>,
}
impl Credential {
    pub fn public(&self, has_key: bool) -> Value {
        json!({
            "id": self.id,
            "org_id": self.org_id,
            "provider": self.provider,
            "kind": self.kind,
            "label": self.label,
            "owner_user_id": self.owner_user_id,
            "share_with_org_pools": self.share_with_org_pools,
            "status": self.status,
            "has_key": has_key,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "connected_at": self.connected_at,
        })
    }
}

/// Whether a saved Unity account worked when it was last saved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnityStatus {
    /// Unity accepted the username and password.
    Connected,
    /// Unity could not be asked; `last_error` says why.
    Unverified,
}

/// A member's Unity account. The password is never part of the record.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnityAccount {
    pub id: String,
    pub org_id: String,
    pub owner_user_id: String,
    /// The Unity ID email; also the account's label.
    pub username: String,
    pub status: UnityStatus,
    /// Why the last save could not be verified: `unreachable`,
    /// `service_error` or `unexpected_response`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validated_at: Option<String>,
}
impl UnityAccount {
    pub fn public(&self) -> Value {
        json!({
            "id": self.id,
            "org_id": self.org_id,
            "provider": UNITY,
            "kind": Kind::Personal,
            "label": self.username,
            "username": self.username,
            "owner_user_id": self.owner_user_id,
            "share_with_org_pools": false,
            "status": self.status,
            "has_key": true,
            "last_error": self.last_error,
            "validated_at": self.validated_at,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "connected_at": self.validated_at,
        })
    }
}

/// A refusal with the HTTP status the caller should see. Its message names
/// the rule, never a username, password or token.
#[derive(Debug)]
pub struct Refusal {
    pub status: u16,
    pub message: String,
}
impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Refusal {}
fn refuse(status: u16, message: &str) -> anyhow::Error {
    anyhow::Error::new(Refusal {
        status,
        message: message.to_owned(),
    })
}

pub const UNITY: &str = "unity";
pub const UNITY_INVALID: &str =
    "Unity did not accept this email and password. Check them and save again.";
pub const UNITY_TWO_FACTOR: &str = "This Unity account asks for two-factor sign-in, which Engine Box cannot answer with a password. Turn off two-factor sign-in for this Unity account in your Unity ID settings, then save again.";

/// One eligible credential for a run, as [`Credentials::candidates`] returns it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Candidate {
    pub id: String,
    pub kind: Kind,
    /// Why it is eligible: `own` (the requesting member's personal credential),
    /// `shared` (another member's, shared into org pools), `team` or `api_key`.
    pub reason: &'static str,
}

struct Session {
    credential: String,
    public: Value,
    control: mpsc::Sender<auth::Control>,
    cancel: Arc<AtomicBool>,
}

#[derive(Clone)]
pub struct Credentials {
    root: PathBuf,
    binaries: Arc<BTreeMap<String, Option<String>>>,
    records: Arc<Mutex<BTreeMap<String, Credential>>>,
    sessions: Arc<Mutex<BTreeMap<String, Session>>>,
    unity: Arc<Mutex<BTreeMap<String, UnityAccount>>>,
    unity_login: unity::Client,
}

fn valid_user(id: &str) -> bool {
    id.len() == 28
        && id.starts_with("usr_")
        && id[4..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn valid_label(label: &str) -> bool {
    (1..=80).contains(&label.len()) && !label.chars().any(char::is_control)
}
fn valid_key(key: &str) -> bool {
    (8..=4096).contains(&key.len()) && key.bytes().all(|b| (33..=126).contains(&b))
}
fn valid_username(name: &str) -> bool {
    (3..=254).contains(&name.len())
        && name.contains('@')
        && !name.chars().any(|c| c.is_control() || c.is_whitespace())
}
fn valid_password(password: &str) -> bool {
    (1..=1024).contains(&password.len()) && !password.chars().any(char::is_control)
}
fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

impl Credentials {
    /// Open the store. `provided` overrides native client paths (tests);
    /// otherwise clients are found as the controller-scope accounts find them.
    pub fn open(root: &Path, provided: &BTreeMap<String, String>) -> Result<Self> {
        let records_dir = root.join("agent-credentials");
        state::private_dir(&records_dir)?;
        security::private_dir(&records_dir.join("homes"))?;
        security::private_dir(&records_dir.join("keys"))?;
        state::private_dir(&records_dir.join(UNITY))?;
        let mut binaries = BTreeMap::new();
        let env = BTreeMap::from([(
            "PATH".to_owned(),
            format!(
                "{}:/usr/local/bin:/usr/bin:/bin",
                root.join("agents/bin").display()
            ),
        )]);
        for p in enginebox_agents::PROVIDERS {
            let path = provided
                .get(p)
                .cloned()
                .or_else(|| native::find_binary(native::binary(p), &env));
            binaries.insert(p.to_owned(), path);
        }
        let mut records = BTreeMap::new();
        for path in state::json_files(&records_dir)? {
            let record: Credential = serde_json::from_value(state::read_json(&path)?)?;
            ensure!(
                path.file_stem().and_then(|s| s.to_str()) == Some(record.id.as_str())
                    && valid_id(&record.id),
                "Invalid credential record"
            );
            records.insert(record.id.clone(), record);
        }
        let mut accounts = BTreeMap::new();
        for path in state::json_files(&records_dir.join(UNITY))? {
            let account: UnityAccount = serde_json::from_value(state::read_json(&path)?)?;
            ensure!(
                path.file_stem().and_then(|s| s.to_str()) == Some(account.id.as_str())
                    && valid_id(&account.id),
                "Invalid credential record"
            );
            accounts.insert(account.id.clone(), account);
        }
        Ok(Self {
            root: records_dir,
            binaries: Arc::new(binaries),
            records: Arc::new(Mutex::new(records)),
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
            unity: Arc::new(Mutex::new(accounts)),
            unity_login: unity::Client::from_env(),
        })
    }
    /// Use another Unity login endpoint (tests, or a configured stand-in).
    pub fn with_unity_login(mut self, client: unity::Client) -> Self {
        self.unity_login = client;
        self
    }
    fn home(&self, id: &str) -> Result<PathBuf> {
        ensure!(valid_id(id), "Invalid credential");
        let home = self.root.join("homes").join(id);
        security::private_dir(&home)?;
        security::private_dir(&home.join(".codex"))?;
        security::private_dir(&home.join(".claude"))?;
        Ok(home)
    }
    fn key_path(&self, id: &str) -> PathBuf {
        self.root.join("keys").join(format!("{id}.json"))
    }
    fn has_key(&self, id: &str) -> bool {
        self.key_path(id).is_file()
    }
    fn save(&self, record: &Credential) -> Result<()> {
        state::write_json(
            &self.root.join(format!("{}.json", record.id)),
            &serde_json::to_value(record)?,
        )
    }
    /// The record, if it belongs to `org`.
    pub fn get(&self, org: &str, id: &str) -> Option<Credential> {
        self.records
            .lock()
            .unwrap()
            .get(id)
            .filter(|c| c.org_id == org)
            .cloned()
    }
    /// `GET /v1/credentials`: the acting org's credentials (metadata only).
    pub fn list(&self, org: &str) -> Value {
        let records = self.records.lock().unwrap();
        let rows: Vec<Value> = records
            .values()
            .filter(|c| c.org_id == org)
            .map(|c| c.public(self.has_key(&c.id)))
            .collect();
        drop(records);
        let rows = rows
            .into_iter()
            .chain(
                self.unity
                    .lock()
                    .unwrap()
                    .values()
                    .filter(|a| a.org_id == org)
                    .map(UnityAccount::public),
            )
            .collect::<Vec<_>>();
        json!({"credentials": rows})
    }
    /// `POST /v1/credentials`: create a record in the acting org.
    pub fn create(&self, org: &str, body: &Value) -> Result<Value> {
        crate::keys(
            body,
            &[
                "provider",
                "kind",
                "label",
                "owner_user_id",
                "share_with_org_pools",
                "key",
            ],
            &["provider", "kind", "label"],
        )?;
        ensure!(pools::valid_org(org), "Invalid org");
        ensure!(
            body["provider"] != UNITY,
            "A Unity account is saved with its username and password"
        );
        let provider = enginebox_agents::provider(crate::string(body, "provider")?)?.to_owned();
        let kind: Kind = serde_json::from_value(body["kind"].clone())
            .context("kind must be personal, team or api_key")?;
        let label = crate::string(body, "label")?.trim().to_owned();
        ensure!(
            valid_label(&label),
            "label must be 1-80 printable characters"
        );
        let owner = body.get("owner_user_id").and_then(Value::as_str);
        let share = body
            .get("share_with_org_pools")
            .map(|v| {
                v.as_bool()
                    .context("share_with_org_pools must be a boolean")
            })
            .transpose()?
            .unwrap_or(false);
        let key = body.get("key").and_then(Value::as_str);
        match kind {
            Kind::Personal => ensure!(
                owner.is_some_and(valid_user),
                "A personal credential needs its owner's user ID"
            ),
            _ => ensure!(
                owner.is_none() && !share,
                "Only personal credentials have an owner or a sharing choice"
            ),
        }
        if kind == Kind::ApiKey {
            ensure!(
                key.is_some_and(valid_key),
                "key must be 8-4096 printable characters without spaces"
            );
        } else {
            ensure!(key.is_none(), "Only api_key credentials take a key");
        }
        let at = now();
        let record = Credential {
            id: uuid::Uuid::new_v4().simple().to_string(),
            org_id: org.to_owned(),
            provider,
            kind,
            label,
            owner_user_id: owner.map(str::to_owned),
            share_with_org_pools: share,
            status: if kind == Kind::ApiKey {
                Status::Connected
            } else {
                Status::Pending
            },
            created_at: at.clone(),
            updated_at: at.clone(),
            connected_at: (kind == Kind::ApiKey).then_some(at),
        };
        let mut records = self.records.lock().unwrap();
        ensure!(records.len() < MAX_CREDENTIALS, "Credential limit reached");
        if let Some(key) = key {
            crate::jobs::write_private(&self.key_path(&record.id), &json!({"key": key}))?;
        }
        self.save(&record)?;
        let public = record.public(key.is_some());
        records.insert(record.id.clone(), record);
        Ok(json!({"credential": public}))
    }
    /// `POST /v1/credentials/{id}/share {share}`: the owner's sharing choice (personal only).
    pub fn share(&self, org: &str, id: &str, body: &Value) -> Result<Value> {
        crate::keys(body, &["share"], &["share"])?;
        let share = body["share"].as_bool().context("share must be a boolean")?;
        let mut records = self.records.lock().unwrap();
        let record = records
            .get_mut(id)
            .filter(|c| c.org_id == org)
            .context("Unknown credential")?;
        ensure!(
            record.kind == Kind::Personal,
            "Only personal credentials are shared by their owner"
        );
        let mut next = record.clone();
        next.share_with_org_pools = share;
        next.updated_at = now();
        self.save(&next)?;
        *record = next;
        Ok(json!({"credential": record.public(self.has_key(id))}))
    }
    /// `POST /v1/credentials/{id}/delete`: the record, its home and its key.
    pub fn delete(&self, org: &str, id: &str) -> Result<Value> {
        {
            let mut accounts = self.unity.lock().unwrap();
            if accounts.get(id).is_some_and(|a| a.org_id == org) {
                remove_if_present(&self.key_path(id))?;
                std::fs::remove_file(self.unity_path(id))?;
                accounts.remove(id);
                return Ok(json!({"deleted": true}));
            }
        }
        let mut records = self.records.lock().unwrap();
        ensure!(
            records.get(id).is_some_and(|c| c.org_id == org),
            "Unknown credential"
        );
        let mut sessions = self.sessions.lock().unwrap();
        for s in sessions.values().filter(|s| s.credential == id) {
            s.cancel.store(true, Ordering::SeqCst);
            let _ = s.control.send(auth::Control::Cancel);
        }
        sessions.retain(|_, s| s.credential != id);
        let _ = std::fs::remove_file(self.key_path(id));
        let home = self.root.join("homes").join(id);
        if home.exists() {
            std::fs::remove_dir_all(&home)?;
        }
        std::fs::remove_file(self.root.join(format!("{id}.json")))?;
        records.remove(id);
        Ok(json!({"deleted": true}))
    }
    /// `POST /v1/credentials/{id}/login`: native sign-in into the credential's own home.
    pub fn start_login(&self, org: &str, id: &str) -> Result<Value> {
        let record = self.get(org, id).context("Unknown credential")?;
        ensure!(
            record.kind != Kind::ApiKey,
            "API key credentials have no sign-in"
        );
        let exe = self.binaries[&record.provider]
            .clone()
            .context("Install the native client on the controller.")?;
        let home = self.home(id)?;
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(s) = sessions
            .values()
            .find(|s| s.credential == id && active(&s.public))
        {
            return Ok(s.public.clone());
        }
        sessions.retain(|_, s| active(&s.public));
        ensure!(
            sessions.len() < MAX_SESSIONS,
            "Too many sign-ins are active."
        );
        let session = uuid::Uuid::new_v4().simple().to_string();
        let public = json!({"id":session,"credential_id":id,"provider":record.provider,"scope":"credential","status":"starting","input_required":false,"message":"Starting native sign-in.","expires_at":(chrono::Utc::now()+chrono::Duration::seconds(600)).to_rfc3339()});
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        sessions.insert(
            session.clone(),
            Session {
                credential: id.to_owned(),
                public: public.clone(),
                control: tx,
                cancel: cancel.clone(),
            },
        );
        drop(sessions);
        let this = self.clone();
        let provider = record.provider.clone();
        thread::spawn(move || {
            let result = auth::login(
                &home,
                &provider,
                &exe,
                &this.binaries,
                rx,
                &cancel,
                |event| this.login_event(&session, event),
            );
            if result.is_err() {
                this.login_event(&session, json!({"type":"result","reason":"failed"}));
            }
        });
        Ok(public)
    }
    fn login_event(&self, session: &str, event: Value) {
        let mut sessions = self.sessions.lock().unwrap();
        let Some(s) = sessions.get_mut(session) else {
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
                let state = match event["reason"].as_str() {
                    Some("completed") => "succeeded",
                    Some("cancelled") => "cancelled",
                    Some("expired") => "expired",
                    _ => "failed",
                };
                s.public["status"] = json!(state);
                s.public["input_required"] = json!(false);
                s.public["message"] = json!(if state == "succeeded" {
                    "Connected."
                } else {
                    "Sign-in did not complete. Try again."
                });
                let credential = s.credential.clone();
                drop(sessions);
                let mut records = self.records.lock().unwrap();
                if let Some(record) = records.get_mut(&credential) {
                    let mut next = record.clone();
                    let at = now();
                    next.updated_at = at.clone();
                    if state == "succeeded" {
                        next.status = Status::Connected;
                        next.connected_at = Some(at);
                    } else if next.status != Status::Connected {
                        next.status = Status::Failed;
                    }
                    if self.save(&next).is_ok() {
                        *record = next;
                    }
                }
            }
            _ => {}
        }
    }
    fn session(&self, org: &str, id: &str, session: &str) -> Result<()> {
        ensure!(self.get(org, id).is_some(), "Unknown credential");
        let sessions = self.sessions.lock().unwrap();
        ensure!(
            sessions.get(session).is_some_and(|s| s.credential == id),
            "Unknown sign-in"
        );
        Ok(())
    }
    /// `GET /v1/credentials/{id}/sessions/{session}`.
    pub fn login_status(&self, org: &str, id: &str, session: &str) -> Result<Value> {
        self.session(org, id, session)?;
        Ok(self.sessions.lock().unwrap()[session].public.clone())
    }
    /// `POST /v1/credentials/{id}/sessions/{session}/input {text}` (Claude's authorization code).
    pub fn login_input(&self, org: &str, id: &str, session: &str, body: &Value) -> Result<Value> {
        crate::keys(body, &["text"], &["text"])?;
        self.session(org, id, session)?;
        let text = crate::string(body, "text")?;
        let sessions = self.sessions.lock().unwrap();
        let s = &sessions[session];
        ensure!(
            s.public["provider"] == "claude"
                && s.public["input_required"] == true
                && auth::valid_input(text),
            "This sign-in is not waiting for a valid authorization code"
        );
        s.control.send(auth::Control::Input(text.into()))?;
        Ok(s.public.clone())
    }
    /// `POST /v1/credentials/{id}/sessions/{session}/cancel`.
    pub fn login_cancel(&self, org: &str, id: &str, session: &str) -> Result<Value> {
        self.session(org, id, session)?;
        let sessions = self.sessions.lock().unwrap();
        let s = &sessions[session];
        s.cancel.store(true, Ordering::SeqCst);
        let _ = s.control.send(auth::Control::Cancel);
        Ok(s.public.clone())
    }

    fn unity_path(&self, id: &str) -> PathBuf {
        self.root.join(UNITY).join(format!("{id}.json"))
    }
    /// The org's Unity account with this ID.
    pub fn unity_get(&self, org: &str, id: &str) -> Option<UnityAccount> {
        self.unity
            .lock()
            .unwrap()
            .get(id)
            .filter(|a| a.org_id == org)
            .cloned()
    }
    /// The username and password of a save or update request, checked.
    fn unity_fields<'a>(body: &'a Value, current: Option<&'a str>) -> Result<(&'a str, &'a str)> {
        let username = match (body.get("username"), current) {
            (Some(v), _) => v.as_str().context("username must be a string")?.trim(),
            (None, Some(current)) => current,
            (None, None) => return Err(refuse(400, "username is required")),
        };
        if !valid_username(username) {
            return Err(refuse(
                400,
                "username must be your Unity ID email (3-254 characters, no spaces)",
            ));
        }
        let password = body
            .get("password")
            .map(|v| v.as_str().context("password must be a string"))
            .transpose()?
            .ok_or_else(|| refuse(400, "password is required"))?;
        if !valid_password(password) {
            return Err(refuse(
                400,
                "password must be 1-1024 characters without control characters",
            ));
        }
        Ok((username, password))
    }
    /// Ask Unity whether the username and password work: `Ok(None)` when it
    /// accepted them, `Ok(Some(reason))` when it could not be asked, a
    /// [`Refusal`] when it rejected them or wants a second factor.
    fn unity_check(&self, username: &str, password: &str) -> Result<Option<&'static str>> {
        match self.unity_login.login(username, password) {
            unity::Login::Token(_) => Ok(None),
            unity::Login::Invalid => Err(refuse(400, UNITY_INVALID)),
            unity::Login::TwoFactor => Err(refuse(400, UNITY_TWO_FACTOR)),
            unity::Login::Unknown(reason) => Ok(Some(reason)),
        }
    }
    fn unity_store(&self, account: &UnityAccount, password: &str) -> Result<()> {
        crate::jobs::write_private(&self.key_path(&account.id), &json!({"password": password}))?;
        state::write_json(
            &self.unity_path(&account.id),
            &serde_json::to_value(account)?,
        )
    }
    /// `POST /v1/credentials {provider: "unity", kind: "personal",
    /// owner_user_id, username, password}`: save a member's Unity account
    /// after asking Unity whether it works. Blocking (one Unity sign-in).
    ///
    /// One per member per org: a second save is a 409 (update the first). A
    /// rejected password or a two-factor account is refused (400) and nothing
    /// is stored; when Unity cannot be asked the account is saved `unverified`.
    /// `actor` is the member the dashboard names; a member saves only their own.
    pub fn unity_create(&self, org: &str, actor: &str, body: &Value) -> Result<Value> {
        crate::keys(
            body,
            &[
                "provider",
                "kind",
                "owner_user_id",
                "username",
                "password",
                "share_with_org_pools",
                "key",
            ],
            &["provider", "kind", "owner_user_id", "password"],
        )?;
        ensure!(pools::valid_org(org), "Invalid org");
        ensure!(body["provider"] == UNITY, "provider must be unity");
        if body["kind"] != "personal" {
            return Err(refuse(
                400,
                "A Unity account is personal: each member saves their own, never a team account or key",
            ));
        }
        if body.get("share_with_org_pools").is_some() || body.get("key").is_some() {
            return Err(refuse(
                400,
                "A Unity account is used only by its owner; it is never shared and takes no key",
            ));
        }
        let owner = body["owner_user_id"]
            .as_str()
            .filter(|o| valid_user(o))
            .ok_or_else(|| refuse(400, "A Unity account needs its owner's user ID"))?;
        if actor.starts_with("usr_") && actor != owner {
            return Err(refuse(403, "Members save only their own Unity account."));
        }
        let (username, password) = Self::unity_fields(body, None)?;
        let exists = |accounts: &BTreeMap<String, UnityAccount>| {
            accounts
                .values()
                .any(|a| a.org_id == org && a.owner_user_id == owner)
        };
        let duplicate = || {
            refuse(
                409,
                "You already have a Unity account in this organization. Update it instead.",
            )
        };
        if exists(&self.unity.lock().unwrap()) {
            return Err(duplicate());
        }
        let unverified = self.unity_check(username, password)?;
        let at = now();
        let account = UnityAccount {
            id: uuid::Uuid::new_v4().simple().to_string(),
            org_id: org.to_owned(),
            owner_user_id: owner.to_owned(),
            username: username.to_owned(),
            status: if unverified.is_some() {
                UnityStatus::Unverified
            } else {
                UnityStatus::Connected
            },
            last_error: unverified.map(str::to_owned),
            created_at: at.clone(),
            updated_at: at.clone(),
            validated_at: unverified.is_none().then_some(at),
        };
        let total = self.records.lock().unwrap().len();
        let mut accounts = self.unity.lock().unwrap();
        // Two saves can race through Unity; the first one stored wins.
        if exists(&accounts) {
            return Err(duplicate());
        }
        ensure!(
            total + accounts.len() < MAX_CREDENTIALS,
            "Credential limit reached"
        );
        self.unity_store(&account, password)?;
        let public = account.public();
        accounts.insert(account.id.clone(), account);
        Ok(json!({"credential": public}))
    }
    /// `POST /v1/credentials/{id}/update {username?, password}`: replace a
    /// Unity account's username and password, checked with Unity the same way
    /// as a save. A refused update leaves the saved account as it was.
    pub fn unity_update(&self, org: &str, actor: &str, id: &str, body: &Value) -> Result<Value> {
        crate::keys(body, &["username", "password"], &["password"])?;
        let current = self
            .unity_get(org, id)
            .ok_or_else(|| refuse(404, "Unknown credential"))?;
        if actor.starts_with("usr_") && actor != current.owner_user_id {
            return Err(refuse(403, "Only its owner updates a Unity account."));
        }
        let (username, password) = Self::unity_fields(body, Some(&current.username))?;
        let unverified = self.unity_check(username, password)?;
        let mut accounts = self.unity.lock().unwrap();
        let account = accounts
            .get_mut(id)
            .filter(|a| a.org_id == org)
            .ok_or_else(|| refuse(404, "Unknown credential"))?;
        let at = now();
        let mut next = account.clone();
        next.username = username.to_owned();
        next.updated_at = at.clone();
        next.status = if unverified.is_some() {
            UnityStatus::Unverified
        } else {
            UnityStatus::Connected
        };
        next.last_error = unverified.map(str::to_owned);
        next.validated_at = unverified.is_none().then_some(at);
        self.unity_store(&next, password)?;
        *account = next;
        Ok(json!({"credential": account.public()}))
    }
    /// The Unity account a session started by `member` uses: only that
    /// member's own in `org`. There is no fallback to a team account or to
    /// another member's account.
    pub fn unity_for(&self, org: &str, member: &str) -> Option<UnityAccount> {
        self.unity
            .lock()
            .unwrap()
            .values()
            .find(|a| a.org_id == org && a.owner_user_id == member)
            .cloned()
    }
    /// Sign `member`'s own Unity account in and return a short-lived access
    /// token, for a node to run `Unity.Licensing.Client --activate-all
    /// --accessToken <token> --include-personal`. The password stays on the
    /// controller. Blocking (one Unity sign-in). Not called by sessions yet.
    pub fn unity_access_token(&self, org: &str, member: &str) -> Result<String> {
        let account = self.unity_for(org, member).ok_or_else(|| {
            refuse(
                404,
                "Save your Unity account on the Agents page to use Unity in your sessions.",
            )
        })?;
        let missing = "The saved Unity password is missing. Update your Unity account.";
        let stored = state::read_json(&self.key_path(&account.id)).context(missing)?;
        let password = stored["password"].as_str().context(missing)?;
        match self.unity_login.login(&account.username, password) {
            unity::Login::Token(token) => Ok(token),
            unity::Login::Invalid => Err(refuse(
                400,
                "Unity no longer accepts your saved password. Update your Unity account.",
            )),
            unity::Login::TwoFactor => Err(refuse(400, UNITY_TWO_FACTOR)),
            unity::Login::Unknown(_) => Err(refuse(
                502,
                "Unity sign-in is unavailable right now. Try again shortly.",
            )),
        }
    }

    /// The selection interface for runs (not called yet). Eligible connected
    /// credentials of `org` for `provider`, in priority order: the requesting
    /// member's own personal credentials, then personal credentials their
    /// owners shared into org pools, then team accounts, then API keys. Pool
    /// priorities and fallback policy will build on this.
    pub fn candidates(&self, org: &str, provider: &str, member: Option<&str>) -> Vec<Candidate> {
        let records = self.records.lock().unwrap();
        let mut out: Vec<(u8, Candidate)> = records
            .values()
            .filter(|c| c.org_id == org && c.provider == provider && c.status == Status::Connected)
            .filter_map(|c| {
                let (rank, reason) = match c.kind {
                    Kind::Personal if member.is_some() && c.owner_user_id.as_deref() == member => {
                        (0, "own")
                    }
                    Kind::Personal if c.share_with_org_pools => (1, "shared"),
                    Kind::Personal => return None,
                    Kind::Team => (2, "team"),
                    Kind::ApiKey => (3, "api_key"),
                };
                Some((
                    rank,
                    Candidate {
                        id: c.id.clone(),
                        kind: c.kind,
                        reason,
                    },
                ))
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.id.cmp(&b.1.id)));
        out.into_iter().map(|(_, c)| c).collect()
    }
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

fn active(public: &Value) -> bool {
    matches!(public["status"].as_str(), Some("starting" | "waiting"))
}

#[cfg(test)]
mod tests {
    use super::*;
    const ORG: &str = "org_0123456789abcdef01234567";
    const OTHER: &str = "org_fedcba9876543210fedcba98";
    const ALICE: &str = "usr_aaaaaaaaaaaaaaaaaaaaaaaa";
    const BOB: &str = "usr_bbbbbbbbbbbbbbbbbbbbbbbb";

    fn open() -> (tempfile::TempDir, Credentials) {
        let temp = tempfile::tempdir().unwrap();
        // The controller canonicalizes its state root (macOS /var is a symlink).
        let c = Credentials::open(&temp.path().canonicalize().unwrap(), &BTreeMap::new()).unwrap();
        (temp, c)
    }
    fn mark_connected(c: &Credentials, id: &str) {
        let mut records = c.records.lock().unwrap();
        records.get_mut(id).unwrap().status = Status::Connected;
    }

    #[test]
    fn records_are_org_scoped_and_validated() {
        let (temp, c) = open();
        let made = c
            .create(ORG, &json!({"provider":"codex","kind":"personal","label":"Alice's ChatGPT","owner_user_id":ALICE}))
            .unwrap();
        let id = made["credential"]["id"].as_str().unwrap().to_owned();
        assert_eq!(made["credential"]["status"], "pending");
        assert_eq!(c.list(ORG)["credentials"].as_array().unwrap().len(), 1);
        assert_eq!(c.list(OTHER)["credentials"].as_array().unwrap().len(), 0);
        assert!(c.get(OTHER, &id).is_none());
        assert!(c.share(OTHER, &id, &json!({"share":true})).is_err());
        assert!(c.delete(OTHER, &id).is_err());
        for bad in [
            json!({"provider":"codex","kind":"personal","label":"x"}),
            json!({"provider":"codex","kind":"team","label":"x","owner_user_id":ALICE}),
            json!({"provider":"codex","kind":"team","label":"x","share_with_org_pools":true}),
            json!({"provider":"codex","kind":"api_key","label":"x"}),
            json!({"provider":"codex","kind":"team","label":"x","key":"sk-12345678"}),
            json!({"provider":"gemini","kind":"team","label":"x"}),
            json!({"provider":"codex","kind":"root","label":"x"}),
            json!({"provider":"codex","kind":"team","label":""}),
        ] {
            assert!(c.create(ORG, &bad).is_err(), "{bad}");
        }
        // Reopened from disk.
        let again =
            Credentials::open(&temp.path().canonicalize().unwrap(), &BTreeMap::new()).unwrap();
        assert!(again.get(ORG, &id).is_some());
        c.delete(ORG, &id).unwrap();
        assert!(c.get(ORG, &id).is_none());
    }

    #[test]
    fn api_keys_are_write_only() {
        let (temp, c) = open();
        let secret = "sk-test-only-0123456789";
        let made = c
            .create(
                ORG,
                &json!({"provider":"claude","kind":"api_key","label":"CI key","key":secret}),
            )
            .unwrap();
        assert!(!made.to_string().contains(secret));
        assert_eq!(made["credential"]["has_key"], true);
        assert_eq!(made["credential"]["status"], "connected");
        assert!(!c.list(ORG).to_string().contains(secret));
        let id = made["credential"]["id"].as_str().unwrap();
        let record =
            std::fs::read_to_string(temp.path().join(format!("agent-credentials/{id}.json")))
                .unwrap();
        assert!(!record.contains(secret));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(
                temp.path()
                    .join(format!("agent-credentials/keys/{id}.json")),
            )
            .unwrap()
            .permissions()
            .mode();
            assert_eq!(mode & 0o077, 0);
        }
    }

    #[test]
    fn candidates_prefer_own_then_shared_then_team_then_keys() {
        let (_temp, c) = open();
        let mk = |body: Value| {
            c.create(ORG, &body).unwrap()["credential"]["id"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        let alice =
            mk(json!({"provider":"codex","kind":"personal","label":"a","owner_user_id":ALICE}));
        let bob_private =
            mk(json!({"provider":"codex","kind":"personal","label":"b","owner_user_id":BOB}));
        let bob_shared =
            mk(json!({"provider":"codex","kind":"personal","label":"b2","owner_user_id":BOB}));
        let team = mk(json!({"provider":"codex","kind":"team","label":"t"}));
        let key =
            mk(json!({"provider":"codex","kind":"api_key","label":"k","key":"sk-0123456789"}));
        let pending_team = mk(json!({"provider":"codex","kind":"team","label":"p"}));
        let claude = mk(json!({"provider":"claude","kind":"team","label":"c"}));
        for id in [&alice, &bob_private, &bob_shared, &team, &claude] {
            mark_connected(&c, id);
        }
        c.share(ORG, &bob_shared, &json!({"share":true})).unwrap();
        let ids = |member| {
            c.candidates(ORG, "codex", member)
                .into_iter()
                .map(|x| x.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(Some(ALICE)),
            vec![alice.clone(), bob_shared.clone(), team.clone(), key.clone()]
        );
        // Without a member (an org pool run), only shared, team and keys.
        assert_eq!(
            ids(None),
            vec![bob_shared.clone(), team.clone(), key.clone()]
        );
        assert!(!ids(Some(ALICE)).contains(&bob_private));
        assert!(!ids(Some(ALICE)).contains(&pending_team));
        assert!(c.candidates(OTHER, "codex", Some(ALICE)).is_empty());
        // Sharing is personal-only.
        assert!(c.share(ORG, &team, &json!({"share":true})).is_err());
    }

    fn with_unity() -> (tempfile::TempDir, Credentials, Arc<Mutex<Vec<String>>>) {
        let (temp, c) = open();
        let (url, seen) = unity::fake::serve();
        let c = c.with_unity_login(
            unity::Client::new(&url).with_timeout(std::time::Duration::from_secs(1)),
        );
        (temp, c, seen)
    }
    fn unity_body(owner: &str, username: &str, password: &str) -> Value {
        json!({"provider":"unity","kind":"personal","owner_user_id":owner,"username":username,"password":password})
    }
    fn refusal(error: anyhow::Error) -> (u16, String) {
        let r = error.downcast_ref::<Refusal>().expect("a refusal");
        (r.status, r.message.clone())
    }

    #[test]
    fn unity_accounts_are_checked_with_unity_when_saved() {
        let (temp, c, seen) = with_unity();
        // Wrong password and two-factor accounts are refused and nothing is stored.
        let (status, message) = refusal(
            c.unity_create(
                ORG,
                ALICE,
                &unity_body(ALICE, "a@example.com", "wrong-password"),
            )
            .unwrap_err(),
        );
        assert_eq!((status, message.as_str()), (400, UNITY_INVALID));
        let (status, message) = refusal(
            c.unity_create(
                ORG,
                ALICE,
                &unity_body(ALICE, "a@example.com", "tfa-password"),
            )
            .unwrap_err(),
        );
        assert_eq!(status, 400);
        assert!(message.contains("Turn off two-factor sign-in"));
        assert!(c.unity_for(ORG, ALICE).is_none());
        // Unity unavailable: saved, but unverified with the reason.
        let made = c
            .unity_create(
                ORG,
                ALICE,
                &unity_body(ALICE, "a@example.com", "down-password"),
            )
            .unwrap();
        assert_eq!(made["credential"]["status"], "unverified");
        assert_eq!(made["credential"]["last_error"], "service_error");
        assert_eq!(made["credential"]["validated_at"], Value::Null);
        let id = made["credential"]["id"].as_str().unwrap().to_owned();
        // Updating re-checks: a good password connects it.
        let updated = c
            .unity_update(ORG, ALICE, &id, &json!({"password":"right-password"}))
            .unwrap();
        assert_eq!(updated["credential"]["status"], "connected");
        assert_eq!(updated["credential"]["label"], "a@example.com");
        assert_eq!(updated["credential"]["last_error"], Value::Null);
        assert!(updated["credential"]["validated_at"].is_string());
        // A refused update leaves the saved account as it was.
        assert_eq!(
            refusal(
                c.unity_update(ORG, ALICE, &id, &json!({"password":"wrong-password"}))
                    .unwrap_err()
            )
            .0,
            400
        );
        assert_eq!(
            c.unity_for(ORG, ALICE).unwrap().status,
            UnityStatus::Connected
        );
        let timed_out = c
            .unity_update(
                ORG,
                ALICE,
                &id,
                &json!({"username":"a2@example.com","password":"slow-password"}),
            )
            .unwrap();
        assert_eq!(timed_out["credential"]["status"], "unverified");
        assert_eq!(timed_out["credential"]["last_error"], "unreachable");
        assert_eq!(timed_out["credential"]["username"], "a2@example.com");
        // Unity was asked with exactly the saved password.
        assert!(
            seen.lock()
                .unwrap()
                .iter()
                .any(|b| b.contains("right-password"))
        );
        // No password in any listing, record or reopened state; only in the 0600 key file.
        for password in [
            "right-password",
            "slow-password",
            "wrong-password",
            "tfa-password",
        ] {
            assert!(!c.list(ORG).to_string().contains(password));
        }
        let record = std::fs::read_to_string(
            temp.path()
                .join(format!("agent-credentials/unity/{id}.json")),
        )
        .unwrap();
        assert!(!record.contains("password"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(
                temp.path()
                    .join(format!("agent-credentials/keys/{id}.json")),
            )
            .unwrap()
            .permissions()
            .mode();
            assert_eq!(mode & 0o077, 0);
        }
        let again =
            Credentials::open(&temp.path().canonicalize().unwrap(), &BTreeMap::new()).unwrap();
        assert_eq!(again.unity_for(ORG, ALICE).unwrap().id, id);
        // Removal deletes the record and the password.
        c.delete(ORG, &id).unwrap();
        assert!(c.unity_for(ORG, ALICE).is_none());
        assert!(
            !temp
                .path()
                .join(format!("agent-credentials/keys/{id}.json"))
                .exists()
        );
    }

    #[test]
    fn unity_is_one_personal_account_per_member_used_only_by_its_owner() {
        let (_temp, c, _) = with_unity();
        let alice = c
            .unity_create(
                ORG,
                ALICE,
                &unity_body(ALICE, "a@example.com", "right-password"),
            )
            .unwrap()["credential"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        // A second save is a conflict; update the first instead.
        assert_eq!(
            refusal(
                c.unity_create(
                    ORG,
                    ALICE,
                    &unity_body(ALICE, "b@example.com", "camel-password")
                )
                .unwrap_err()
            )
            .0,
            409
        );
        // The same member in another org has their own.
        c.unity_create(
            OTHER,
            ALICE,
            &unity_body(ALICE, "a@example.com", "right-password"),
        )
        .unwrap();
        // Bob cannot save one for Alice or update hers.
        assert_eq!(
            refusal(
                c.unity_create(
                    ORG,
                    BOB,
                    &unity_body(ALICE, "b@example.com", "right-password")
                )
                .unwrap_err()
            )
            .0,
            403
        );
        assert_eq!(
            refusal(
                c.unity_update(ORG, BOB, &alice, &json!({"password":"right-password"}))
                    .unwrap_err()
            )
            .0,
            403
        );
        assert_eq!(
            refusal(
                c.unity_update(OTHER, ALICE, &alice, &json!({"password":"right-password"}))
                    .unwrap_err()
            )
            .0,
            404
        );
        // Unity is personal only, never shared, never a key; sign-in and sharing do not apply.
        for bad in [
            json!({"provider":"unity","kind":"team","owner_user_id":BOB,"username":"b@example.com","password":"right-password"}),
            json!({"provider":"unity","kind":"api_key","owner_user_id":BOB,"username":"b@example.com","password":"right-password"}),
            json!({"provider":"unity","kind":"personal","owner_user_id":BOB,"username":"b@example.com","password":"right-password","share_with_org_pools":true}),
            json!({"provider":"unity","kind":"personal","owner_user_id":BOB,"username":"not-an-email","password":"right-password"}),
            json!({"provider":"unity","kind":"personal","owner_user_id":BOB,"username":"b@example.com"}),
        ] {
            assert!(c.unity_create(ORG, BOB, &bad).is_err(), "{bad}");
        }
        assert!(
            c.create(
                ORG,
                &json!({"provider":"unity","kind":"personal","label":"x","owner_user_id":BOB})
            )
            .is_err()
        );
        assert!(c.share(ORG, &alice, &json!({"share":true})).is_err());
        assert!(c.start_login(ORG, &alice).is_err());
        assert!(c.unity_for(ORG, BOB).is_none());
        // Selection: only the member's own account, and agent candidates never include it.
        assert_eq!(c.unity_for(ORG, ALICE).unwrap().id, alice);
        assert!(c.candidates(ORG, UNITY, Some(ALICE)).is_empty());
        assert_eq!(
            c.unity_access_token(ORG, ALICE).unwrap(),
            unity::fake::TOKEN
        );
        assert_eq!(refusal(c.unity_access_token(ORG, BOB).unwrap_err()).0, 404);
        // An unverified save still never falls back to anyone else's.
        c.unity_create(ORG, BOB, &unity_body(BOB, "b@example.com", "down-password"))
            .unwrap();
        assert_eq!(refusal(c.unity_access_token(ORG, BOB).unwrap_err()).0, 502);
    }

    #[test]
    fn sign_in_needs_an_installed_client_and_never_applies_to_keys() {
        let (_temp, c) = open();
        let team = c
            .create(ORG, &json!({"provider":"cursor","kind":"team","label":"t"}))
            .unwrap();
        let key = c
            .create(
                ORG,
                &json!({"provider":"cursor","kind":"api_key","label":"k","key":"key-01234567"}),
            )
            .unwrap();
        let none = Credentials::open(c.root.parent().unwrap(), &BTreeMap::new()).unwrap();
        if none.binaries["cursor"].is_none() {
            assert!(
                none.start_login(ORG, team["credential"]["id"].as_str().unwrap())
                    .is_err()
            );
        }
        assert!(
            c.start_login(ORG, key["credential"]["id"].as_str().unwrap())
                .is_err()
        );
        assert!(
            c.start_login(OTHER, team["credential"]["id"].as_str().unwrap())
                .is_err()
        );
    }
}
