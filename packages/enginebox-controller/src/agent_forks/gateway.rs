//! Each person's own Xava Inference (xinf) connection, the only model access
//! an agent-fork run gets (docs/ENGINE-SESSIONS.md, "Model access";
//! contract §4, owner decision 2026-11-04 #1: never the owner's keys).
//!
//! Engine Box is a confidential OAuth client of xinf (`https://xavainf.com`,
//! OAuth 2.1, PKCE S256, `scope=inference account:read`). A person connects
//! once: `POST /v1/projects/{p}/gateway/connect {return_to}` answers xinf's
//! authorize URL; xinf sends the browser to Engine Box's public callback
//! (`/v1/gateway/xinf/callback`, the dashboard forwards it here), which
//! exchanges the code, stores the credential and returns to `return_to`.
//! People who prefer an API key `PUT …/gateway {key:"xk_live_…"}` instead.
//!
//! Records live in `agent-credentials/gateway/<digest>.json`, one per
//! (org, person); the person is the member (`usr_…`) or the app-vouched user
//! (`gj:<user>@<client>`, the same across that app's rooms). Tokens and keys
//! are **sealed**: AES-256-GCM under a controller-private key file
//! (`agent-credentials/gateway-seal.key`, created on first use), bound to the
//! record's digest. No read, log, audit entry or error ever contains them.
//! A run receives only a short-lived access token (refreshed when less than
//! 15 minutes remain), delivered to the node in its poll for that run; the
//! node's relay keeps it in memory. The refresh token never leaves here.
use anyhow::{Context, Result, ensure};
use base64::Engine as _;
use enginebox_core::state;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

/// xinf's production origin (tests point the controller at a stand-in).
pub const XINF_ORIGIN: &str = "https://xavainf.com";
/// The platform org's `secret_source` connection holding Engine Box's OAuth
/// client registration at xinf.
pub const CONNECTION: &str = "xava-inference";
pub const CLIENT_ID: &str = "XINF_CLIENT_ID";
pub const CLIENT_SECRET: &str = "XINF_CLIENT_SECRET";
/// `https://<engine box public origin>/v1/gateway/xinf/callback`, exactly as
/// registered at xinf.
pub const REDIRECT_URI: &str = "XINF_REDIRECT_URI";
pub const SCOPE: &str = "inference account:read";
/// Refresh the access token when less than this remains.
pub const REFRESH_BEFORE_SECONDS: u64 = 15 * 60;
const PENDING_SECONDS: u64 = 10 * 60;
const MAX_PENDING: usize = 500;
const MAX_RECORDS: usize = 20_000;
const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REPLY: u64 = 64 * 1024;
pub const NOT_CONFIGURED: &str = "Engine Box is not registered with Xava Inference yet. The owner adds the xava-inference connection (XINF_CLIENT_ID, XINF_CLIENT_SECRET, XINF_REDIRECT_URI).";
pub const RECONNECT: &str =
    "Your Xava Inference connection expired or was revoked. Connect Xava Inference again.";

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
fn random(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut buf)
        .expect("system randomness");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}
fn sha256(s: &str) -> String {
    format!("{:x}", Sha256::digest(s.as_bytes()))
}

/// OAuth tokens from xinf (`xat_…` access, `xrt_…` refresh).
#[derive(Clone)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: u64,
}
impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Why xinf refused or could not be reached. `Revoked`: the credential is
/// dead (invalid_grant, invalid_token); the person must connect again.
#[derive(Debug)]
pub enum XinfError {
    Revoked,
    Unavailable(String),
}
impl std::fmt::Display for XinfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Revoked => f.write_str(RECONNECT),
            Self::Unavailable(why) => write!(f, "Xava Inference is unavailable ({why})."),
        }
    }
}
impl std::error::Error for XinfError {}

/// Engine Box as an OAuth client of xinf.
pub trait Xinf: Send + Sync {
    /// The API origin runs call (`https://xavainf.com`).
    fn origin(&self) -> String;
    fn authorize_url(&self, state: &str, challenge: &str) -> String;
    fn exchange(&self, code: &str, verifier: &str) -> std::result::Result<Tokens, XinfError>;
    fn refresh(&self, refresh_token: &str) -> std::result::Result<Tokens, XinfError>;
    /// `GET /oauth/account` (balance, this connection's cap and spend).
    fn account(&self, access_token: &str) -> std::result::Result<Value, XinfError>;
    /// Best effort (RFC 7009).
    fn revoke(&self, token: &str);
}

pub struct XinfClient {
    origin: String,
    client_id: String,
    client_secret: String,
    redirect_uri: String,
}
impl std::fmt::Debug for XinfClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XinfClient")
            .field("client_id", &self.client_id)
            .finish()
    }
}
impl XinfClient {
    pub fn new(
        origin: &str,
        client_id: &str,
        client_secret: &str,
        redirect_uri: &str,
    ) -> Result<Self> {
        let printable = |s: &str, max: usize| {
            !s.is_empty() && s.len() <= max && s.bytes().all(|b| (33..=126).contains(&b))
        };
        ensure!(
            printable(client_id, 200),
            "XINF_CLIENT_ID is not a client id."
        );
        ensure!(
            printable(client_secret, 512) && client_secret.len() >= 16,
            "XINF_CLIENT_SECRET is not a client secret."
        );
        let redirect = url::Url::parse(redirect_uri).context("XINF_REDIRECT_URI is not a URL.")?;
        ensure!(
            redirect.scheme() == "https"
                && redirect.path() == "/v1/gateway/xinf/callback"
                && redirect.query().is_none()
                && redirect.username().is_empty(),
            "XINF_REDIRECT_URI must be https://<engine box origin>/v1/gateway/xinf/callback."
        );
        Ok(Self {
            origin: origin.trim_end_matches('/').to_owned(),
            client_id: client_id.to_owned(),
            client_secret: client_secret.to_owned(),
            redirect_uri: redirect_uri.to_owned(),
        })
    }
    fn http() -> std::result::Result<reqwest::blocking::Client, XinfError> {
        reqwest::blocking::Client::builder()
            .timeout(TIMEOUT)
            .connect_timeout(TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| XinfError::Unavailable("no HTTP client".into()))
    }
    fn read(response: reqwest::blocking::Response) -> (u16, Value) {
        use std::io::Read;
        let status = response.status().as_u16();
        let mut bytes = Vec::new();
        let _ = response.take(MAX_REPLY).read_to_end(&mut bytes);
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
    fn token(&self, form: &[(&str, &str)]) -> std::result::Result<Tokens, XinfError> {
        let response = Self::http()?
            .post(format!("{}/oauth/token", self.origin))
            .basic_auth(&self.client_id, Some(&self.client_secret))
            .form(form)
            .send()
            .map_err(|_| XinfError::Unavailable("unreachable".into()))?;
        let (status, value) = Self::read(response);
        if status == 400 || status == 401 {
            let error = value["error"].as_str().unwrap_or("");
            if ["invalid_grant", "invalid_token", "unauthorized_client"].contains(&error) {
                return Err(XinfError::Revoked);
            }
            return Err(XinfError::Unavailable(format!(
                "token endpoint said {error}"
            )));
        }
        if status != 200 {
            return Err(XinfError::Unavailable(format!("HTTP {status}")));
        }
        tokens(&value).ok_or_else(|| XinfError::Unavailable("invalid token response".into()))
    }
}
/// A token response with an `xat_` access token (and `xrt_` refresh token).
pub fn tokens(value: &Value) -> Option<Tokens> {
    let access = value["access_token"]
        .as_str()
        .filter(|t| t.starts_with("xat_") && t.len() <= 512)?;
    let refresh = match value.get("refresh_token") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .filter(|t| t.starts_with("xrt_") && t.len() <= 512)?
                .to_owned(),
        ),
    };
    let ttl = value["expires_in"]
        .as_u64()
        .unwrap_or(3600)
        .clamp(60, 86_400);
    Some(Tokens {
        access_token: access.to_owned(),
        refresh_token: refresh,
        expires_at: now() + ttl,
    })
}
impl Xinf for XinfClient {
    fn origin(&self) -> String {
        self.origin.clone()
    }
    fn authorize_url(&self, state: &str, challenge: &str) -> String {
        let mut url = url::Url::parse(&format!("{}/oauth/authorize", self.origin)).unwrap();
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", &self.redirect_uri)
            .append_pair("scope", SCOPE)
            // The origin as the audience: xinf accepts it for /v1/* and for
            // /oauth/account (a `/v1` audience is refused by the account
            // endpoint).
            .append_pair("resource", &self.origin)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", state);
        url.to_string()
    }
    fn exchange(&self, code: &str, verifier: &str) -> std::result::Result<Tokens, XinfError> {
        self.token(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", &self.redirect_uri),
            ("resource", &self.origin),
        ])
    }
    fn refresh(&self, refresh_token: &str) -> std::result::Result<Tokens, XinfError> {
        self.token(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("resource", &self.origin),
        ])
    }
    fn account(&self, access_token: &str) -> std::result::Result<Value, XinfError> {
        let response = Self::http()?
            .get(format!("{}/oauth/account", self.origin))
            .bearer_auth(access_token)
            .send()
            .map_err(|_| XinfError::Unavailable("unreachable".into()))?;
        match Self::read(response) {
            (200, value) if value.is_object() => Ok(value),
            (401, _) => Err(XinfError::Revoked),
            (status, _) => Err(XinfError::Unavailable(format!("HTTP {status}"))),
        }
    }
    fn revoke(&self, token: &str) {
        if let Ok(http) = Self::http() {
            let _ = http
                .post(format!("{}/oauth/revoke", self.origin))
                .basic_auth(&self.client_id, Some(&self.client_secret))
                .form(&[("token", token)])
                .send();
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Oauth,
    Key,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Connected,
    /// xinf refused the refresh token: connect again.
    Reconnect,
}

/// One person's connection. `sealed` is the only place secrets live.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub digest: String,
    pub org_id: String,
    pub provider: String,
    pub kind: Kind,
    pub status: Status,
    pub connected_at: u64,
    pub updated_at: u64,
    /// When the sealed access token expires (0 for keys).
    pub access_expires_at: u64,
    sealed: String,
}

#[derive(Serialize, Deserialize, Default)]
struct Secret {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key: Option<String>,
}

/// A connect waiting for xinf's callback (memory only, ten minutes).
struct Pending {
    org: String,
    person: String,
    verifier: String,
    return_to: String,
    expires_at: u64,
}

/// What a run gets: a bearer for xinf and when it stops working.
#[derive(Clone)]
pub struct Access {
    pub token: String,
    pub expires_at: u64,
    pub kind: Kind,
}

/// The (org, person) a credential belongs to, as a file-safe digest.
pub fn digest(org: &str, person: &str) -> String {
    sha256(&format!("enginebox-gateway-v1\n{org}\n{person}"))
}

/// An API key the person pasted (`xk_live_…`).
pub fn valid_key(key: &str) -> bool {
    key.strip_prefix("xk_live_").is_some_and(|rest| {
        (16..=200).contains(&rest.len())
            && rest
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    })
}

/// `return_to` (an https URL on an allowed origin) with `xinf=<outcome>`.
fn returning(return_to: &str, outcome: &str) -> String {
    match url::Url::parse(return_to) {
        Ok(mut url) => {
            url.query_pairs_mut().append_pair("xinf", outcome);
            url.to_string()
        }
        Err(_) => return_to.to_owned(),
    }
}

/// A `return_to` the caller may name: an https URL without credentials or
/// fragment whose origin is in `origins`.
pub fn check_return_to(return_to: &str, origins: &[String]) -> Option<String> {
    let url = url::Url::parse(return_to).ok()?;
    let origin = url.origin().ascii_serialization();
    (return_to.len() <= 1024
        && url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        && origins.contains(&origin))
    .then(|| url.to_string())
}

#[derive(Clone)]
pub struct Gateway {
    dir: PathBuf,
    key_path: PathBuf,
    records: Arc<Mutex<BTreeMap<String, Record>>>,
    pending: Arc<Mutex<BTreeMap<String, Pending>>>,
    /// Serialises refreshes: xinf rotates refresh tokens, and a second use of
    /// a rotated one revokes the whole connection.
    refreshing: Arc<Mutex<()>>,
}
impl Gateway {
    pub fn open(root: &Path) -> Result<Self> {
        let base = root.join("agent-credentials");
        state::private_dir(&base)?;
        let dir = base.join("gateway");
        state::private_dir(&dir)?;
        let mut records = BTreeMap::new();
        for path in state::json_files(&dir)? {
            let r: Record = serde_json::from_value(state::read_json(&path)?)?;
            ensure!(
                r.digest.len() == 64
                    && path.file_stem().and_then(|s| s.to_str()) == Some(r.digest.as_str()),
                "Invalid gateway credential record"
            );
            records.insert(r.digest.clone(), r);
        }
        Ok(Self {
            dir,
            key_path: base.join("gateway-seal.key"),
            records: Arc::new(Mutex::new(records)),
            pending: Arc::new(Mutex::new(BTreeMap::new())),
            refreshing: Arc::new(Mutex::new(())),
        })
    }

    fn seal_key(&self) -> Result<ring::aead::LessSafeKey> {
        use std::io::{Read, Write};
        let mut options = std::fs::OpenOptions::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let bytes = match options.clone().read(true).open(&self.key_path) {
            Ok(mut f) => {
                let mut b = Vec::new();
                f.read_to_end(&mut b)?;
                b
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut b = vec![0u8; 32];
                ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b)
                    .map_err(|_| anyhow::anyhow!("No system randomness"))?;
                match options.write(true).create_new(true).open(&self.key_path) {
                    Ok(mut f) => {
                        f.write_all(&b)?;
                        f.sync_all()?;
                        b
                    }
                    // Another thread made it first: use theirs.
                    Err(_) => std::fs::read(&self.key_path)?,
                }
            }
            Err(e) => return Err(e.into()),
        };
        ensure!(bytes.len() == 32, "The gateway seal key is damaged");
        Ok(ring::aead::LessSafeKey::new(
            ring::aead::UnboundKey::new(&ring::aead::AES_256_GCM, &bytes)
                .map_err(|_| anyhow::anyhow!("Invalid seal key"))?,
        ))
    }
    fn seal(&self, digest: &str, secret: &Secret) -> Result<String> {
        let key = self.seal_key()?;
        let mut nonce = [0u8; 12];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut nonce)
            .map_err(|_| anyhow::anyhow!("No system randomness"))?;
        let mut data = serde_json::to_vec(secret)?;
        key.seal_in_place_append_tag(
            ring::aead::Nonce::assume_unique_for_key(nonce),
            ring::aead::Aad::from(format!("gateway:{digest}").as_bytes()),
            &mut data,
        )
        .map_err(|_| anyhow::anyhow!("Sealing failed"))?;
        let mut out = nonce.to_vec();
        out.extend_from_slice(&data);
        Ok(format!(
            "v1.{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(out)
        ))
    }
    fn open_sealed(&self, r: &Record) -> Result<Secret> {
        let raw = r
            .sealed
            .strip_prefix("v1.")
            .and_then(|b| {
                base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(b)
                    .ok()
            })
            .filter(|b| b.len() > 12 + 16)
            .context("The gateway credential is damaged")?;
        let key = self.seal_key()?;
        let (nonce, data) = raw.split_at(12);
        let mut data = data.to_vec();
        let plain = key
            .open_in_place(
                ring::aead::Nonce::try_assume_unique_for_key(nonce)
                    .map_err(|_| anyhow::anyhow!("damaged"))?,
                ring::aead::Aad::from(format!("gateway:{}", r.digest).as_bytes()),
                &mut data,
            )
            .map_err(|_| {
                anyhow::anyhow!("The gateway credential cannot be opened; connect again")
            })?;
        Ok(serde_json::from_slice(plain)?)
    }
    fn save(&self, r: &Record) -> Result<()> {
        state::write_json(
            &self.dir.join(format!("{}.json", r.digest)),
            &serde_json::to_value(r)?,
        )
    }
    fn store(
        &self,
        org: &str,
        person: &str,
        kind: Kind,
        secret: &Secret,
        expires: u64,
    ) -> Result<()> {
        let d = digest(org, person);
        let sealed = self.seal(&d, secret)?;
        let t = now();
        let mut records = self.records.lock().unwrap();
        ensure!(
            records.contains_key(&d) || records.len() < MAX_RECORDS,
            "Too many gateway connections"
        );
        let connected_at = records.get(&d).map_or(t, |r| r.connected_at);
        let record = Record {
            digest: d.clone(),
            org_id: org.to_owned(),
            provider: "xinf".into(),
            kind,
            status: Status::Connected,
            connected_at: if records.get(&d).is_some_and(|r| r.kind == kind) {
                connected_at
            } else {
                t
            },
            updated_at: t,
            access_expires_at: expires,
            sealed,
        };
        self.save(&record)?;
        records.insert(d, record);
        Ok(())
    }

    /// The person's record, if connected (any status).
    pub fn record(&self, org: &str, person: &str) -> Option<Record> {
        self.records
            .lock()
            .unwrap()
            .get(&digest(org, person))
            .cloned()
    }

    /// Start connecting: xinf's authorize URL for this person.
    pub fn connect(
        &self,
        org: &str,
        person: &str,
        return_to: &str,
        xinf: &dyn Xinf,
    ) -> Result<String> {
        let state = random(32);
        let verifier = random(48);
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes()));
        let t = now();
        let mut pending = self.pending.lock().unwrap();
        pending.retain(|_, p| p.expires_at > t);
        ensure!(
            pending.len() < MAX_PENDING,
            "Too many Xava Inference connections are in progress; try again in a few minutes."
        );
        pending.insert(
            sha256(&state),
            Pending {
                org: org.to_owned(),
                person: person.to_owned(),
                verifier,
                return_to: return_to.to_owned(),
                expires_at: t + PENDING_SECONDS,
            },
        );
        Ok(xinf.authorize_url(&state, &challenge))
    }

    /// xinf's callback: `code` or `error` for `state`. Returns where to send
    /// the browser and, when stored, whose connection it was (org, person).
    pub fn callback(
        &self,
        state: &str,
        code: Option<&str>,
        xinf: &dyn Xinf,
    ) -> Result<(String, Option<(String, String)>)> {
        let p = {
            let mut pending = self.pending.lock().unwrap();
            let t = now();
            pending.retain(|_, p| p.expires_at > t);
            pending
                .remove(&sha256(state))
                .context("This Xava Inference connection request expired or was already used. Start it again.")?
        };
        let Some(code) = code else {
            return Ok((returning(&p.return_to, "denied"), None));
        };
        match xinf.exchange(code, &p.verifier) {
            Ok(t) if t.refresh_token.is_some() => {
                self.store(
                    &p.org,
                    &p.person,
                    Kind::Oauth,
                    &Secret {
                        access_token: Some(t.access_token),
                        refresh_token: t.refresh_token,
                        key: None,
                    },
                    t.expires_at,
                )?;
                Ok((
                    returning(&p.return_to, "connected"),
                    Some((p.org, p.person)),
                ))
            }
            Ok(_) | Err(_) => Ok((returning(&p.return_to, "failed"), None)),
        }
    }

    /// `PUT …/gateway {key}`: a write-only API key instead of OAuth.
    pub fn put_key(&self, org: &str, person: &str, key: &str) -> Result<()> {
        ensure!(
            valid_key(key),
            "key must be a Xava Inference API key (xk_live_…)."
        );
        self.store(
            org,
            person,
            Kind::Key,
            &Secret {
                key: Some(key.to_owned()),
                ..Default::default()
            },
            0,
        )
    }

    /// `DELETE …/gateway`: forget the connection (revoked at xinf when OAuth).
    pub fn delete(&self, org: &str, person: &str, xinf: Option<&dyn Xinf>) -> Result<bool> {
        let d = digest(org, person);
        let Some(record) = self.records.lock().unwrap().remove(&d) else {
            return Ok(false);
        };
        if let (Some(xinf), Kind::Oauth) = (xinf, record.kind)
            && let Ok(secret) = self.open_sealed(&record)
            && let Some(refresh) = secret.refresh_token
        {
            xinf.revoke(&refresh);
        }
        let _ = std::fs::remove_file(self.dir.join(format!("{d}.json")));
        Ok(true)
    }

    /// A bearer for a run, refreshed when less than 15 minutes remain.
    pub fn access(&self, org: &str, person: &str, xinf: Option<&dyn Xinf>) -> Result<Access> {
        let d = digest(org, person);
        let current = |records: &BTreeMap<String, Record>| -> Result<Option<Access>> {
            let r = records
                .get(&d)
                .context("Connect Xava Inference and add balance to run agents.")?;
            ensure!(r.status == Status::Connected, RECONNECT);
            let secret = self.open_sealed(r)?;
            Ok(match r.kind {
                Kind::Key => Some(Access {
                    token: secret.key.context("damaged key")?,
                    expires_at: 0,
                    kind: Kind::Key,
                }),
                Kind::Oauth => (r.access_expires_at > now() + REFRESH_BEFORE_SECONDS)
                    .then(|| {
                        secret.access_token.map(|token| Access {
                            token,
                            expires_at: r.access_expires_at,
                            kind: Kind::Oauth,
                        })
                    })
                    .flatten(),
            })
        };
        if let Some(access) = current(&self.records.lock().unwrap())? {
            return Ok(access);
        }
        let _refreshing = self.refreshing.lock().unwrap();
        // Someone else may have refreshed while we waited.
        if let Some(access) = current(&self.records.lock().unwrap())? {
            return Ok(access);
        }
        let record = self
            .records
            .lock()
            .unwrap()
            .get(&d)
            .cloned()
            .context("gone")?;
        let refresh = self
            .open_sealed(&record)?
            .refresh_token
            .context("damaged credential")?;
        let xinf = xinf.context(NOT_CONFIGURED)?;
        match xinf.refresh(&refresh) {
            Ok(t) => {
                let secret = Secret {
                    access_token: Some(t.access_token.clone()),
                    refresh_token: Some(t.refresh_token.unwrap_or(refresh)),
                    key: None,
                };
                let sealed = self.seal(&d, &secret)?;
                let mut records = self.records.lock().unwrap();
                let r = records.get_mut(&d).context("gone")?;
                r.sealed = sealed;
                r.access_expires_at = t.expires_at;
                r.updated_at = now();
                self.save(r)?;
                Ok(Access {
                    token: t.access_token,
                    expires_at: t.expires_at,
                    kind: Kind::Oauth,
                })
            }
            Err(XinfError::Revoked) => {
                let mut records = self.records.lock().unwrap();
                if let Some(r) = records.get_mut(&d) {
                    r.status = Status::Reconnect;
                    r.updated_at = now();
                    let _ = self.save(r);
                }
                anyhow::bail!(RECONNECT)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// `GET …/gateway`: connection state, with balance, cap and spend from
    /// xinf when the connection can read them.
    pub fn status(&self, org: &str, person: &str, xinf: Option<&dyn Xinf>) -> Value {
        let topup = format!(
            "{}/dashboard/billing",
            xinf.map_or(XINF_ORIGIN.to_owned(), |x| x.origin())
        );
        let Some(r) = self.record(org, person) else {
            return json!({"connected": false, "topup_url": topup});
        };
        let mut view = json!({
            "connected": r.status == Status::Connected,
            "kind": r.kind,
            "status": r.status,
            "connected_at": r.connected_at,
            "topup_url": topup,
            "monthly_cap_usd": null,
        });
        if r.kind == Kind::Oauth
            && r.status == Status::Connected
            && let Some(xinf) = xinf
            && let Some(account) = self.account(org, person, xinf)
        {
            view["balance_usd"] = account["balance"]["available_usd"].clone();
            view["monthly_cap_usd"] = account["connection"]["monthly_cap_usd"].clone();
            view["spent_this_month_usd"] = account["connection"]["spent_this_month_usd"].clone();
            view["remaining_budget_usd"] = account["connection"]["remaining_budget_usd"].clone();
            if let Some(url) = account["topup_url"]
                .as_str()
                .filter(|u| u.starts_with(&xinf.origin()))
            {
                view["topup_url"] = json!(url);
            }
        }
        if r.status == Status::Reconnect {
            view["error"] = json!(RECONNECT);
        }
        view
    }

    /// xinf's account view for an OAuth connection (None when unreadable).
    pub fn account(&self, org: &str, person: &str, xinf: &dyn Xinf) -> Option<Value> {
        let access = self.access(org, person, Some(xinf)).ok()?;
        (access.kind == Kind::Oauth)
            .then(|| xinf.account(&access.token).ok())
            .flatten()
    }
}

/// The USD amount in xinf's decimal-string money format.
pub fn usd(value: &Value) -> Option<f64> {
    match value {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

/// A stand-in for xinf's OAuth endpoints (tests only).
#[cfg(test)]
pub mod fake {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    pub const CLIENT_ID: &str = "xc_enginebox_test";
    pub const CLIENT_SECRET: &str = "xcs_fake_engine_box_secret_0001";
    pub const CODE: &str = "xac_fake_authorization_code";
    pub const REDIRECT: &str = "https://enginebox.example/v1/gateway/xinf/callback";

    #[derive(Default)]
    pub struct Seen {
        /// Every token request body (form-encoded), in order.
        pub token_requests: Vec<String>,
        /// Refresh tokens presented.
        pub refreshes: Vec<String>,
        /// Number of access tokens issued so far.
        pub issued: u64,
        pub balance_usd: String,
        /// Access tokens expire after this many seconds.
        pub ttl: u64,
    }
    pub struct Fake {
        pub url: String,
        pub seen: Arc<Mutex<Seen>>,
    }

    /// Serves `/oauth/token` (authorization_code with [`CODE`] and any
    /// verifier whose S256 matches the challenge is not checked here;
    /// refresh_token rotates), `/oauth/account` and `/oauth/revoke`.
    pub fn serve() -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Seen {
            balance_usd: "12.500000000".into(),
            ttl: 3600,
            ..Default::default()
        }));
        let shared = seen.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let seen = shared.clone();
                thread::spawn(move || {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let (head, mut body) = loop {
                        match stream.read(&mut chunk) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                        let text = String::from_utf8_lossy(&buf).to_string();
                        if let Some((h, b)) = text.split_once("\r\n\r\n") {
                            break (h.to_owned(), b.to_owned());
                        }
                    };
                    let length: usize = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    while body.len() < length {
                        match stream.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => body.push_str(&String::from_utf8_lossy(&chunk[..n])),
                        }
                    }
                    let header = |name: &str| {
                        head.lines().find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
                        })
                    };
                    let basic = format!(
                        "Basic {}",
                        base64::engine::general_purpose::STANDARD
                            .encode(format!("{CLIENT_ID}:{CLIENT_SECRET}"))
                    );
                    let form: BTreeMap<String, String> =
                        url::form_urlencoded::parse(body.as_bytes())
                            .into_owned()
                            .collect();
                    let mut s = seen.lock().unwrap();
                    let (status, reply) = if head.starts_with("POST /oauth/token ") {
                        s.token_requests.push(body.clone());
                        let issue = |s: &mut Seen| {
                            s.issued += 1;
                            json!({"access_token": format!("xat_g1_access{:04}abcdefghijklmnop", s.issued),
                                   "refresh_token": format!("xrt_g1_refresh{:04}abcdefghijklmnop", s.issued),
                                   "token_type": "Bearer", "expires_in": s.ttl, "scope": SCOPE})
                        };
                        if header("authorization").as_deref() != Some(basic.as_str()) {
                            (401, json!({"error": "invalid_client"}))
                        } else if form.get("grant_type").map(String::as_str)
                            == Some("authorization_code")
                            && form.get("code").map(String::as_str) == Some(CODE)
                            && form.get("redirect_uri").map(String::as_str) == Some(REDIRECT)
                            && form.get("code_verifier").is_some_and(|v| v.len() >= 43)
                        {
                            (200, issue(&mut s))
                        } else if form.get("grant_type").map(String::as_str)
                            == Some("refresh_token")
                        {
                            let presented = form.get("refresh_token").cloned().unwrap_or_default();
                            let expected = format!("xrt_g1_refresh{:04}abcdefghijklmnop", s.issued);
                            s.refreshes.push(presented.clone());
                            if presented == expected {
                                (200, issue(&mut s))
                            } else {
                                (400, json!({"error": "invalid_grant"}))
                            }
                        } else {
                            (400, json!({"error": "invalid_grant"}))
                        }
                    } else if head.starts_with("GET /oauth/account ") {
                        let current =
                            format!("Bearer xat_g1_access{:04}abcdefghijklmnop", s.issued);
                        if header("authorization").as_deref() == Some(current.as_str()) {
                            (
                                200,
                                json!({"currency": "USD", "balance": {"available_usd": s.balance_usd},
                                "connection": {"monthly_cap_usd": "50.000000000", "spent_this_month_usd": "1.250000000", "remaining_budget_usd": "48.750000000"},
                                "topup_url": "http://ignored.example/dashboard/billing"}),
                            )
                        } else {
                            (401, json!({"error": "invalid_token"}))
                        }
                    } else if head.starts_with("POST /oauth/revoke ") {
                        (200, json!({}))
                    } else {
                        (404, json!({"error": "not_found"}))
                    };
                    drop(s);
                    let text = reply.to_string();
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                        text.len()
                    );
                });
            }
        });
        Fake { url, seen }
    }
    pub fn client(url: &str) -> XinfClient {
        XinfClient::new(url, CLIENT_ID, CLIENT_SECRET, REDIRECT).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connects_seals_refreshes_and_never_stores_plaintext() {
        let temp = tempfile::tempdir().unwrap();
        let g = Gateway::open(temp.path()).unwrap();
        let xinf = fake::serve();
        let client = fake::client(&xinf.url);
        // Connect: the authorize URL carries PKCE, scope, resource and state.
        let url = g
            .connect(
                "default",
                "gj:u1@ebc_app",
                "https://gamejam.example/room?x=1",
                &client,
            )
            .unwrap();
        let parsed = url::Url::parse(&url).unwrap();
        let q: BTreeMap<String, String> = parsed.query_pairs().into_owned().collect();
        assert_eq!(q["scope"], SCOPE);
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["resource"], xinf.url);
        assert_eq!(q["redirect_uri"], fake::REDIRECT);
        let state = q["state"].clone();
        // A wrong state is refused; the right one stores the credential.
        assert!(g.callback("nope", Some(fake::CODE), &client).is_err());
        let (back, who) = g.callback(&state, Some(fake::CODE), &client).unwrap();
        assert_eq!(back, "https://gamejam.example/room?x=1&xinf=connected");
        assert_eq!(who, Some(("default".into(), "gj:u1@ebc_app".into())));
        // One use only.
        assert!(g.callback(&state, Some(fake::CODE), &client).is_err());
        let access = g.access("default", "gj:u1@ebc_app", Some(&client)).unwrap();
        assert!(access.token.starts_with("xat_g1_access0001"));
        // Nothing on disk contains a token.
        for entry in walk(temp.path()) {
            let text = String::from_utf8_lossy(&std::fs::read(&entry).unwrap()).to_string();
            assert!(
                !text.contains("xat_") && !text.contains("xrt_"),
                "{entry:?}"
            );
        }
        // Status reads the balance through the connection.
        let status = g.status("default", "gj:u1@ebc_app", Some(&client));
        assert_eq!(status["connected"], true);
        assert_eq!(status["balance_usd"], "12.500000000");
        assert_eq!(status["monthly_cap_usd"], "50.000000000");
        assert_eq!(
            status["topup_url"],
            format!("{}/dashboard/billing", xinf.url)
        );
        assert!(!status.to_string().contains("xat_"));
        // Close to expiry: refreshed (rotating) exactly once.
        {
            let mut records = g.records.lock().unwrap();
            let r = records.values_mut().next().unwrap();
            r.access_expires_at = now() + 60;
        }
        let refreshed = g.access("default", "gj:u1@ebc_app", Some(&client)).unwrap();
        assert!(refreshed.token.starts_with("xat_g1_access0002"));
        assert_eq!(xinf.seen.lock().unwrap().refreshes.len(), 1);
        // A revoked refresh token marks the connection for reconnecting.
        {
            let mut records = g.records.lock().unwrap();
            records.values_mut().next().unwrap().access_expires_at = 0;
        }
        xinf.seen.lock().unwrap().issued = 99;
        assert!(g.access("default", "gj:u1@ebc_app", Some(&client)).is_err());
        assert_eq!(
            g.record("default", "gj:u1@ebc_app").unwrap().status,
            Status::Reconnect
        );
        // Reopening reads the same records.
        let again = Gateway::open(temp.path()).unwrap();
        assert!(again.record("default", "gj:u1@ebc_app").is_some());
        // Other people and orgs have nothing.
        assert!(g.access("default", "gj:u2@ebc_app", Some(&client)).is_err());
        assert!(g.access("org_x", "gj:u1@ebc_app", Some(&client)).is_err());
        assert!(g.delete("default", "gj:u1@ebc_app", Some(&client)).unwrap());
        assert!(g.record("default", "gj:u1@ebc_app").is_none());
    }

    #[test]
    fn keys_are_write_only_and_denials_return() {
        let temp = tempfile::tempdir().unwrap();
        let g = Gateway::open(temp.path()).unwrap();
        let xinf = fake::serve();
        let client = fake::client(&xinf.url);
        assert!(g.put_key("default", "usr_a", "sk-not-xinf").is_err());
        let key = "xk_live_abcdefghijklmnopqrstuvwx";
        g.put_key("default", "usr_a", key).unwrap();
        let status = g.status("default", "usr_a", Some(&client));
        assert_eq!(status["kind"], "key");
        assert!(!status.to_string().contains(key));
        assert_eq!(g.access("default", "usr_a", None).unwrap().token, key);
        for entry in walk(temp.path()) {
            assert!(!String::from_utf8_lossy(&std::fs::read(&entry).unwrap()).contains(key));
        }
        let url = g
            .connect("default", "usr_b", "https://gamejam.example/", &client)
            .unwrap();
        let state = url::Url::parse(&url)
            .unwrap()
            .query_pairs()
            .find(|(k, _)| k == "state")
            .unwrap()
            .1
            .to_string();
        let (back, who) = g.callback(&state, None, &client).unwrap();
        assert_eq!(back, "https://gamejam.example/?xinf=denied");
        assert!(who.is_none());
        assert_eq!(
            check_return_to(
                "https://gamejam.example/r?a=1",
                &["https://gamejam.example".into()]
            ),
            Some("https://gamejam.example/r?a=1".into())
        );
        for bad in [
            "http://gamejam.example/",
            "https://evil.example/",
            "https://u:p@gamejam.example/",
            "https://gamejam.example/#f",
        ] {
            assert!(
                check_return_to(bad, &["https://gamejam.example".into()]).is_none(),
                "{bad}"
            );
        }
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
        out
    }
}
