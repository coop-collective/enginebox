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
//! Records live under `agent-credentials/<id>.json`, homes under
//! `agent-credentials/homes/<id>/`, keys under `agent-credentials/keys/<id>.json`.
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
        Ok(Self {
            root: records_dir,
            binaries: Arc::new(binaries),
            records: Arc::new(Mutex::new(records)),
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
        })
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
