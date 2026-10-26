//! Scoped service tokens: owner-minted bearer credentials limited to agent-run
//! operations on named (host, workspace) pairs. They never authorize fleet
//! administration, provider sign-in, CI, storage or minting further tokens.
//! Only a SHA-256 digest of each secret is stored; the plaintext is returned once.
use anyhow::{Context, Result, ensure};
use enginebox_core::{identifier, state};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use subtle::ConstantTimeEq;

pub const PREFIX: &str = "ebst_";
const MAX_TOKENS: usize = 1000;
const MAX_SCOPES: usize = 256;
const MAX_LABEL: usize = 128;

/// Operations a service token may hold. Nothing else is grantable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Operation {
    #[serde(rename = "agent_runs.start")]
    Start,
    #[serde(rename = "agent_runs.get")]
    Get,
    #[serde(rename = "agent_runs.list")]
    List,
    #[serde(rename = "agent_runs.cancel")]
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub host_id: String,
    pub name: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Token {
    pub id: String,
    pub label: String,
    secret_hash: String,
    pub scopes: BTreeSet<Scope>,
    pub operations: BTreeSet<Operation>,
    pub created_at: String,
    pub expires_at: Option<String>,
    pub revoked_at: Option<String>,
    /// Profiles this token may name on a run. Empty: runs without a profile
    /// only. Omitted when empty so older releases still read the record.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub profiles: BTreeSet<String>,
    /// HMAC key that signs callbacks for this token's runs. Stored because it
    /// must sign; never returned after minting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    callback_secret: Option<String>,
    /// Pools whose runs this token may start, read and cancel (by its
    /// operations). Omitted when empty so older releases still read it.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub pools: BTreeSet<String>,
    /// When set, the token's pool grants hold only while each pool belongs to
    /// this project: checked at mint and again on every request. Omitted when
    /// unset so older releases still read the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}
impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the secret digest.
        f.debug_struct("Token")
            .field("id", &self.id)
            .field("label", &self.label)
            .finish()
    }
}
impl Token {
    pub fn principal(&self) -> String {
        format!("service-token:{}", self.id)
    }
    pub fn allows(&self, operation: Operation, host_id: &str, name: &str) -> bool {
        self.operations.contains(&operation) && self.sees(host_id, name)
    }
    /// True when this token may start a run naming `profile`.
    pub fn allows_profile(&self, profile: &str) -> bool {
        self.profiles.contains(profile)
    }
    pub fn callback_secret(&self) -> Option<&str> {
        self.callback_secret.as_deref()
    }
    /// `pool_project` is the pool's current project (None: unknown pool).
    pub fn allows_pool(
        &self,
        operation: Operation,
        pool: &str,
        pool_project: Option<&str>,
    ) -> bool {
        self.operations.contains(&operation) && self.sees_pool(pool, pool_project)
    }
    /// Granted, and inside the token's project restriction if it has one.
    pub fn sees_pool(&self, pool: &str, pool_project: Option<&str>) -> bool {
        self.pools.contains(pool)
            && self
                .project
                .as_deref()
                .is_none_or(|project| pool_project == Some(project))
    }
    pub fn sees(&self, host_id: &str, name: &str) -> bool {
        self.scopes
            .iter()
            .any(|s| s.host_id == host_id && s.name == name)
    }
    fn expired(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        self.expires_at
            .as_deref()
            .is_some_and(|at| chrono::DateTime::parse_from_rfc3339(at).map_or(true, |at| at <= now))
    }
    /// Public view: never includes the secret digest.
    pub fn public(&self) -> Value {
        let now = chrono::Utc::now();
        json!({
            "id":self.id,
            "label":self.label,
            "scopes":self.scopes,
            "operations":self.operations,
            "created_at":self.created_at,
            "expires_at":self.expires_at,
            "revoked_at":self.revoked_at,
            "profiles":self.profiles,
            "pools":self.pools,
            "project":self.project,
            "has_callback_secret":self.callback_secret.is_some(),
            "active":self.revoked_at.is_none() && !self.expired(now),
        })
    }
}

/// Body of `POST /v1/service-tokens`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MintRequest {
    pub label: String,
    pub scopes: Vec<Scope>,
    pub operations: Vec<Operation>,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub profiles: Vec<String>,
    #[serde(default)]
    pub callback_secret: Option<String>,
    #[serde(default)]
    pub pools: Vec<String>,
    /// Restrict the pool grants to this project's pools.
    #[serde(default)]
    pub project: Option<String>,
}

fn secret_hash(secret: &str) -> String {
    state::digest(&json!(secret))
}
fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Why a presented service token was refused (both answer HTTP 401).
#[derive(Debug, PartialEq)]
pub enum Refusal {
    Unknown,
    Inactive,
}

#[derive(Clone)]
pub struct ServiceTokens {
    root: PathBuf,
    tokens: Arc<Mutex<BTreeMap<String, Token>>>,
}
impl ServiceTokens {
    pub fn open(root: &Path) -> Result<Self> {
        let root = root.join("service-tokens");
        state::private_dir(&root)?;
        let mut tokens = BTreeMap::new();
        for path in state::json_files(&root)? {
            let token: Token = serde_json::from_value(state::read_json(&path)?)?;
            ensure!(
                valid_id(&token.id)
                    && path.file_stem().and_then(|s| s.to_str()) == Some(&token.id)
                    && enginebox_node_protocol::hash(&token.secret_hash),
                "Invalid service token record"
            );
            tokens.insert(token.id.clone(), token);
        }
        Ok(Self {
            root,
            tokens: Arc::new(Mutex::new(tokens)),
        })
    }
    fn save(&self, token: &Token) -> Result<()> {
        state::write_json(
            &self.root.join(format!("{}.json", token.id)),
            &serde_json::to_value(token)?,
        )
    }
    /// Mint a token. `known_host` rejects scopes naming unregistered hosts.
    /// `known_profile` rejects profiles that do not exist; `pool_project`
    /// answers a pool's project (None: no such pool).
    pub fn mint(
        &self,
        request: MintRequest,
        known_host: impl Fn(&str) -> bool,
        known_profile: impl Fn(&str) -> bool,
        pool_project: impl Fn(&str) -> Option<String>,
    ) -> Result<Value> {
        let label = request.label.trim().to_owned();
        ensure!(
            !label.is_empty() && label.len() <= MAX_LABEL && !label.chars().any(char::is_control),
            "label must contain 1-128 printable characters"
        );
        ensure!(
            (!request.scopes.is_empty() || !request.pools.is_empty())
                && request.scopes.len() <= MAX_SCOPES,
            "scopes must name 1-256 workspaces, or pools at least one pool"
        );
        ensure!(
            request.pools.len() <= MAX_SCOPES
                && request
                    .pools
                    .iter()
                    .all(|p| enginebox_agents::pools::valid_name(p) && pool_project(p).is_some()),
            "pools must name at most 256 existing pools"
        );
        if let Some(project) = &request.project {
            ensure!(
                enginebox_agents::pools::valid_project(project),
                "project must be a project ID"
            );
            ensure!(
                !request.pools.is_empty(),
                "project restricts pool grants; name at least one pool"
            );
            ensure!(
                request
                    .pools
                    .iter()
                    .all(|p| pool_project(p).as_deref() == Some(project.as_str())),
                "Every granted pool must belong to the token's project"
            );
        }
        for scope in &request.scopes {
            identifier(&scope.host_id, false)?;
            identifier(&scope.name, true)?;
            ensure!(known_host(&scope.host_id), "Unknown host in scope");
        }
        ensure!(
            !request.operations.is_empty(),
            "operations must name at least one agent-run operation"
        );
        ensure!(
            request.profiles.len() <= MAX_SCOPES
                && request
                    .profiles
                    .iter()
                    .all(|p| enginebox_agents::profile::valid_name(p) && known_profile(p)),
            "profiles must name at most 256 existing profiles"
        );
        if let Some(secret) = &request.callback_secret {
            ensure!(
                enginebox_agents::profile::valid_secret(secret),
                "callback_secret must be 32-256 printable ASCII characters without spaces"
            );
        }
        let now = chrono::Utc::now();
        let expires_at = match request.expires_at {
            Some(at) => {
                let at = chrono::DateTime::parse_from_rfc3339(&at)
                    .context("expires_at must be RFC 3339")?
                    .with_timezone(&chrono::Utc);
                ensure!(at > now, "expires_at must be in the future");
                Some(at.to_rfc3339())
            }
            None => None,
        };
        let mut tokens = self.tokens.lock().unwrap();
        if tokens.len() >= MAX_TOKENS {
            self.prune_inactive(&mut tokens, now);
        }
        ensure!(tokens.len() < MAX_TOKENS, "Service token limit reached");
        let id = uuid::Uuid::new_v4().simple().to_string();
        let secret = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let token = Token {
            id: id.clone(),
            label,
            secret_hash: secret_hash(&secret),
            scopes: request.scopes.into_iter().collect(),
            operations: request.operations.into_iter().collect(),
            created_at: now.to_rfc3339(),
            expires_at,
            revoked_at: None,
            profiles: request.profiles.into_iter().collect(),
            callback_secret: request.callback_secret,
            pools: request.pools.into_iter().collect(),
            project: request.project,
        };
        self.save(&token)?;
        let public = token.public();
        tokens.insert(id.clone(), token);
        Ok(
            json!({"token":public,"secret":format!("{PREFIX}{id}.{secret}"),"secret_shown_once":true}),
        )
    }
    /// Short-lived tokens (for example OAuth access tokens renewed hourly) would
    /// otherwise exhaust the limit. Records revoked or expired for over a day are
    /// deleted; recent ones stay visible for audit.
    fn prune_inactive(
        &self,
        tokens: &mut BTreeMap<String, Token>,
        now: chrono::DateTime<chrono::Utc>,
    ) {
        let cutoff = now - chrono::Duration::days(1);
        let stale: Vec<String> = tokens
            .values()
            .filter(|t| {
                let revoked = t
                    .revoked_at
                    .as_deref()
                    .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok());
                let expired = t
                    .expires_at
                    .as_deref()
                    .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok());
                revoked.or(expired).is_some_and(|at| at <= cutoff)
            })
            .map(|t| t.id.clone())
            .collect();
        for id in stale {
            if std::fs::remove_file(self.root.join(format!("{id}.json"))).is_ok() {
                tokens.remove(&id);
            }
        }
    }
    pub fn list(&self) -> Value {
        let tokens = self.tokens.lock().unwrap();
        json!({"tokens":tokens.values().map(Token::public).collect::<Vec<_>>()})
    }
    /// The listing with each token's pool grants also grouped by project
    /// (`pool_grants: [{project, pools}]`; a deleted pool's project is null).
    pub fn list_grouped(&self, project_of: impl Fn(&str) -> Option<String>) -> Value {
        let tokens = self.tokens.lock().unwrap();
        let views: Vec<Value> = tokens
            .values()
            .map(|t| {
                let mut groups: BTreeMap<Option<String>, Vec<&str>> = BTreeMap::new();
                for pool in &t.pools {
                    groups.entry(project_of(pool)).or_default().push(pool);
                }
                let mut view = t.public();
                view["pool_grants"] = json!(
                    groups
                        .into_iter()
                        .map(|(project, pools)| json!({"project":project,"pools":pools}))
                        .collect::<Vec<_>>()
                );
                view
            })
            .collect();
        json!({"tokens":views})
    }
    pub fn revoke(&self, id: &str) -> Result<Value> {
        ensure!(valid_id(id), "Invalid service token ID");
        let mut tokens = self.tokens.lock().unwrap();
        let token = tokens.get_mut(id).context("Unknown service token")?;
        if token.revoked_at.is_none() {
            let mut revoked = token.clone();
            revoked.revoked_at = Some(chrono::Utc::now().to_rfc3339());
            self.save(&revoked)?;
            *token = revoked;
        }
        Ok(json!({"token":token.public()}))
    }
    /// Resolve `ebst_<id>.<secret>`; the digest comparison is constant time.
    pub fn authenticate(&self, presented: &str) -> std::result::Result<Token, Refusal> {
        let (id, secret) = presented
            .strip_prefix(PREFIX)
            .and_then(|rest| rest.split_once('.'))
            .ok_or(Refusal::Unknown)?;
        if !valid_id(id) || secret.is_empty() || secret.len() > 128 {
            return Err(Refusal::Unknown);
        }
        let tokens = self.tokens.lock().unwrap();
        let token = tokens.get(id).ok_or(Refusal::Unknown)?;
        let matches: bool = token
            .secret_hash
            .as_bytes()
            .ct_eq(secret_hash(secret).as_bytes())
            .into();
        if !matches {
            return Err(Refusal::Unknown);
        }
        if token.revoked_at.is_some() || token.expired(chrono::Utc::now()) {
            return Err(Refusal::Inactive);
        }
        Ok(token.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(expires_at: Option<String>) -> MintRequest {
        MintRequest {
            label: "ci".into(),
            scopes: vec![Scope {
                host_id: "worker".into(),
                name: "ebox-a".into(),
            }],
            operations: vec![Operation::Start, Operation::Get],
            expires_at,
            profiles: vec![],
            callback_secret: None,
            pools: vec![],
            project: None,
        }
    }
    #[test]
    fn a_project_restriction_limits_pool_grants_to_that_project() {
        let temp = tempfile::tempdir().unwrap();
        let tokens = ServiceTokens::open(temp.path()).unwrap();
        let projects = |p: &str| match p {
            "alpha-workers" => Some("alpha".to_owned()),
            "beta-workers" => Some("beta".to_owned()),
            _ => None,
        };
        let restricted = |pools: &[&str], project: Option<&str>| {
            let mut r = request(None);
            r.scopes.clear();
            r.pools = pools.iter().map(|p| p.to_string()).collect();
            r.project = project.map(str::to_owned);
            r
        };
        // Every granted pool must be in the project; a project needs pools.
        assert!(
            tokens
                .mint(
                    restricted(&["alpha-workers", "beta-workers"], Some("alpha")),
                    |_| true,
                    |_| true,
                    projects
                )
                .is_err()
        );
        assert!(
            tokens
                .mint(restricted(&[], Some("alpha")), |_| true, |_| true, projects)
                .is_err()
        );
        assert!(
            tokens
                .mint(
                    restricted(&["alpha-workers"], Some("Bad_Project")),
                    |_| true,
                    |_| true,
                    projects
                )
                .is_err()
        );
        let minted = tokens
            .mint(
                restricted(&["alpha-workers"], Some("alpha")),
                |_| true,
                |_| true,
                projects,
            )
            .unwrap();
        assert_eq!(minted["token"]["project"], "alpha");
        let id = minted["token"]["id"].as_str().unwrap();
        let token = tokens.tokens.lock().unwrap().get(id).unwrap().clone();
        assert!(token.allows_pool(Operation::Start, "alpha-workers", Some("alpha")));
        // A pool moved to another project is no longer reachable with it.
        assert!(!token.allows_pool(Operation::Start, "alpha-workers", Some("beta")));
        assert!(!token.sees_pool("alpha-workers", None));
        // Without a restriction, the grant ignores projects (as before).
        let open = tokens
            .mint(
                restricted(&["alpha-workers"], None),
                |_| true,
                |_| true,
                projects,
            )
            .unwrap();
        let id = open["token"]["id"].as_str().unwrap();
        let token = tokens.tokens.lock().unwrap().get(id).unwrap().clone();
        assert!(token.sees_pool("alpha-workers", Some("beta")));
        let stored = std::fs::read_to_string(
            temp.path()
                .join("service-tokens")
                .join(format!("{id}.json")),
        )
        .unwrap();
        assert!(
            !stored.contains("\"project\""),
            "unset project is omitted for older releases"
        );
    }
    #[test]
    fn secrets_are_hashed_shown_once_and_revocation_persists() {
        let temp = tempfile::tempdir().unwrap();
        let tokens = ServiceTokens::open(temp.path()).unwrap();
        let minted = tokens
            .mint(request(None), |h| h == "worker", |_| false, |_| None)
            .unwrap();
        let secret = minted["secret"].as_str().unwrap().to_owned();
        let id = minted["token"]["id"].as_str().unwrap().to_owned();
        let stored = std::fs::read_to_string(
            temp.path()
                .join("service-tokens")
                .join(format!("{id}.json")),
        )
        .unwrap();
        let raw_secret = secret.split_once('.').unwrap().1;
        assert!(!stored.contains(raw_secret));
        assert!(!tokens.list().to_string().contains(raw_secret));
        assert!(!tokens.list().to_string().contains("secret_hash"));
        let token = tokens.authenticate(&secret).unwrap();
        assert!(token.allows(Operation::Start, "worker", "ebox-a"));
        assert!(!token.allows(Operation::Cancel, "worker", "ebox-a"));
        assert!(!token.allows(Operation::Start, "worker", "ebox-b"));
        assert_eq!(
            tokens.authenticate(&format!("{secret}x")).unwrap_err(),
            Refusal::Unknown
        );
        tokens.revoke(&id).unwrap();
        assert_eq!(
            ServiceTokens::open(temp.path())
                .unwrap()
                .authenticate(&secret)
                .unwrap_err(),
            Refusal::Inactive
        );
    }
    #[test]
    fn profile_grants_and_callback_secrets_are_validated_and_never_listed() {
        let temp = tempfile::tempdir().unwrap();
        let tokens = ServiceTokens::open(temp.path()).unwrap();
        let mut unknown = request(None);
        unknown.profiles = vec!["verifier".into()];
        assert!(tokens.mint(unknown, |_| true, |_| false, |_| None).is_err());
        let mut weak = request(None);
        weak.callback_secret = Some("short".into());
        assert!(tokens.mint(weak, |_| true, |_| true, |_| None).is_err());
        let mut granted = request(None);
        granted.profiles = vec!["verifier".into()];
        let key = "k".repeat(40);
        granted.callback_secret = Some(key.clone());
        let minted = tokens
            .mint(granted, |_| true, |p| p == "verifier", |_| None)
            .unwrap();
        assert_eq!(minted["token"]["profiles"], json!(["verifier"]));
        assert_eq!(minted["token"]["has_callback_secret"], true);
        assert!(!minted.to_string().contains(&key));
        assert!(!tokens.list().to_string().contains(&key));
        let token = tokens
            .authenticate(minted["secret"].as_str().unwrap())
            .unwrap();
        assert!(token.allows_profile("verifier"));
        assert!(!token.allows_profile("coder"));
        assert_eq!(token.callback_secret(), Some(key.as_str()));
        // A token without grants or a key keeps the original record shape.
        let plain = tokens
            .mint(request(None), |_| true, |_| true, |_| None)
            .unwrap();
        let id = plain["token"]["id"].as_str().unwrap();
        let stored = std::fs::read_to_string(
            temp.path()
                .join("service-tokens")
                .join(format!("{id}.json")),
        )
        .unwrap();
        assert!(!stored.contains("profiles") && !stored.contains("callback_secret"));
    }
    #[test]
    fn minting_at_the_limit_prunes_tokens_inactive_for_over_a_day() {
        let temp = tempfile::tempdir().unwrap();
        let tokens = ServiceTokens::open(temp.path()).unwrap();
        let old = (chrono::Utc::now() - chrono::Duration::days(2)).to_rfc3339();
        let recent = chrono::Utc::now().to_rfc3339();
        {
            let mut map = tokens.tokens.lock().unwrap();
            for i in 0..MAX_TOKENS {
                let id = format!("{i:032x}");
                let token = Token {
                    id: id.clone(),
                    label: "old".into(),
                    secret_hash: secret_hash("x"),
                    scopes: BTreeSet::new(),
                    operations: BTreeSet::new(),
                    created_at: old.clone(),
                    expires_at: None,
                    // Half are long revoked; the rest were revoked moments ago.
                    revoked_at: Some(if i % 2 == 0 {
                        old.clone()
                    } else {
                        recent.clone()
                    }),
                    profiles: BTreeSet::new(),
                    callback_secret: None,
                    pools: BTreeSet::new(),
                    project: None,
                };
                tokens.save(&token).unwrap();
                map.insert(id, token);
            }
        }
        tokens
            .mint(request(None), |_| true, |_| true, |_| None)
            .unwrap();
        let map = tokens.tokens.lock().unwrap();
        assert_eq!(map.len(), MAX_TOKENS / 2 + 1);
        assert!(
            !temp
                .path()
                .join("service-tokens")
                .join(format!("{:032x}.json", 0))
                .exists()
        );
        assert!(
            temp.path()
                .join("service-tokens")
                .join(format!("{:032x}.json", 1))
                .exists()
        );
    }
    #[test]
    fn minting_rejects_unknown_hosts_empty_grants_and_past_expiry() {
        let temp = tempfile::tempdir().unwrap();
        let tokens = ServiceTokens::open(temp.path()).unwrap();
        assert!(
            tokens
                .mint(request(None), |_| false, |_| false, |_| None)
                .is_err()
        );
        let mut empty = request(None);
        empty.operations.clear();
        assert!(tokens.mint(empty, |_| true, |_| true, |_| None).is_err());
        assert!(
            tokens
                .mint(
                    request(Some("2000-01-01T00:00:00Z".into())),
                    |_| true,
                    |_| true,
                    |_| None
                )
                .is_err()
        );
        let mut bad = request(None);
        bad.scopes[0].name = "../other".into();
        assert!(tokens.mint(bad, |_| true, |_| true, |_| None).is_err());
        assert!(
            serde_json::from_value::<MintRequest>(
                json!({"label":"x","scopes":[],"operations":["hosts.admin"]})
            )
            .is_err()
        );
    }
}
