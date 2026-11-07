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
//! A member may also connect their **own** subscriptions through an app they
//! approved with the `agent-credentials` OAuth scope ([`Credentials::member_create`]
//! and the other `member_*` methods; docs/MEMBER-SUBSCRIPTIONS.md). Those are
//! ordinary personal credentials owned by the token's member, marked in
//! `agent-credentials/member/<id>.json` so they can never be shared into org
//! pools: they serve only their owner's own runs.
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

/// Credentials across all orgs, per org, and per member in an org: one org
/// (or one member's app) cannot exhaust the others.
pub const MAX_CREDENTIALS: usize = 4096;
const MAX_CREDENTIALS_PER_ORG: usize = 512;
const MAX_CREDENTIALS_PER_MEMBER: usize = 16;
/// Sign-ins in progress across all orgs, per org, and per member.
const MAX_SESSIONS: usize = 64;
const MAX_SESSIONS_PER_ORG: usize = 16;
const MAX_SESSIONS_PER_MEMBER: usize = 2;

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
    /// A new personal credential of `owner`, pending sign-in, never shared.
    fn personal(org: &str, provider: &str, label: &str, owner: &str) -> Self {
        let at = now();
        Self {
            id: uuid::Uuid::new_v4().simple().to_string(),
            org_id: org.to_owned(),
            provider: provider.to_owned(),
            kind: Kind::Personal,
            label: label.to_owned(),
            owner_user_id: Some(owner.to_owned()),
            share_with_org_pools: false,
            status: Status::Pending,
            created_at: at.clone(),
            updated_at: at,
            connected_at: None,
        }
    }
    /// `member_connected`: connected through a member's own app grant, so
    /// never shareable (the dashboard hides Share for it).
    pub fn public(&self, has_key: bool, member_connected: bool) -> Value {
        json!({
            "is_member_connected": member_connected,
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
const MEMBER_DIR: &str = "member";
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

/// The providers a member may connect through an app: subscriptions with a
/// native sign-in and a delegable access token.
pub const MEMBER_PROVIDERS: &[&str] = &["claude", "codex"];

/// A credential connected through a member's own app grant. Stored beside
/// the record (`agent-credentials/member/<id>.json`, a directory older
/// controllers do not read) so the record format is unchanged.
/// (Only the ID: the record holds the rest. Older marker fields are ignored.)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemberMeta {
    pub id: String,
}

struct Session {
    credential: String,
    org: String,
    owner: Option<String>,
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
    member: Arc<Mutex<BTreeMap<String, MemberMeta>>>,
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
        state::private_dir(&records_dir.join(MEMBER_DIR))?;
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
        let mut member = BTreeMap::new();
        for path in state::json_files(&records_dir.join(MEMBER_DIR))? {
            let meta: MemberMeta = serde_json::from_value(state::read_json(&path)?)?;
            ensure!(
                path.file_stem().and_then(|s| s.to_str()) == Some(meta.id.as_str())
                    && valid_id(&meta.id),
                "Invalid credential record"
            );
            // A marker whose record is gone (an interrupted delete) is removed.
            if records.contains_key(&meta.id) {
                member.insert(meta.id.clone(), meta);
            } else if let Err(error) = std::fs::remove_file(&path) {
                eprintln!("credentials: an orphaned member marker could not be removed: {error}");
            }
        }
        Ok(Self {
            root: records_dir,
            binaries: Arc::new(binaries),
            records: Arc::new(Mutex::new(records)),
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
            unity: Arc::new(Mutex::new(accounts)),
            unity_login: unity::Client::from_env(),
            member: Arc::new(Mutex::new(member)),
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
            .map(|c| c.public(self.has_key(&c.id), self.is_member_connected(&c.id)))
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
        Self::check_limits(&records, org, owner)?;
        if let Some(key) = key {
            crate::jobs::write_private(&self.key_path(&record.id), &json!({"key": key}))?;
        }
        self.save(&record)?;
        let public = record.public(key.is_some(), false);
        records.insert(record.id.clone(), record);
        Ok(json!({"credential": public}))
    }
    /// The global, per-org and per-member credential limits (409 when reached).
    fn check_limits(
        records: &BTreeMap<String, Credential>,
        org: &str,
        owner: Option<&str>,
    ) -> Result<()> {
        let in_org = records.values().filter(|c| c.org_id == org).count();
        let of_member = owner.map_or(0, |o| {
            records
                .values()
                .filter(|c| c.org_id == org && c.owner_user_id.as_deref() == Some(o))
                .count()
        });
        if records.len() >= MAX_CREDENTIALS
            || in_org >= MAX_CREDENTIALS_PER_ORG
            || of_member >= MAX_CREDENTIALS_PER_MEMBER
        {
            return Err(refuse(409, "Credential limit reached."));
        }
        Ok(())
    }
    /// `POST /v1/credentials/{id}/share {share}`: the owner's sharing choice (personal only).
    pub fn share(&self, org: &str, id: &str, body: &Value) -> Result<Value> {
        crate::keys(body, &["share"], &["share"])?;
        let share = body["share"].as_bool().context("share must be a boolean")?;
        if self.member.lock().unwrap().contains_key(id) {
            return Err(refuse(
                400,
                "A subscription connected through an app is used only by its owner's own runs; it is never shared.",
            ));
        }
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
        Ok(json!({"credential": record.public(self.has_key(id), false)}))
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
        drop(sessions);
        // The marker goes last; a failure to remove it is logged and the
        // orphan is swept when the store next opens.
        if self.member.lock().unwrap().remove(id).is_some()
            && let Err(error) = remove_if_present(&self.member_path(id))
        {
            eprintln!("credentials: a member marker could not be removed: {error}");
        }
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
        let in_org = sessions.values().filter(|s| s.org == org).count();
        let of_owner = sessions
            .values()
            .filter(|s| {
                s.org == org && record.owner_user_id.is_some() && s.owner == record.owner_user_id
            })
            .count();
        if sessions.len() >= MAX_SESSIONS
            || in_org >= MAX_SESSIONS_PER_ORG
            || of_owner >= MAX_SESSIONS_PER_MEMBER
        {
            return Err(refuse(
                429,
                "Too many sign-ins are active; finish or cancel one first.",
            ));
        }
        let session = uuid::Uuid::new_v4().simple().to_string();
        let public = json!({"id":session,"credential_id":id,"provider":record.provider,"scope":"credential","status":"starting","input_required":false,"message":"Starting native sign-in.","expires_at":(chrono::Utc::now()+chrono::Duration::seconds(600)).to_rfc3339()});
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        sessions.insert(
            session.clone(),
            Session {
                credential: id.to_owned(),
                org: org.to_owned(),
                owner: record.owner_user_id.clone(),
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

    fn member_path(&self, id: &str) -> PathBuf {
        self.root.join(MEMBER_DIR).join(format!("{id}.json"))
    }
    /// Whether `id` was connected through a member's own app grant.
    pub fn is_member_connected(&self, id: &str) -> bool {
        self.member.lock().unwrap().contains_key(id)
    }
    /// What a member sees of their own credential: no org, owner or sharing
    /// fields, and never a value.
    fn member_view(&self, c: &Credential) -> Value {
        json!({
            "id": c.id,
            "provider": c.provider,
            "label": c.label,
            "status": c.status,
            "created_at": c.created_at,
            "updated_at": c.updated_at,
            "connected_at": c.connected_at,
        })
    }
    /// `member`'s own personal credential `id` in `org`. Anything else
    /// (another member's, a team account, a key, another org's) is the same
    /// 404, so a member cannot probe what exists.
    pub fn member_owned(&self, org: &str, member: &str, id: &str) -> Result<Credential> {
        // An app reaches only what it connected: a member-connected
        // credential of a member provider, never one made on the dashboard.
        self.get(org, id)
            .filter(|c| {
                valid_user(member)
                    && c.kind == Kind::Personal
                    && c.owner_user_id.as_deref() == Some(member)
                    && MEMBER_PROVIDERS.contains(&c.provider.as_str())
                    && self.is_member_connected(id)
            })
            .ok_or_else(|| refuse(404, "Unknown credential."))
    }
    /// `GET /v1/projects/{p}/me/credentials`: the member's own personal
    /// credentials in `org`, and nobody else's.
    pub fn member_list(&self, org: &str, member: &str) -> Value {
        let records = self.records.lock().unwrap();
        let rows: Vec<Value> = records
            .values()
            .filter(|c| {
                c.org_id == org
                    && c.kind == Kind::Personal
                    && c.owner_user_id.as_deref() == Some(member)
                    && MEMBER_PROVIDERS.contains(&c.provider.as_str())
                    && self.is_member_connected(&c.id)
            })
            .map(|c| self.member_view(c))
            .collect();
        json!({"credentials": rows})
    }
    /// `POST /v1/projects/{p}/me/credentials {provider, label?}`: the
    /// member's own personal credential for `provider`, created when they
    /// have none. Never shared (`share_with_org_pools` is not accepted and
    /// stays false). Returns whether it was created.
    pub fn member_create(&self, org: &str, member: &str, body: &Value) -> Result<(bool, Value)> {
        if !body.is_object() {
            return Err(refuse(400, "Body must be a JSON object."));
        }
        if body.as_object().is_some_and(|o| {
            o.keys()
                .any(|k| !["provider", "label"].contains(&k.as_str()))
        }) {
            return Err(refuse(
                400,
                "Only provider and label are accepted; a member's subscription is never shared and takes no key.",
            ));
        }
        if !valid_user(member) || !pools::valid_org(org) {
            return Err(refuse(
                403,
                "Only a member connects their own subscription.",
            ));
        }
        let provider = body["provider"]
            .as_str()
            .filter(|p| MEMBER_PROVIDERS.contains(p))
            .ok_or_else(|| refuse(400, "provider must be claude or codex"))?
            .to_owned();
        let label = match body.get("label") {
            None | Some(Value::Null) => match provider.as_str() {
                "claude" => "Claude subscription".to_owned(),
                _ => "ChatGPT subscription".to_owned(),
            },
            Some(v) => v
                .as_str()
                .map(str::trim)
                .filter(|l| valid_label(l))
                .ok_or_else(|| refuse(400, "label must be 1-80 printable characters"))?
                .to_owned(),
        };
        let mut records = self.records.lock().unwrap();
        // Reuse only a credential this member connected through an app; a
        // dashboard-made (or shared) personal one is never adopted.
        if let Some(existing) = records.values().find(|c| {
            c.org_id == org
                && c.kind == Kind::Personal
                && c.provider == provider
                && c.owner_user_id.as_deref() == Some(member)
                && self.is_member_connected(&c.id)
        }) {
            return Ok((false, json!({"credential": self.member_view(existing)})));
        }
        Self::check_limits(&records, org, Some(member))?;
        let record = Credential::personal(org, &provider, &label, member);
        let meta = MemberMeta {
            id: record.id.clone(),
        };
        // Marker first: a crash between the two leaves a marker the next
        // open removes, never a shareable record.
        state::write_json(&self.member_path(&meta.id), &serde_json::to_value(&meta)?)?;
        self.save(&record)?;
        let view = self.member_view(&record);
        self.member.lock().unwrap().insert(meta.id.clone(), meta);
        records.insert(record.id.clone(), record);
        Ok((true, json!({"credential": view})))
    }
    /// The parts of a sign-in a member's app may relay: the validated
    /// provider link, the device code, whether a pasted code is awaited, and
    /// the state. Never a credential value (the sign-in never stores one here).
    fn member_session_view(public: &Value) -> Value {
        let mut view = serde_json::Map::new();
        for key in [
            "id",
            "credential_id",
            "provider",
            "status",
            "input_required",
            "message",
            "expires_at",
            "authorization_url",
            "user_code",
        ] {
            if let Some(v) = public.get(key) {
                view.insert(key.to_owned(), v.clone());
            }
        }
        Value::Object(view)
    }
    /// `POST /v1/projects/{p}/me/credentials/{id}/login {}`.
    pub fn member_login(&self, org: &str, member: &str, id: &str) -> Result<Value> {
        self.member_owned(org, member, id)?;
        Ok(json!({"session": Self::member_session_view(&self.start_login(org, id)?)}))
    }
    /// `GET /v1/projects/{p}/me/credentials/{id}/sessions/{session}`.
    pub fn member_login_status(
        &self,
        org: &str,
        member: &str,
        id: &str,
        session: &str,
    ) -> Result<Value> {
        self.member_owned(org, member, id)?;
        let public = self
            .login_status(org, id, session)
            .map_err(|_| refuse(404, "Unknown sign-in."))?;
        Ok(json!({"session": Self::member_session_view(&public)}))
    }
    /// `POST /v1/projects/{p}/me/credentials/{id}/sessions/{session}/input {text}`.
    pub fn member_login_input(
        &self,
        org: &str,
        member: &str,
        id: &str,
        session: &str,
        body: &Value,
    ) -> Result<Value> {
        self.member_owned(org, member, id)?;
        let text = body
            .as_object()
            .filter(|o| o.len() == 1)
            .and_then(|o| o.get("text"))
            .and_then(Value::as_str)
            .ok_or_else(|| refuse(400, "Send {\"text\": <the code the provider showed>}."))?;
        let sessions = self.sessions.lock().unwrap();
        let s = sessions
            .get(session)
            .filter(|s| s.credential == id)
            .ok_or_else(|| refuse(404, "Unknown sign-in."))?;
        if !active(&s.public) {
            return Err(refuse(410, "This sign-in has ended; start a new sign-in."));
        }
        if s.public["provider"] != "claude" || s.public["input_required"] != true {
            return Err(refuse(409, "This sign-in is not waiting for a code."));
        }
        if !auth::valid_input(text) {
            return Err(refuse(
                400,
                "That does not look like the code the provider showed.",
            ));
        }
        s.control
            .send(auth::Control::Input(text.into()))
            .map_err(|_| refuse(410, "This sign-in has ended; start a new sign-in."))?;
        Ok(json!({"session": Self::member_session_view(&s.public)}))
    }
    /// `POST /v1/projects/{p}/me/credentials/{id}/sessions/{session}/cancel {}`.
    pub fn member_login_cancel(
        &self,
        org: &str,
        member: &str,
        id: &str,
        session: &str,
    ) -> Result<Value> {
        self.member_owned(org, member, id)?;
        let public = self
            .login_cancel(org, id, session)
            .map_err(|_| refuse(404, "Unknown sign-in."))?;
        Ok(json!({"session": Self::member_session_view(&public)}))
    }
    /// `POST /v1/projects/{p}/me/credentials/{id}/delete {}`: the record,
    /// its home (the provider's tokens) and any sign-in in progress.
    pub fn member_delete(&self, org: &str, member: &str, id: &str) -> Result<Value> {
        self.member_owned(org, member, id)?;
        self.delete(org, id)
    }

    /// The credential a member run uses (docs/MEMBER-SUBSCRIPTIONS.md): the
    /// member's **own** connected personal credential for `provider` in
    /// `org`, the one named by `requested` when given. Nothing else is ever
    /// eligible: no shared, team, key, controller or other member's
    /// credential, and no fallback. None: `409 credential_required`.
    pub fn member_select(
        &self,
        org: &str,
        member: &str,
        provider: &str,
        requested: Option<&str>,
    ) -> Result<String> {
        if !valid_user(member) {
            return Err(refuse(
                403,
                "Only a member's own subscription runs their prompts.",
            ));
        }
        let own: Vec<String> = self
            .candidates(org, provider, Some(member))
            .into_iter()
            .filter(|c| c.reason == "own" && c.kind == Kind::Personal)
            .map(|c| c.id)
            .collect();
        let chosen = match requested {
            Some(id) => own.into_iter().find(|c| c == id),
            None => own.into_iter().next(),
        };
        chosen.ok_or_else(|| {
            refuse(
                409,
                "credential_required: connect your own subscription for this provider first.",
            )
        })
    }
    /// A member run's access material, read at admission from the run's own
    /// credential only: the record must still exist, be connected, belong to
    /// `run.member` and be for `provider`. Access material only (a Codex
    /// `auth.json` with its refresh token removed, or Claude's access token).
    /// It must cover the whole `timeout_seconds`; this release refuses
    /// otherwise (central renewal first arrives with the full-login change).
    pub fn member_material(
        &self,
        run: &enginebox_agents::queue::MemberRun,
        provider: &str,
        timeout_seconds: u64,
    ) -> Result<enginebox_agents::member::Delegated> {
        let record = self
            .records
            .lock()
            .unwrap()
            .get(&run.credential_id)
            .cloned()
            .filter(|c| {
                c.kind == Kind::Personal
                    && c.owner_user_id.as_deref() == Some(run.member.as_str())
                    && c.provider == provider
            })
            .context("The run's own credential is gone; reconnect the subscription.")?;
        ensure!(
            record.status == Status::Connected,
            "The run's own credential is not connected; reconnect the subscription."
        );
        let home = self.home(&record.id)?;
        let material = enginebox_agents::delegation::home_material(&home, provider)?.material;
        let delegated = enginebox_agents::member::Delegated::from_material(provider, &material)?;
        // The access must outlast the whole run: a native client keeps the
        // token it started with, so it cannot be renewed mid-run.
        if let Some(at) = delegated.expires_at {
            ensure!(
                at > chrono::Utc::now().timestamp() as f64 + timeout_seconds as f64 + 60.,
                "The subscription's access expires before this run's timeout; reconnect it or shorten the timeout."
            );
        }
        Ok(delegated)
    }

    /// Tests: complete a sign-in by writing the provider's file into the
    /// credential's home and marking it connected.
    #[cfg(test)]
    pub(crate) fn connect_for_test(&self, id: &str, provider_file: &Value) {
        let home = self.home(id).unwrap();
        let provider = self.records.lock().unwrap()[id].provider.clone();
        security::write_json(
            &home,
            enginebox_agents::delegation::native_file(&provider),
            provider_file,
        )
        .unwrap();
        self.records.lock().unwrap().get_mut(id).unwrap().status = Status::Connected;
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

    #[test]
    fn a_member_sees_and_changes_only_their_own_subscriptions() {
        let (temp, c) = open();
        let (created, alice) = c
            .member_create(ORG, ALICE, &json!({"provider":"claude"}))
            .unwrap();
        assert!(created);
        let alice = alice["credential"]["id"].as_str().unwrap().to_owned();
        // One per provider: connecting again returns the same credential.
        let (created, again) = c
            .member_create(ORG, ALICE, &json!({"provider":"claude","label":"Mine"}))
            .unwrap();
        assert!(!created);
        assert_eq!(again["credential"]["id"], alice.as_str());
        let bob = c
            .member_create(ORG, BOB, &json!({"provider":"claude"}))
            .unwrap()
            .1["credential"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_ne!(alice, bob);
        // An org team account and key are never a member's to see or use.
        let team = c
            .create(ORG, &json!({"provider":"claude","kind":"team","label":"t"}))
            .unwrap()["credential"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let listed = |member| {
            c.member_list(ORG, member)["credentials"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v["id"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(listed(ALICE), vec![alice.clone()]);
        assert_eq!(listed(BOB), vec![bob.clone()]);
        assert!(
            c.member_list(OTHER, ALICE)["credentials"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        // Bob cannot sign in to, poll, answer, cancel or delete Alice's (404,
        // the same as an unknown ID), nor reach the team account.
        for id in [&alice, &team] {
            for result in [
                c.member_login(ORG, BOB, id),
                c.member_login_status(ORG, BOB, id, &"0".repeat(32)),
                c.member_login_input(ORG, BOB, id, &"0".repeat(32), &json!({"text":"code"})),
                c.member_login_cancel(ORG, BOB, id, &"0".repeat(32)),
                c.member_delete(ORG, BOB, id),
            ] {
                assert_eq!(refusal(result.unwrap_err()).0, 404);
            }
        }
        assert_eq!(
            refusal(c.member_delete(OTHER, ALICE, &alice).unwrap_err()).0,
            404
        );
        assert!(c.get(ORG, &alice).is_some());
        // Never shareable, never a key, never another provider or owner.
        assert!(c.share(ORG, &alice, &json!({"share":true})).is_err());
        assert!(!c.get(ORG, &alice).unwrap().share_with_org_pools);
        for bad in [
            json!({"provider":"claude","share_with_org_pools":true}),
            json!({"provider":"claude","key":"sk-0123456789"}),
            json!({"provider":"claude","owner_user_id":BOB}),
            json!({"provider":"cursor"}),
            json!({"provider":"unity"}),
            json!({"label":"x"}),
        ] {
            assert_eq!(
                refusal(c.member_create(ORG, ALICE, &bad).unwrap_err()).0,
                400,
                "{bad}"
            );
        }
        assert_eq!(
            refusal(
                c.member_create(ORG, "owner", &json!({"provider":"claude"}))
                    .unwrap_err()
            )
            .0,
            403
        );
        // The marker survives a restart, so the sharing lock does too.
        let reopened =
            Credentials::open(&temp.path().canonicalize().unwrap(), &BTreeMap::new()).unwrap();
        assert!(reopened.is_member_connected(&alice));
        assert!(reopened.share(ORG, &alice, &json!({"share":true})).is_err());
        // Alice removes her own; Bob's stays.
        c.member_delete(ORG, ALICE, &alice).unwrap();
        assert!(c.get(ORG, &alice).is_none());
        assert!(!c.is_member_connected(&alice));
        assert!(
            !temp
                .path()
                .join(format!("agent-credentials/member/{alice}.json"))
                .exists()
        );
        assert!(c.get(ORG, &bob).is_some());
    }

    #[test]
    fn an_app_reaches_only_what_it_connected_within_limits() {
        let (temp, c) = open();
        // Alice's own subscription made on the dashboard (and shared).
        let dashboard = c
            .create(
                ORG,
                &json!({"provider":"claude","kind":"personal","label":"d","owner_user_id":ALICE}),
            )
            .unwrap()["credential"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        c.share(ORG, &dashboard, &json!({"share":true})).unwrap();
        // The app never adopts it: connecting makes a separate one.
        let (created, app) = c
            .member_create(ORG, ALICE, &json!({"provider":"claude"}))
            .unwrap();
        assert!(created);
        let app = app["credential"]["id"].as_str().unwrap().to_owned();
        assert_ne!(app, dashboard);
        // Nor can the app list, sign in to or delete the dashboard one.
        let listed = c.member_list(ORG, ALICE)["credentials"].clone();
        assert_eq!(listed.as_array().unwrap().len(), 1);
        assert_eq!(listed[0]["id"], app.as_str());
        assert_eq!(
            refusal(c.member_delete(ORG, ALICE, &dashboard).unwrap_err()).0,
            404
        );
        assert_eq!(
            refusal(c.member_login(ORG, ALICE, &dashboard).unwrap_err()).0,
            404
        );
        // The listing marks app-connected credentials for the dashboard.
        let all = c.list(ORG)["credentials"].clone();
        let flag = |id: &str| {
            all.as_array()
                .unwrap()
                .iter()
                .find(|v| v["id"] == id)
                .unwrap()["is_member_connected"]
                .clone()
        };
        assert_eq!(flag(&app), true);
        assert_eq!(flag(&dashboard), false);
        // Per-member limit: one member cannot exhaust the org.
        for i in 0..20 {
            let made = c.create(ORG, &json!({"provider":"codex","kind":"personal","label":format!("x{i}"),"owner_user_id":BOB}));
            if i < MAX_CREDENTIALS_PER_MEMBER {
                made.unwrap();
            } else {
                assert_eq!(refusal(made.unwrap_err()).0, 409);
            }
        }
        // Alice is not affected by Bob's limit.
        c.member_create(ORG, ALICE, &json!({"provider":"codex"}))
            .unwrap();
        // An orphaned marker (a deleted record) is swept on open.
        let orphan = temp
            .path()
            .join(format!("agent-credentials/member/{}.json", "e".repeat(32)));
        state::write_json(&orphan, &json!({"id": "e".repeat(32)})).unwrap();
        Credentials::open(&temp.path().canonicalize().unwrap(), &BTreeMap::new()).unwrap();
        assert!(!orphan.exists());
    }

    #[test]
    fn sign_in_input_errors_say_what_to_do() {
        let (_temp, c) = open();
        let id = c
            .member_create(ORG, ALICE, &json!({"provider":"claude"}))
            .unwrap()
            .1["credential"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let session = "0".repeat(32);
        // A bad body is a 400 before anything else; an unknown sign-in a 404.
        assert_eq!(
            refusal(
                c.member_login_input(ORG, ALICE, &id, &session, &json!({"code":"x"}))
                    .unwrap_err()
            )
            .0,
            400
        );
        assert_eq!(
            refusal(
                c.member_login_input(ORG, ALICE, &id, &session, &json!({"text":"abcdefgh12"}))
                    .unwrap_err()
            )
            .0,
            404
        );
        // An ended sign-in is 410, one not waiting for a code is 409.
        let (tx, _rx) = mpsc::channel();
        let add = |status: &str, input: bool| {
            let key = uuid::Uuid::new_v4().simple().to_string();
            c.sessions.lock().unwrap().insert(
                key.clone(),
                Session {
                    credential: id.clone(),
                    org: ORG.into(),
                    owner: Some(ALICE.into()),
                    public: json!({"id":key,"provider":"claude","status":status,"input_required":input}),
                    control: tx.clone(),
                    cancel: Arc::new(AtomicBool::new(false)),
                },
            );
            key
        };
        let expired = add("expired", false);
        let waiting = add("waiting", false);
        let asking = add("waiting", true);
        let text = json!({"text":"abcdefgh12"});
        assert_eq!(
            refusal(
                c.member_login_input(ORG, ALICE, &id, &expired, &text)
                    .unwrap_err()
            )
            .0,
            410
        );
        assert_eq!(
            refusal(
                c.member_login_input(ORG, ALICE, &id, &waiting, &text)
                    .unwrap_err()
            )
            .0,
            409
        );
        assert_eq!(
            refusal(
                c.member_login_input(
                    ORG,
                    ALICE,
                    &id,
                    &asking,
                    &json!({"text":"bad code with spaces"})
                )
                .unwrap_err()
            )
            .0,
            400
        );
        c.member_login_input(ORG, ALICE, &id, &asking, &text)
            .unwrap();
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
