//! Engine sessions, v1 (docs/ENGINE-SESSIONS.md, "What v1 implements"): a
//! member starts the Unity Editor for a repository on their own Windows engine
//! machine and gets a MoQ stream back.
//!
//! The controller is the front door, shaped like branch previews: the request
//! names a repository, ref, project path and engine version, never a host,
//! command or image. A session records the member who started it and is
//! placed only onto an online engine machine of that member in the same org
//! (personal machines run only their owner's sessions). The node reaches its
//! sessions over a channel parallel to the single job slot (`engine-poll` /
//! `engine-report`, never `active`), fenced by the node's epoch and the
//! session's generation. Per-session secrets (a repository read token, the
//! member's Unity access token, the relay publish token and a control key) are
//! minted on demand, held only in memory and handed only to the node and
//! epoch the session is placed on. They are never written to a record.
//!
//! Callers today: the owner token, and members through the dashboard (the
//! owner token plus `X-EngineBox-Org` / `X-EngineBox-Actor`).
//! TODO(engine-sessions OAuth): a service-token grant
//! `{kind:"engine_sessions", project}` with operations
//! `engine_sessions.start|get|renew|end` and the `engine-sessions` OAuth scope,
//! so Gamejam can start sessions as an OAuth client (design note in
//! docs/ENGINE-SESSIONS.md, "OAuth").
use anyhow::{Context, Result, ensure};
use enginebox_core::state;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use subtle::ConstantTimeEq;

pub const MIN_TTL_SECONDS: u64 = 60;
pub const MAX_TTL_SECONDS: u64 = 4 * 3600;
pub const DEFAULT_TTL_SECONDS: u64 = 3600;
/// A stopping session whose node never confirms is closed after this long.
pub const ENDING_GRACE_SECONDS: u64 = 120;
/// Most retained session records (terminal ones included).
pub const MAX_SESSIONS: usize = 5000;
/// Most sessions one member holds open at once.
pub const MAX_OPEN_PER_MEMBER: usize = 4;
/// The platform org's `secret_source` connection holding relay tokens.
pub const MOQ_CONNECTION: &str = "cloudflare-moq";
pub const MOQ_PUBLISH: &str = "MOQ_PUBLISH_TOKEN";
pub const MOQ_SUBSCRIBE: &str = "MOQ_SUBSCRIBE_TOKEN";
/// Optional: a relay token with publish rights the owner lets members'
/// browsers use to control their own sessions (the stream host stops after
/// about 15 s without a controlling client's liveness).
pub const MOQ_CONTROL: &str = "MOQ_CONTROL_TOKEN";
/// The relay the stream host publishes to (it is fixed in the stream host).
pub const DEFAULT_RELAY: &str = "https://draft-16.cloudflare.mediaoverquic.com";
/// v1 fallback for repositories Engine Box neither hosts nor links: an
/// org `secret_source` connection `engine-repo-<name>` yielding the remote and
/// a read token (sent as GitHub-style Basic `x-access-token`).
pub const REPOSITORY_CONNECTION_PREFIX: &str = "engine-repo-";
pub const REPOSITORY_REMOTE: &str = "GIT_REMOTE_URL";
pub const REPOSITORY_TOKEN: &str = "GIT_READ_TOKEN";
pub fn relay_missing() -> anyhow::Error {
    refuse(
        409,
        "The platform has no cloudflare-moq connection with MOQ_PUBLISH_TOKEN and MOQ_SUBSCRIBE_TOKEN.",
    )
}
/// An https remote without credentials, query or fragment.
pub fn check_remote(remote: &str) -> Result<()> {
    let url = url::Url::parse(remote).map_err(|_| refuse(409, "GIT_REMOTE_URL is not a URL."))?;
    ensure!(
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.host_str().is_some(),
        refuse(
            409,
            "GIT_REMOTE_URL must be an https URL without credentials."
        )
    );
    Ok(())
}
/// Repository read token lifetime handed to a node.
pub const REPOSITORY_TOKEN_TTL: u64 = 3600;
pub const NO_UNITY_ACCOUNT: &str =
    "Add your Unity account on the Agents page to run Unity engine sessions.";
pub const NO_ENGINE_MACHINE: &str = "You have no engine machine in this organization. Register a Windows engine machine on the Compute page, then claim it as yours.";

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
fn random_hex(bytes: usize) -> String {
    let mut out = String::new();
    while out.len() < bytes * 2 {
        out.push_str(&uuid::Uuid::new_v4().simple().to_string());
    }
    out.truncate(bytes * 2);
    out
}
fn equal(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

/// A caller-facing refusal with its HTTP status.
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
fn refuse(status: u16, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(Refusal {
        status,
        message: message.into(),
    })
}
/// The status and message of an error for a caller.
pub fn status_of(error: &anyhow::Error) -> (u16, String) {
    match error.downcast_ref::<Refusal>() {
        Some(r) => (r.status, r.message.clone()),
        None => (400, crate::catalog::message(error)),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Queued,
    Placed,
    Preparing,
    Starting,
    Ready,
    Ending,
    Ended,
    Failed,
    Lost,
}
impl State {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Ended | Self::Failed | Self::Lost)
    }
    /// Held by a node: it may be running engine processes.
    fn on_node(self) -> bool {
        matches!(
            self,
            Self::Placed | Self::Preparing | Self::Starting | Self::Ready | Self::Ending
        )
    }
    fn rank(self) -> u8 {
        match self {
            Self::Queued => 0,
            Self::Placed => 1,
            Self::Preparing => 2,
            Self::Starting => 3,
            Self::Ready => 4,
            Self::Ending => 5,
            Self::Ended | Self::Failed | Self::Lost => 6,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Engine {
    pub kind: String,
    pub version: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    pub id: String,
    pub org_id: String,
    pub project: String,
    pub request_id: String,
    pub request_digest: String,
    pub repository: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub project_path: String,
    pub engine: Engine,
    pub width: u32,
    pub height: u32,
    pub started_by: String,
    pub state: State,
    #[serde(default)]
    pub error: Option<String>,
    pub lease: String,
    pub lease_expires_at: u64,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default)]
    pub node: Option<String>,
    #[serde(default)]
    pub node_epoch: u64,
    #[serde(default)]
    pub generation: u64,
    pub namespace: String,
    /// The stream host's MoQ session tag (a broadcast generation number).
    pub tag: u32,
    /// A scene the editor opens first (`Assets/….unity`).
    #[serde(default)]
    pub scene: Option<String>,
    /// The per-session key the controlling client and the stream host share.
    pub control_key: String,
    /// What the node reported (commit, timings, process ids); never secrets.
    #[serde(default)]
    pub report: Value,
    #[serde(default)]
    pub ending_since: Option<u64>,
    #[serde(default)]
    pub ended_at: Option<u64>,
}
impl Session {
    /// What any caller who may see the session reads. No lease, control key
    /// or token; the creating member's own `stream` block is added separately.
    pub fn public(&self) -> Value {
        json!({
            "id": self.id, "project": self.project, "request_id": self.request_id,
            "repository": self.repository, "ref": self.git_ref, "project_path": self.project_path,
            "engine": self.engine, "width": self.width, "height": self.height,
            "scene": self.scene, "started_by": self.started_by, "state": self.state, "error": self.error,
            "lease_expires_at": self.lease_expires_at, "created_at": self.created_at,
            "updated_at": self.updated_at, "node": self.node, "generation": self.generation,
            "commit": self.report.get("commit"), "ended_at": self.ended_at,
        })
    }
}

/// One registered engine machine as placement sees it.
#[derive(Clone, Debug, Default)]
pub struct NodeView {
    pub id: String,
    pub org: String,
    /// The member whose personal machine this is.
    pub engine_owner: Option<String>,
    pub engine: bool,
    pub online: bool,
    pub revoked: bool,
    pub epoch: u64,
    /// Unity editor versions the node reports installed (`tools.engine.unity`).
    pub unity_versions: Vec<String>,
}
impl NodeView {
    fn serves(&self, org: &str, member: &str) -> bool {
        !self.revoked
            && self.engine
            && self.org == org
            && self.engine_owner.as_deref() == Some(member)
    }
}

/// A repository read credential for a node.
#[derive(Clone)]
pub struct RepositoryAccess {
    pub remote: String,
    pub token: String,
    /// `bearer` or `github_token` (Basic `x-access-token:<token>`).
    pub auth: String,
    pub expires_at: String,
}
/// Relay tokens from the platform's `cloudflare-moq` connection.
#[derive(Clone)]
pub struct Relay {
    pub url: String,
    pub publish_token: String,
    pub subscribe_token: String,
    pub control_token: Option<String>,
}

/// What the controller provides to sessions; the runtime implements it over
/// contributors, tenancy, repositories, credentials and connections.
pub trait Platform {
    fn nodes(&self) -> Vec<NodeView>;
    /// Whether `repository` is readable for `org` (hosted, linked, or an
    /// owner-configured `engine-repo-<name>` connection).
    fn repository_known(&self, org: &str, project: &str, repository: &str) -> bool;
    fn repository_access(
        &self,
        org: &str,
        project: &str,
        repository: &str,
    ) -> Result<RepositoryAccess>;
    fn has_unity_account(&self, org: &str, member: &str) -> bool;
    fn unity_access_token(&self, org: &str, member: &str) -> Result<String>;
    fn relay(&self) -> Result<Relay>;
    fn audit(&self, org: &str, actor: &str, action: &str, target: &str, ok: bool, detail: Value);
}

pub fn valid_session_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub fn valid_version(v: &str) -> bool {
    // 6000.6.5f1, 2022.3.10f1, 6000.0.0b12
    let Some((numbers, suffix)) = v.split_once(|c: char| "abfpx".contains(c)) else {
        return false;
    };
    let parts: Vec<&str> = numbers.split('.').collect();
    v.len() <= 24
        && parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()))
        && !suffix.is_empty()
        && suffix.len() <= 4
        && suffix.bytes().all(|b| b.is_ascii_digit())
}
/// A project directory inside the repository: relative, `/`-separated, no
/// `..`, empty or `.` for the root.
pub fn valid_project_path(p: &str) -> bool {
    p.len() <= 200
        && !p.starts_with('/')
        && !p.contains('\\')
        && !p.contains(':')
        && p.split('/').all(|c| {
            c != ".."
                && (c.is_empty() || c == "." || c.len() <= 80)
                && c.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b" _.-".contains(&b))
        })
        && !p.split('/').any(|c| c.starts_with(' ') || c.ends_with(' '))
}
fn valid_scene(s: &str) -> bool {
    s.len() <= 200
        && s.starts_with("Assets/")
        && s.ends_with(".unity")
        && !s.contains("..")
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b" _./-".contains(&b))
}
/// The stream host's canonical session UUID for a session id.
pub fn session_uuid(id: &str) -> String {
    uuid::Uuid::parse_str(id)
        .map(|u| u.hyphenated().to_string())
        .unwrap_or_default()
}
fn valid_ref(r: &str) -> bool {
    crate::previews::valid_branch(r) && !r.starts_with('-')
}

#[derive(Clone)]
pub struct EngineSessions {
    root: PathBuf,
    sessions: Arc<Mutex<BTreeMap<String, Session>>>,
    /// Minted secrets per (session, generation), delivered while a session is
    /// `placed`; memory only.
    secrets: Arc<Mutex<BTreeMap<(String, u64), Value>>>,
}

impl EngineSessions {
    pub fn open(root: &Path) -> Result<Self> {
        let root = root.join("engine-sessions");
        state::private_dir(&root)?;
        let mut sessions = BTreeMap::new();
        for path in state::json_files(&root)? {
            let s: Session = serde_json::from_value(state::read_json(&path)?)?;
            ensure!(
                valid_session_id(&s.id) && path.file_stem().and_then(|f| f.to_str()) == Some(&s.id),
                "Invalid engine session record"
            );
            sessions.insert(s.id.clone(), s);
        }
        Ok(Self {
            root,
            sessions: Arc::new(Mutex::new(sessions)),
            secrets: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }
    fn save(&self, s: &Session) -> Result<()> {
        state::write_json(
            &self.root.join(format!("{}.json", s.id)),
            &serde_json::to_value(s)?,
        )
    }
    fn set_state(
        &self,
        s: &mut Session,
        next: State,
        error: Option<String>,
        platform: &dyn Platform,
    ) -> Result<()> {
        if s.state == next {
            return Ok(());
        }
        let t = now();
        s.state = next;
        s.updated_at = t;
        if error.is_some() {
            s.error = error;
        }
        if next == State::Ending {
            s.ending_since = Some(t);
        }
        if next.terminal() {
            s.ended_at = Some(t);
            self.secrets
                .lock()
                .unwrap()
                .retain(|(id, _), _| id != &s.id);
            platform.audit(
                &s.org_id,
                &s.started_by,
                "engine.session_end",
                &s.id,
                next == State::Ended,
                json!({"state": next, "repository": s.repository, "node": s.node, "reason": s.error}),
            );
        }
        self.save(s)
    }

    /// Expire leases, notice lost nodes and place queued sessions.
    fn tick(&self, sessions: &mut BTreeMap<String, Session>, platform: &dyn Platform) {
        let nodes = platform.nodes();
        let t = now();
        let ids: Vec<String> = sessions
            .values()
            .filter(|s| !s.state.terminal())
            .map(|s| s.id.clone())
            .collect();
        for id in &ids {
            let s = sessions.get_mut(id).unwrap();
            let node = s
                .node
                .as_ref()
                .and_then(|n| nodes.iter().find(|v| &v.id == n));
            let outcome = if s.state == State::Queued {
                (s.lease_expires_at <= t).then(|| {
                    (
                        State::Ended,
                        Some("The lease expired before an engine machine took the session.".into()),
                    )
                })
            } else if node.is_none_or(|n| n.revoked || !n.online || n.epoch != s.node_epoch) {
                Some((
                    State::Lost,
                    Some("The engine machine went offline or restarted.".into()),
                ))
            } else if s.state == State::Ending {
                s.ending_since
                    .filter(|since| since + ENDING_GRACE_SECONDS <= t)
                    .map(|_| {
                        (
                            State::Lost,
                            Some("The engine machine did not confirm the stop.".into()),
                        )
                    })
            } else if s.lease_expires_at <= t {
                Some((State::Ending, Some("The lease expired.".into())))
            } else {
                None
            };
            if let Some((next, error)) = outcome {
                let _ = self.set_state(s, next, error, platform);
            }
        }
        // Placement: oldest first, one session per engine machine.
        let mut queued: Vec<(u64, String)> = sessions
            .values()
            .filter(|s| s.state == State::Queued)
            .map(|s| (s.created_at, s.id.clone()))
            .collect();
        queued.sort();
        for (_, id) in queued {
            let busy: Vec<String> = sessions
                .values()
                .filter(|s| s.state.on_node())
                .filter_map(|s| s.node.clone())
                .collect();
            let s = sessions.get_mut(&id).unwrap();
            let Some(node) = nodes.iter().find(|n| {
                n.serves(&s.org_id, &s.started_by)
                    && n.online
                    && !busy.contains(&n.id)
                    && n.unity_versions.contains(&s.engine.version)
            }) else {
                continue;
            };
            s.node = Some(node.id.clone());
            s.node_epoch = node.epoch;
            s.generation += 1;
            let _ = self.set_state(s, State::Placed, None, platform);
        }
    }

    /// `POST /v1/projects/{project}/engine-sessions`.
    pub fn create(
        &self,
        org: &str,
        project: &str,
        actor: &str,
        body: &Value,
        platform: &dyn Platform,
    ) -> Result<(u16, Value)> {
        crate::keys(
            body,
            &[
                "request_id",
                "repository",
                "ref",
                "project_path",
                "engine",
                "ttl_seconds",
                "mode",
                "scene",
                "width",
                "height",
            ],
            &["request_id", "repository", "ref", "engine"],
        )
        .map_err(|_| refuse(400, "Send request_id, repository, ref, project_path, engine {kind, version}, and optionally ttl_seconds, width and height."))?;
        ensure!(
            enginebox_agents::pools::valid_project(project),
            refuse(404, "Unknown project.")
        );
        let request_id = body["request_id"].as_str().unwrap_or("");
        ensure!(
            enginebox_node_protocol::id(request_id),
            refuse(400, "request_id must be 1-80 letters, digits, - or _.")
        );
        let repository = body["repository"].as_str().unwrap_or("");
        ensure!(
            enginebox_agents::repository::valid_name(repository),
            refuse(400, "repository must name a repository of this project.")
        );
        let git_ref = body["ref"].as_str().unwrap_or("");
        ensure!(
            valid_ref(git_ref),
            refuse(400, "ref must be a branch name or a commit.")
        );
        let project_path = body["project_path"].as_str().unwrap_or("");
        ensure!(
            body.get("project_path").is_none_or(Value::is_string)
                && valid_project_path(project_path),
            refuse(
                400,
                "project_path must be a relative directory inside the repository."
            )
        );
        let engine: Engine = serde_json::from_value(body["engine"].clone())
            .map_err(|_| refuse(400, "engine must be {kind: \"unity\", version}."))?;
        ensure!(
            engine.kind == "unity" && valid_version(&engine.version),
            refuse(
                400,
                "engine must be {kind: \"unity\", version} with a Unity version such as 6000.6.5f1."
            )
        );
        ensure!(
            body.get("mode").is_none_or(|m| m == "editor"),
            refuse(400, "mode must be editor in this release.")
        );
        let ttl = match body.get("ttl_seconds") {
            None => DEFAULT_TTL_SECONDS,
            Some(v) => v
                .as_u64()
                .filter(|t| (MIN_TTL_SECONDS..=MAX_TTL_SECONDS).contains(t))
                .ok_or_else(|| {
                    refuse(
                        400,
                        format!("ttl_seconds must be {MIN_TTL_SECONDS}-{MAX_TTL_SECONDS}."),
                    )
                })?,
        };
        let dimension = |key: &str, default: u32, range: std::ops::RangeInclusive<u32>| match body
            .get(key)
        {
            None => Ok(default),
            Some(v) => v
                .as_u64()
                .and_then(|v| u32::try_from(v).ok())
                .filter(|v| range.contains(v) && v % 2 == 0)
                .ok_or_else(|| refuse(400, format!("{key} must be an even number in {range:?}."))),
        };
        // The stream host's and editor bridge's bounds.
        let width = dimension("width", 1280, 320..=1920)?;
        let height = dimension("height", 720, 180..=1080)?;
        let scene = match body.get("scene") {
            None | Some(Value::Null) => None,
            Some(v) => Some(
                v.as_str()
                    .filter(|s| valid_scene(s))
                    .ok_or_else(|| {
                        refuse(
                            400,
                            "scene must be a project path like Assets/Scenes/Main.unity.",
                        )
                    })?
                    .to_owned(),
            ),
        };
        let digest = state::digest(&json!({
            "project": project, "repository": repository, "ref": git_ref,
            "project_path": project_path, "engine": engine, "ttl_seconds": ttl,
            "width": width, "height": height, "scene": scene,
        }));
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(existing) = sessions
            .values()
            .find(|s| s.org_id == org && s.started_by == actor && s.request_id == request_id)
        {
            ensure!(
                existing.request_digest == digest,
                refuse(409, "request_id was already used for a different session.")
            );
            return Ok((202, self.created_view(existing)));
        }
        let audit_refusal = |message: &str| {
            platform.audit(
                org,
                actor,
                "engine.session_start",
                &format!("{project}/{repository}"),
                false,
                json!({"reason": message}),
            );
        };
        if !platform.repository_known(org, project, repository) {
            audit_refusal("Unknown repository.");
            return Err(refuse(404, "Unknown repository."));
        }
        if !platform.has_unity_account(org, actor) {
            audit_refusal(NO_UNITY_ACCOUNT);
            return Err(refuse(409, NO_UNITY_ACCOUNT));
        }
        if !platform.nodes().iter().any(|n| n.serves(org, actor)) {
            audit_refusal(NO_ENGINE_MACHINE);
            return Err(refuse(409, NO_ENGINE_MACHINE));
        }
        let open = sessions
            .values()
            .filter(|s| s.org_id == org && s.started_by == actor && !s.state.terminal())
            .count();
        ensure!(
            open < MAX_OPEN_PER_MEMBER,
            refuse(
                429,
                "End one of your engine sessions before starting another."
            )
        );
        if sessions.len() >= MAX_SESSIONS {
            // Forget the oldest finished records.
            let mut done: Vec<(u64, String)> = sessions
                .values()
                .filter(|s| s.state.terminal())
                .map(|s| (s.updated_at, s.id.clone()))
                .collect();
            done.sort();
            for (_, id) in done.into_iter().take(sessions.len() + 1 - MAX_SESSIONS) {
                sessions.remove(&id);
                let _ = std::fs::remove_file(self.root.join(format!("{id}.json")));
            }
            ensure!(
                sessions.len() < MAX_SESSIONS,
                refuse(429, "Too many engine sessions are open.")
            );
        }
        let t = now();
        let id = uuid::Uuid::new_v4().simple().to_string();
        let session = Session {
            id: id.clone(),
            org_id: org.to_owned(),
            project: project.to_owned(),
            request_id: request_id.to_owned(),
            request_digest: digest,
            repository: repository.to_owned(),
            git_ref: git_ref.to_owned(),
            project_path: project_path.trim_matches('/').to_owned(),
            engine,
            width,
            height,
            started_by: actor.to_owned(),
            state: State::Queued,
            error: None,
            lease: random_hex(32),
            lease_expires_at: t + ttl,
            created_at: t,
            updated_at: t,
            node: None,
            node_epoch: 0,
            generation: 0,
            namespace: format!("eb-{id}"),
            tag: u32::from_str_radix(&random_hex(4), 16).unwrap_or(1) & 0x7fff_ffff,
            scene,
            control_key: random_hex(32),
            report: json!({}),
            ending_since: None,
            ended_at: None,
        };
        self.save(&session)?;
        platform.audit(
            org,
            actor,
            "engine.session_start",
            &id,
            true,
            json!({"repository": format!("{project}/{repository}"), "ref": git_ref, "engine": session.engine}),
        );
        sessions.insert(id.clone(), session);
        self.tick(&mut sessions, platform);
        Ok((202, self.created_view(&sessions[&id])))
    }
    fn created_view(&self, s: &Session) -> Value {
        let mut v = s.public();
        v["lease"] = json!(s.lease);
        v
    }
    /// Whether `actor` may see and manage `s`: its starter, or the owner token
    /// acting without a member (`owner`).
    fn visible(s: &Session, org: &str, project: Option<&str>, actor: &str) -> bool {
        s.org_id == org
            && project.is_none_or(|p| s.project == p)
            && (actor == "owner" || s.started_by == actor)
    }
    /// `GET …/engine-sessions` (project) or `GET /v1/engine-sessions` (owner).
    pub fn list(
        &self,
        org: &str,
        project: Option<&str>,
        actor: &str,
        platform: &dyn Platform,
    ) -> Value {
        let mut sessions = self.sessions.lock().unwrap();
        self.tick(&mut sessions, platform);
        let mut rows: Vec<&Session> = sessions
            .values()
            .filter(|s| Self::visible(s, org, project, actor))
            .collect();
        rows.sort_by_key(|s| std::cmp::Reverse(s.created_at));
        json!({"sessions": rows.into_iter().take(200).map(Session::public).collect::<Vec<_>>()})
    }
    /// `GET …/engine-sessions/{id}`. The member who started a ready session
    /// also gets its `stream` block: relay, namespace, subscribe token and
    /// control key. Nobody else does, the owner token included.
    pub fn get(
        &self,
        org: &str,
        project: &str,
        actor: &str,
        id: &str,
        platform: &dyn Platform,
    ) -> Result<Value> {
        ensure!(valid_session_id(id), "Unknown engine session");
        let mut sessions = self.sessions.lock().unwrap();
        self.tick(&mut sessions, platform);
        let s = sessions
            .get(id)
            .filter(|s| Self::visible(s, org, Some(project), actor))
            .context("Unknown engine session")?;
        let mut v = s.public();
        if s.state == State::Ready && s.started_by == actor {
            let relay = match platform.relay() {
                Ok(relay) => relay,
                Err(error) => {
                    v["stream_error"] = json!(status_of(&error).1);
                    return Ok(v);
                }
            };
            v["stream"] = json!({
                "relay_url": relay.url,
                "url": format!("{}/{}", relay.url, relay.subscribe_token),
                "namespace": s.namespace,
                "session": session_uuid(&s.id),
                "tag": s.tag, "width": s.width, "height": s.height,
                "audience": {"token": relay.subscribe_token},
                "control": {"control_key": s.control_key, "token": relay.control_token},
            });
        }
        Ok(v)
    }
    /// `POST …/{id}/renew {lease, ttl_seconds}`.
    pub fn renew(
        &self,
        org: &str,
        project: &str,
        actor: &str,
        id: &str,
        body: &Value,
        platform: &dyn Platform,
    ) -> Result<(u16, Value)> {
        crate::keys(body, &["lease", "ttl_seconds"], &["lease", "ttl_seconds"])
            .map_err(|_| refuse(400, "Send lease and ttl_seconds."))?;
        let ttl = body["ttl_seconds"]
            .as_u64()
            .filter(|t| (MIN_TTL_SECONDS..=MAX_TTL_SECONDS).contains(t))
            .ok_or_else(|| {
                refuse(
                    400,
                    format!("ttl_seconds must be {MIN_TTL_SECONDS}-{MAX_TTL_SECONDS}."),
                )
            })?;
        let mut sessions = self.sessions.lock().unwrap();
        self.tick(&mut sessions, platform);
        let s = sessions
            .get_mut(id)
            .filter(|s| valid_session_id(id) && Self::visible(s, org, Some(project), actor))
            .ok_or_else(|| refuse(404, "Unknown engine session."))?;
        ensure!(
            equal(&s.lease, body["lease"].as_str().unwrap_or("")),
            refuse(409, "That lease does not hold this session.")
        );
        ensure!(
            !s.state.terminal() && s.state != State::Ending,
            refuse(409, "The session is ending or over; start a new one.")
        );
        s.lease_expires_at = now() + ttl;
        s.updated_at = now();
        self.save(s)?;
        Ok((200, s.public()))
    }
    /// `POST …/{id}/end` (project route, the starter or the owner) and
    /// `POST /v1/engine-sessions/{id}/end` (owner, `project` none).
    pub fn end(
        &self,
        org: Option<&str>,
        project: Option<&str>,
        actor: &str,
        id: &str,
        body: &Value,
        platform: &dyn Platform,
    ) -> Result<(u16, Value)> {
        crate::keys(body, &["lease"], &[]).map_err(|_| refuse(400, "Send {} or {lease}."))?;
        let mut sessions = self.sessions.lock().unwrap();
        self.tick(&mut sessions, platform);
        let s = sessions
            .get_mut(id)
            .filter(|s| {
                valid_session_id(id)
                    && match org {
                        Some(org) => Self::visible(s, org, project, actor),
                        None => true,
                    }
            })
            .ok_or_else(|| refuse(404, "Unknown engine session."))?;
        if let Some(lease) = body.get("lease") {
            ensure!(
                equal(&s.lease, lease.as_str().unwrap_or("")),
                refuse(409, "That lease does not hold this session.")
            );
        }
        let ender = actor.to_owned();
        match s.state {
            State::Queued => self.set_state(
                s,
                State::Ended,
                Some(format!("Ended by {ender}.")),
                platform,
            )?,
            st if st.on_node() && st != State::Ending => {
                self.set_state(s, State::Ending, None, platform)?
            }
            _ => {}
        }
        Ok((200, s.public()))
    }

    /// `engine-poll` from `node` at `epoch` (already authenticated and fenced
    /// by the contributor channel): the node's sessions with their desired
    /// state, and secrets for sessions it has not started yet.
    pub fn poll(&self, node: &str, epoch: u64, platform: &dyn Platform) -> Result<Value> {
        let wanted: Vec<Session> = {
            let mut sessions = self.sessions.lock().unwrap();
            self.tick(&mut sessions, platform);
            sessions
                .values()
                .filter(|s| {
                    s.node.as_deref() == Some(node) && s.node_epoch == epoch && s.state.on_node()
                })
                .cloned()
                .collect()
        };
        let mut rows = Vec::new();
        for s in wanted {
            let mut row = json!({
                "id": s.id, "generation": s.generation,
                "desired": if s.state == State::Ending { "stop" } else { "run" },
                "state": s.state,
                "lease_expires_at": s.lease_expires_at,
                "spec": {
                    "repository": format!("{}/{}", s.project, s.repository),
                    "ref": s.git_ref, "project_path": s.project_path,
                    "engine": s.engine, "width": s.width, "height": s.height,
                    "scene": s.scene, "namespace": s.namespace,
                    "session": session_uuid(&s.id), "tag": s.tag,
                },
            });
            if s.state == State::Placed {
                match self.secrets_for(&s, platform) {
                    Ok(secrets) => row["secrets"] = secrets,
                    Err(error) => {
                        let (_, message) = status_of(&error);
                        let mut sessions = self.sessions.lock().unwrap();
                        if let Some(live) = sessions
                            .get_mut(&s.id)
                            .filter(|l| l.state == State::Placed && l.generation == s.generation)
                        {
                            self.set_state(live, State::Failed, Some(message), platform)?;
                        }
                        continue;
                    }
                }
            }
            rows.push(row);
        }
        Ok(json!({"sessions": rows}))
    }
    fn secrets_for(&self, s: &Session, platform: &dyn Platform) -> Result<Value> {
        let key = (s.id.clone(), s.generation);
        if let Some(cached) = self.secrets.lock().unwrap().get(&key) {
            return Ok(cached.clone());
        }
        if !platform.has_unity_account(&s.org_id, &s.started_by) {
            return Err(refuse(409, NO_UNITY_ACCOUNT));
        }
        let unity = platform.unity_access_token(&s.org_id, &s.started_by)?;
        let repository = platform.repository_access(&s.org_id, &s.project, &s.repository)?;
        let relay = platform.relay()?;
        let secrets = json!({
            "repository": {"remote": repository.remote, "token": repository.token, "auth": repository.auth, "expires_at": repository.expires_at},
            "unity": {"access_token": unity},
            "moq": {"relay_url": relay.url, "publish_token": relay.publish_token, "control_key": s.control_key},
        });
        self.secrets.lock().unwrap().insert(key, secrets.clone());
        Ok(secrets)
    }
    /// `engine-report {id, generation, state, error?, detail?}` from `node` at
    /// `epoch`. Only the node, epoch and generation the session is placed on
    /// may move it, and only forward.
    pub fn report(
        &self,
        node: &str,
        epoch: u64,
        body: &Value,
        platform: &dyn Platform,
    ) -> Result<Value> {
        crate::keys(
            body,
            &[
                "session",
                "epoch",
                "id",
                "generation",
                "state",
                "error",
                "detail",
            ],
            &["id", "generation", "state"],
        )?;
        let id = body["id"].as_str().context("Session id required")?;
        let generation = body["generation"].as_u64().context("Generation required")?;
        let next: State = serde_json::from_value(body["state"].clone())?;
        ensure!(
            matches!(
                next,
                State::Preparing | State::Starting | State::Ready | State::Ended | State::Failed
            ),
            "A node reports preparing, starting, ready, ended or failed"
        );
        let error = match body.get("error") {
            None | Some(Value::Null) => None,
            Some(Value::String(e)) => Some(
                e.chars()
                    .filter(|c| !c.is_control())
                    .take(500)
                    .collect::<String>(),
            ),
            _ => anyhow::bail!("error must be a string"),
        };
        let detail = body.get("detail").cloned().unwrap_or(json!({}));
        ensure!(
            detail.is_object() && serde_json::to_vec(&detail)?.len() <= 4096,
            "Invalid session detail"
        );
        let mut sessions = self.sessions.lock().unwrap();
        let s = sessions
            .get_mut(id)
            .filter(|s| {
                s.node.as_deref() == Some(node)
                    && s.node_epoch == epoch
                    && s.generation == generation
            })
            .context("Session is not placed on this node, epoch and generation")?;
        if s.state == next || (s.state.terminal() && next.terminal()) {
            return Ok(json!({"ok": true, "state": s.state}));
        }
        ensure!(!s.state.terminal(), "Session is over");
        ensure!(
            next.terminal() || (s.state != State::Ending && next.rank() > s.state.rank()),
            "Session state only moves forward"
        );
        if let Some(object) = s.report.as_object_mut() {
            for (k, v) in detail.as_object().unwrap() {
                object.insert(k.clone(), v.clone());
            }
        }
        if next == State::Ready {
            s.report["ready_at"] = json!(now());
        }
        if next != State::Placed {
            self.secrets
                .lock()
                .unwrap()
                .retain(|(sid, _), _| sid != &s.id);
        }
        // A session the controller was stopping ends as asked, not as failed.
        let next = if s.state == State::Ending && next == State::Failed {
            State::Ended
        } else {
            next
        };
        self.set_state(s, next, error, platform)?;
        Ok(json!({"ok": true, "state": s.state}))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const ALICE: &str = "usr_aaaaaaaaaaaaaaaaaaaaaaaa";
    pub const BOB: &str = "usr_bbbbbbbbbbbbbbbbbbbbbbbb";
    pub const ORG: &str = "default";

    pub struct Fake {
        pub nodes: Mutex<Vec<NodeView>>,
        pub unity: Mutex<Vec<String>>,
        pub audit: Mutex<Vec<(String, bool)>>,
        pub relay_ok: bool,
    }
    impl Default for Fake {
        fn default() -> Self {
            Self {
                nodes: Mutex::new(vec![NodeView {
                    id: "a".repeat(32),
                    org: ORG.into(),
                    engine_owner: Some(ALICE.into()),
                    engine: true,
                    online: true,
                    revoked: false,
                    epoch: 1,
                    unity_versions: vec!["6000.6.5f1".into()],
                }]),
                unity: Mutex::new(vec![ALICE.into()]),
                audit: Mutex::new(vec![]),
                relay_ok: true,
            }
        }
    }
    impl Platform for Fake {
        fn nodes(&self) -> Vec<NodeView> {
            self.nodes.lock().unwrap().clone()
        }
        fn repository_known(&self, _: &str, project: &str, repository: &str) -> bool {
            project == "qoh" && repository == "queen-of-hearts"
        }
        fn repository_access(&self, _: &str, _: &str, _: &str) -> Result<RepositoryAccess> {
            Ok(RepositoryAccess {
                remote: "https://github.com/coop-collective/queen-of-hearts.git".into(),
                token: "repo-read-secret".into(),
                auth: "github_token".into(),
                expires_at: "2026-11-01T01:00:00Z".into(),
            })
        }
        fn has_unity_account(&self, _: &str, member: &str) -> bool {
            self.unity.lock().unwrap().iter().any(|m| m == member)
        }
        fn unity_access_token(&self, _: &str, _: &str) -> Result<String> {
            Ok("unity-access-secret".into())
        }
        fn relay(&self) -> Result<Relay> {
            ensure!(
                self.relay_ok,
                refuse(409, "Create the cloudflare-moq connection.")
            );
            Ok(Relay {
                url: DEFAULT_RELAY.into(),
                publish_token: "moq-publish-secret".into(),
                subscribe_token: "moq-subscribe-secret".into(),
                control_token: Some("moq-control-secret".into()),
            })
        }
        fn audit(&self, _: &str, _: &str, action: &str, _: &str, ok: bool, _: Value) {
            self.audit.lock().unwrap().push((action.into(), ok));
        }
    }
    pub fn start_body(request_id: &str) -> Value {
        json!({"request_id":request_id,"repository":"queen-of-hearts","ref":"main","project_path":"unity-project","engine":{"kind":"unity","version":"6000.6.5f1"},"ttl_seconds":600})
    }
    fn node() -> String {
        "a".repeat(32)
    }
    fn report(id: &str, generation: u64, state: &str) -> Value {
        json!({"id":id,"generation":generation,"state":state})
    }

    #[test]
    fn lifecycle_is_idempotent_placed_on_the_members_machine_and_secrets_stay_out_of_reads() {
        let dir = tempfile::tempdir().unwrap();
        let es = EngineSessions::open(dir.path()).unwrap();
        let p = Fake::default();
        let (status, created) = es.create(ORG, "qoh", ALICE, &start_body("r1"), &p).unwrap();
        assert_eq!(status, 202);
        assert_eq!(created["state"], "placed");
        assert_eq!(created["node"], node());
        let id = created["id"].as_str().unwrap().to_owned();
        let lease = created["lease"].as_str().unwrap().to_owned();
        // Idempotent on request_id; a different body under the same id conflicts.
        let (_, again) = es.create(ORG, "qoh", ALICE, &start_body("r1"), &p).unwrap();
        assert_eq!(again["id"], id.as_str());
        let mut changed = start_body("r1");
        changed["ref"] = json!("other");
        assert_eq!(
            status_of(&es.create(ORG, "qoh", ALICE, &changed, &p).unwrap_err()).0,
            409
        );
        // The node gets spec and secrets at its epoch only.
        let polled = es.poll(&node(), 1, &p).unwrap();
        let row = &polled["sessions"][0];
        assert_eq!(row["desired"], "run");
        assert_eq!(row["spec"]["repository"], "qoh/queen-of-hearts");
        assert_eq!(
            row["secrets"]["unity"]["access_token"],
            "unity-access-secret"
        );
        assert_eq!(row["secrets"]["moq"]["publish_token"], "moq-publish-secret");
        assert!(!row.to_string().contains("moq-subscribe-secret"));
        assert!(es.poll(&node(), 2, &p).unwrap()["sessions"][0].is_null());
        assert!(es.poll(&"b".repeat(32), 1, &p).unwrap()["sessions"][0].is_null());
        let generation = row["generation"].as_u64().unwrap();
        es.report(&node(), 1, &report(&id, generation, "preparing"), &p)
            .unwrap();
        // Secrets are delivered only until the node starts.
        assert!(es.poll(&node(), 1, &p).unwrap()["sessions"][0]["secrets"].is_null());
        es.report(&node(), 1, &report(&id, generation, "starting"), &p)
            .unwrap();
        // Reads never carry secrets; before ready there is no stream block.
        let read = es.get(ORG, "qoh", ALICE, &id, &p).unwrap();
        assert!(read.get("stream").is_none());
        let mut ready = report(&id, generation, "ready");
        ready["detail"] = json!({"commit":"c".repeat(40)});
        es.report(&node(), 1, &ready, &p).unwrap();
        let read = es.get(ORG, "qoh", ALICE, &id, &p).unwrap();
        assert_eq!(read["state"], "ready");
        assert_eq!(read["commit"], "c".repeat(40));
        assert_eq!(read["stream"]["audience"]["token"], "moq-subscribe-secret");
        assert_eq!(read["stream"]["control"]["token"], "moq-control-secret");
        assert_eq!(read["stream"]["namespace"], format!("eb-{id}"));
        assert_eq!(read["stream"]["session"], session_uuid(&id));
        let text = read.to_string();
        for secret in [
            "unity-access-secret",
            "repo-read-secret",
            "moq-publish-secret",
            &lease,
        ] {
            assert!(!text.contains(secret), "{secret}");
        }
        // The owner and other members never see the stream block.
        let owner = es.get(ORG, "qoh", "owner", &id, &p).unwrap();
        assert!(owner.get("stream").is_none());
        assert!(es.get(ORG, "qoh", BOB, &id, &p).is_err());
        assert!(
            es.get("org_0123456789abcdef01234567", "qoh", ALICE, &id, &p)
                .is_err()
        );
        // Records on disk hold no minted secret.
        let disk =
            std::fs::read_to_string(dir.path().join(format!("engine-sessions/{id}.json"))).unwrap();
        for secret in [
            "unity-access-secret",
            "repo-read-secret",
            "moq-publish-secret",
            "moq-subscribe-secret",
        ] {
            assert!(!disk.contains(secret));
        }
        // Renew needs the lease; end moves to ending, the node confirms.
        assert_eq!(
            status_of(
                &es.renew(
                    ORG,
                    "qoh",
                    ALICE,
                    &id,
                    &json!({"lease":"x","ttl_seconds":600}),
                    &p
                )
                .unwrap_err()
            )
            .0,
            409
        );
        es.renew(
            ORG,
            "qoh",
            ALICE,
            &id,
            &json!({"lease":lease,"ttl_seconds":1200}),
            &p,
        )
        .unwrap();
        assert!(
            es.end(Some(ORG), Some("qoh"), BOB, &id, &json!({}), &p)
                .is_err()
        );
        let (_, ending) = es
            .end(Some(ORG), Some("qoh"), ALICE, &id, &json!({}), &p)
            .unwrap();
        assert_eq!(ending["state"], "ending");
        assert_eq!(
            es.poll(&node(), 1, &p).unwrap()["sessions"][0]["desired"],
            "stop"
        );
        es.report(&node(), 1, &report(&id, generation, "ended"), &p)
            .unwrap();
        assert_eq!(
            es.get(ORG, "qoh", ALICE, &id, &p).unwrap()["state"],
            "ended"
        );
        assert!(es.poll(&node(), 1, &p).unwrap()["sessions"][0].is_null());
        let audit = p.audit.lock().unwrap().clone();
        assert!(audit.contains(&("engine.session_start".into(), true)));
        assert!(audit.contains(&("engine.session_end".into(), true)));
        // Restart keeps the record.
        let reopened = EngineSessions::open(dir.path()).unwrap();
        assert_eq!(
            reopened.get(ORG, "qoh", ALICE, &id, &p).unwrap()["state"],
            "ended"
        );
    }

    #[test]
    fn placement_is_only_onto_the_starting_members_online_engine_machine() {
        let dir = tempfile::tempdir().unwrap();
        let es = EngineSessions::open(dir.path()).unwrap();
        let p = Fake::default();
        p.unity.lock().unwrap().push(BOB.into());
        // Bob has no machine of his own: refused up front.
        let refused = es
            .create(ORG, "qoh", BOB, &start_body("b1"), &p)
            .unwrap_err();
        assert_eq!(status_of(&refused), (409, NO_ENGINE_MACHINE.into()));
        // A machine in another org, or not an engine machine, or another
        // member's, never takes Alice's session.
        {
            let mut nodes = p.nodes.lock().unwrap();
            nodes[0].org = "org_0123456789abcdef01234567".into();
            let mut wrong = nodes[0].clone();
            wrong.id = "c".repeat(32);
            wrong.org = ORG.into();
            wrong.engine_owner = Some(BOB.into());
            nodes.push(wrong);
            let mut not_engine = nodes[0].clone();
            not_engine.id = "d".repeat(32);
            not_engine.org = ORG.into();
            not_engine.engine = false;
            nodes.push(not_engine);
        }
        assert_eq!(
            status_of(
                &es.create(ORG, "qoh", ALICE, &start_body("a1"), &p)
                    .unwrap_err()
            )
            .0,
            409
        );
        // Her own machine offline: queued until it comes online.
        {
            let mut nodes = p.nodes.lock().unwrap();
            nodes[0].org = ORG.into();
            nodes[0].online = false;
        }
        let (_, s) = es.create(ORG, "qoh", ALICE, &start_body("a2"), &p).unwrap();
        assert_eq!(s["state"], "queued");
        // Missing editor version: still queued.
        p.nodes.lock().unwrap()[0].online = true;
        p.nodes.lock().unwrap()[0].unity_versions = vec!["2022.3.10f1".into()];
        assert_eq!(
            es.list(ORG, Some("qoh"), ALICE, &p)["sessions"][0]["state"],
            "queued"
        );
        p.nodes.lock().unwrap()[0].unity_versions = vec!["6000.6.5f1".into()];
        let id = s["id"].as_str().unwrap();
        assert_eq!(
            es.get(ORG, "qoh", ALICE, id, &p).unwrap()["state"],
            "placed"
        );
        assert_eq!(es.get(ORG, "qoh", ALICE, id, &p).unwrap()["node"], node());
        // One session per machine: a second waits.
        let (_, second) = es.create(ORG, "qoh", ALICE, &start_body("a3"), &p).unwrap();
        assert_eq!(second["state"], "queued");
        // Bob's own machine takes Bob's session.
        let (_, bob) = es.create(ORG, "qoh", BOB, &start_body("b2"), &p).unwrap();
        assert_eq!(bob["node"], "c".repeat(32));
        assert_eq!(
            es.list(ORG, Some("qoh"), BOB, &p)["sessions"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            es.list(ORG, None, "owner", &p)["sessions"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn missing_unity_account_is_refused_at_start_and_fails_at_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let es = EngineSessions::open(dir.path()).unwrap();
        let p = Fake::default();
        p.unity.lock().unwrap().clear();
        let refused = es
            .create(ORG, "qoh", ALICE, &start_body("u1"), &p)
            .unwrap_err();
        assert_eq!(status_of(&refused), (409, NO_UNITY_ACCOUNT.into()));
        assert!(
            p.audit
                .lock()
                .unwrap()
                .contains(&("engine.session_start".into(), false))
        );
        // Removed after the start: the session fails with the same reason.
        p.unity.lock().unwrap().push(ALICE.into());
        let (_, s) = es.create(ORG, "qoh", ALICE, &start_body("u2"), &p).unwrap();
        p.unity.lock().unwrap().clear();
        assert!(es.poll(&node(), 1, &p).unwrap()["sessions"][0].is_null());
        let read = es
            .get(ORG, "qoh", ALICE, s["id"].as_str().unwrap(), &p)
            .unwrap();
        assert_eq!(read["state"], "failed");
        assert_eq!(read["error"], NO_UNITY_ACCOUNT);
        // Unknown repositories and bad fields are refused.
        let mut other = start_body("u3");
        other["repository"] = json!("nope");
        p.unity.lock().unwrap().push(ALICE.into());
        assert_eq!(
            status_of(&es.create(ORG, "qoh", ALICE, &other, &p).unwrap_err()).0,
            404
        );
        for (key, value) in [
            ("project_path", json!("../x")),
            ("project_path", json!("C:/x")),
            ("ref", json!("--upload-pack=x")),
            ("engine", json!({"kind":"unreal","version":"5.4"})),
            ("engine", json!({"kind":"unity","version":"6000.6.5f1; rm"})),
            ("ttl_seconds", json!(5)),
            ("width", json!(1281)),
            ("width", json!(3840)),
            ("scene", json!("../x.unity")),
            ("host", json!("x")),
        ] {
            let mut bad = start_body("u4");
            bad[key] = value.clone();
            assert!(
                es.create(ORG, "qoh", ALICE, &bad, &p).is_err(),
                "{key} {value}"
            );
        }
    }

    #[test]
    fn epoch_and_generation_fence_reports_and_restarts_lose_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let es = EngineSessions::open(dir.path()).unwrap();
        let p = Fake::default();
        let (_, s) = es.create(ORG, "qoh", ALICE, &start_body("f1"), &p).unwrap();
        let id = s["id"].as_str().unwrap().to_owned();
        let generation = s["generation"].as_u64().unwrap();
        // Wrong epoch, wrong generation, wrong node: refused.
        assert!(
            es.report(&node(), 2, &report(&id, generation, "preparing"), &p)
                .is_err()
        );
        assert!(
            es.report(&node(), 1, &report(&id, generation + 1, "preparing"), &p)
                .is_err()
        );
        assert!(
            es.report(
                &"b".repeat(32),
                1,
                &report(&id, generation, "preparing"),
                &p
            )
            .is_err()
        );
        es.report(&node(), 1, &report(&id, generation, "starting"), &p)
            .unwrap();
        // Backwards is refused; a repeat is accepted.
        assert!(
            es.report(&node(), 1, &report(&id, generation, "preparing"), &p)
                .is_err()
        );
        es.report(&node(), 1, &report(&id, generation, "starting"), &p)
            .unwrap();
        // A node restart (new epoch) loses the session; old-epoch secrets are not re-sent.
        p.nodes.lock().unwrap()[0].epoch = 2;
        assert!(es.poll(&node(), 2, &p).unwrap()["sessions"][0].is_null());
        assert_eq!(es.get(ORG, "qoh", ALICE, &id, &p).unwrap()["state"], "lost");
        assert!(
            es.report(&node(), 1, &report(&id, generation, "ready"), &p)
                .is_err()
        );
        // Offline nodes lose their sessions too.
        let (_, s2) = es.create(ORG, "qoh", ALICE, &start_body("f2"), &p).unwrap();
        assert_eq!(s2["state"], "placed");
        p.nodes.lock().unwrap()[0].online = false;
        assert_eq!(
            es.get(ORG, "qoh", ALICE, s2["id"].as_str().unwrap(), &p)
                .unwrap()["state"],
            "lost"
        );
    }

    #[test]
    fn leases_expire_into_a_stop_and_unconfirmed_stops_are_closed() {
        let dir = tempfile::tempdir().unwrap();
        let es = EngineSessions::open(dir.path()).unwrap();
        let p = Fake::default();
        let (_, s) = es.create(ORG, "qoh", ALICE, &start_body("l1"), &p).unwrap();
        let id = s["id"].as_str().unwrap().to_owned();
        es.sessions
            .lock()
            .unwrap()
            .get_mut(&id)
            .unwrap()
            .lease_expires_at = 0;
        assert_eq!(
            es.poll(&node(), 1, &p).unwrap()["sessions"][0]["desired"],
            "stop"
        );
        assert_eq!(
            es.get(ORG, "qoh", ALICE, &id, &p).unwrap()["state"],
            "ending"
        );
        // Renewing an ending session is refused.
        let lease = s["lease"].as_str().unwrap();
        assert!(
            es.renew(
                ORG,
                "qoh",
                ALICE,
                &id,
                &json!({"lease":lease,"ttl_seconds":600}),
                &p
            )
            .is_err()
        );
        es.sessions
            .lock()
            .unwrap()
            .get_mut(&id)
            .unwrap()
            .ending_since = Some(0);
        assert_eq!(es.get(ORG, "qoh", ALICE, &id, &p).unwrap()["state"], "lost");
        // A queued session whose lease runs out ends without a node.
        p.nodes.lock().unwrap()[0].online = false;
        let (_, q) = es.create(ORG, "qoh", ALICE, &start_body("l2"), &p).unwrap();
        let qid = q["id"].as_str().unwrap().to_owned();
        es.sessions
            .lock()
            .unwrap()
            .get_mut(&qid)
            .unwrap()
            .lease_expires_at = 0;
        assert_eq!(
            es.get(ORG, "qoh", ALICE, &qid, &p).unwrap()["state"],
            "ended"
        );
        // A missing relay connection fails the session at delivery with its reason.
        let p = Fake {
            relay_ok: false,
            ..Fake::default()
        };
        let (_, r) = es.create(ORG, "qoh", ALICE, &start_body("l3"), &p).unwrap();
        es.poll(&node(), 1, &p).unwrap();
        let read = es.get(ORG, "qoh", ALICE, r["id"].as_str().unwrap(), &p);
        assert!(read.is_err() || read.unwrap()["state"] == "failed");
    }

    #[test]
    fn validators() {
        assert!(valid_version("6000.6.5f1") && valid_version("2022.3.10f1"));
        assert!(
            !valid_version("6000.6.5")
                && !valid_version("6000.6.5f")
                && !valid_version("../6000.6.5f1")
        );
        assert!(
            valid_project_path("unity-project")
                && valid_project_path("")
                && valid_project_path("a/b c")
        );
        assert!(
            !valid_project_path("../a") && !valid_project_path("/a") && !valid_project_path("a\\b")
        );
    }
}
