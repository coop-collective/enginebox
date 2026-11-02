//! Per-session MoQ relays on Cloudflare (docs/ENGINE-SESSIONS.md, "Per-session
//! relays").
//!
//! Cloudflare MoQ tokens are scoped to one relay and to operations
//! (`publish`, `subscribe`), never to a namespace, so a relay-wide subscribe
//! token would let anyone who holds it watch every session on that relay.
//! Each engine session therefore gets its own relay: the node publishes with a
//! token for that relay only, and viewers get short-lived subscribe-only tokens
//! for it. A token's `sub` claim is its relay's id, so the URL clients dial
//! (`https://draft-16.cloudflare.mediaoverquic.com/<token>`) is the same for
//! every relay.
//!
//! The API token and account come from the platform's `cloudflare-moq`
//! connection (`CLOUDFLARE_MOQ_API_TOKEN`, `CLOUDFLARE_ACCOUNT_ID`). Token
//! secrets are returned to callers and held in memory only; nothing here logs a
//! secret or a response body, and error messages carry only HTTP status and
//! Cloudflare error codes.
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::time::Duration;

/// Cloudflare's API; tests point the controller at a stand-in.
pub const API_BASE: &str = "https://api.cloudflare.com/client/v4";
/// `cloudflare-moq` fields that turn the broker on.
pub const API_TOKEN: &str = "CLOUDFLARE_MOQ_API_TOKEN";
pub const ACCOUNT_ID: &str = "CLOUDFLARE_ACCOUNT_ID";
pub const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REPLY: u64 = 256 * 1024;

/// One minted relay token. `secret` is the JWT clients present.
#[derive(Clone)]
pub struct Token {
    pub jti: String,
    pub secret: String,
    /// Unix seconds.
    pub expires_at: u64,
}
impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Token")
            .field("jti", &self.jti)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// What the engine sessions need from a relay provider.
pub trait Broker: Send + Sync {
    /// Create a relay and return its id. The tokens Cloudflare creates with
    /// it (valid for a year) are revoked at once; callers mint their own.
    fn create_relay(&self, name: &str) -> Result<String>;
    /// Mint a token for `relay`: subscribe-only, or publish and subscribe.
    fn mint(&self, relay: &str, publish: bool, expires_at: u64, label: &str) -> Result<Token>;
    fn revoke(&self, relay: &str, jti: &str) -> Result<()>;
    /// Delete a relay (and with it every token). An unknown relay is gone.
    fn delete_relay(&self, relay: &str) -> Result<()>;
}

/// A Cloudflare relay id or token id: 32 lowercase hex characters.
pub fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The Cloudflare API client.
pub struct Cloudflare {
    base: String,
    account: String,
    token: String,
    timeout: Duration,
}
impl std::fmt::Debug for Cloudflare {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cloudflare")
            .field("account", &self.account)
            .finish()
    }
}

impl Cloudflare {
    pub fn new(base: &str, account: &str, token: &str) -> Result<Self> {
        let account = account.trim();
        let token = token.trim();
        ensure!(
            valid_id(account),
            "CLOUDFLARE_ACCOUNT_ID must be a 32-character Cloudflare account ID."
        );
        ensure!(
            (20..=512).contains(&token.len()) && token.bytes().all(|b| (33..=126).contains(&b)),
            "CLOUDFLARE_MOQ_API_TOKEN must be a Cloudflare API token."
        );
        Ok(Self {
            base: base.trim_end_matches('/').to_owned(),
            account: account.to_owned(),
            token: token.to_owned(),
            timeout: TIMEOUT,
        })
    }
    fn relays(&self) -> String {
        format!("{}/accounts/{}/moq/relays", self.base, self.account)
    }
    /// One API call: the HTTP status and the parsed envelope (Null when the
    /// body is not JSON). Never follows redirects.
    fn call(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value)> {
        let client = reqwest::blocking::Client::builder()
            .timeout(self.timeout)
            .connect_timeout(self.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("Cloudflare client unavailable")?;
        let mut request = client
            .request(method, url)
            .bearer_auth(&self.token)
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .map_err(|_| anyhow::anyhow!("Cloudflare is unreachable"))?;
        let status = response.status().as_u16();
        let mut bytes = Vec::new();
        {
            use std::io::Read;
            response
                .take(MAX_REPLY)
                .read_to_end(&mut bytes)
                .map_err(|_| anyhow::anyhow!("Cloudflare's reply could not be read"))?;
        }
        Ok((
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        ))
    }
    /// The envelope's `result` on success, else a value-free error.
    fn result(what: &str, (status, envelope): (u16, Value)) -> Result<Value> {
        if (200..300).contains(&status) && envelope["success"] != json!(false) {
            return Ok(envelope["result"].clone());
        }
        let codes: Vec<String> = envelope["errors"]
            .as_array()
            .map(|e| {
                e.iter()
                    .filter_map(|e| e["code"].as_u64().map(|c| c.to_string()))
                    .take(4)
                    .collect()
            })
            .unwrap_or_default();
        if codes.is_empty() {
            bail!("Cloudflare MoQ {what} failed (HTTP {status})")
        }
        bail!(
            "Cloudflare MoQ {what} failed (HTTP {status}, code {})",
            codes.join(", ")
        )
    }
    fn tokens(result: &Value) -> Vec<Value> {
        result["issuers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|i| i["type"] == "cloudflare_jwt")
            .flat_map(|i| {
                i["cloudflare_tokens"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .collect()
    }
}

fn rfc3339(at: u64) -> String {
    chrono::DateTime::from_timestamp(at as i64, 0)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
pub fn unix(at: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(at)
        .ok()
        .and_then(|t| u64::try_from(t.timestamp()).ok())
}

impl Broker for Cloudflare {
    fn create_relay(&self, name: &str) -> Result<String> {
        let result = Self::result(
            "relay creation",
            self.call(
                reqwest::Method::POST,
                &self.relays(),
                Some(json!({"name": name})),
            )?,
        )?;
        let relay = result["uid"]
            .as_str()
            .filter(|u| valid_id(u))
            .context("Cloudflare returned no relay id")?
            .to_owned();
        // The default tokens last a year and would be the only ones Engine Box
        // did not mint; revoke them now. A failure here is harmless (deleting
        // the relay at the session's end revokes them too) but is reported.
        for token in Self::tokens(&result) {
            if let Some(jti) = token["jti"].as_str().filter(|j| valid_id(j))
                && let Err(error) = self.revoke(&relay, jti)
            {
                eprintln!(
                    "{}",
                    json!({"event":"enginebox.moq.default_token_kept","relay":relay,"jti":jti,"reason":error.to_string()})
                );
            }
        }
        Ok(relay)
    }
    fn mint(&self, relay: &str, publish: bool, expires_at: u64, label: &str) -> Result<Token> {
        ensure!(valid_id(relay), "Invalid relay id");
        let operations = if publish {
            json!(["publish", "subscribe"])
        } else {
            json!(["subscribe"])
        };
        let result = Self::result(
            "token minting",
            self.call(
                reqwest::Method::POST,
                &format!("{}/{relay}/tokens", self.relays()),
                Some(json!({"operations": operations, "expires": rfc3339(expires_at), "label": label})),
            )?,
        )?;
        let token = Self::tokens(&result)
            .into_iter()
            .next()
            .context("Cloudflare returned no token")?;
        let jti = token["jti"]
            .as_str()
            .filter(|j| valid_id(j))
            .context("Cloudflare returned no token id")?;
        let secret = token["secret"]
            .as_str()
            .filter(|s| !s.is_empty() && s.bytes().all(|b| (33..=126).contains(&b)))
            .context("Cloudflare returned no token secret")?;
        Ok(Token {
            jti: jti.to_owned(),
            secret: secret.to_owned(),
            expires_at: token["expires"]
                .as_str()
                .and_then(unix)
                .unwrap_or(expires_at)
                .min(expires_at),
        })
    }
    fn revoke(&self, relay: &str, jti: &str) -> Result<()> {
        ensure!(
            valid_id(relay) && valid_id(jti),
            "Invalid relay or token id"
        );
        let reply = self.call(
            reqwest::Method::DELETE,
            &format!("{}/{relay}/tokens/{jti}", self.relays()),
            None,
        )?;
        if reply.0 == 404 {
            return Ok(());
        }
        Self::result("token revocation", reply).map(|_| ())
    }
    fn delete_relay(&self, relay: &str) -> Result<()> {
        ensure!(valid_id(relay), "Invalid relay id");
        let reply = self.call(
            reqwest::Method::DELETE,
            &format!("{}/{relay}", self.relays()),
            None,
        )?;
        if reply.0 == 404 {
            return Ok(());
        }
        Self::result("relay deletion", reply).map(|_| ())
    }
}

/// A stand-in for Cloudflare's MoQ relay API (tests only): relays, at most ten
/// tokens per relay, the shown-once secrets, revocation and deletion, behind
/// a bearer API token. `fail_create` makes relay creation answer 500.
#[cfg(test)]
pub mod fake {
    use serde_json::{Value, json};
    use std::{
        collections::BTreeMap,
        io::{Read, Write},
        net::TcpListener,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        thread,
    };

    pub const ACCOUNT: &str = "e40385e6d96d5cad8df33b1ef3e3694a";
    pub const API_TOKEN: &str = "fake-cloudflare-moq-api-token-0001";

    #[derive(Clone, Debug)]
    pub struct FakeToken {
        pub jti: String,
        pub operations: Vec<String>,
        pub expires: String,
        pub label: String,
        pub secret: String,
    }
    #[derive(Default)]
    pub struct State {
        /// relay id -> its tokens
        pub relays: BTreeMap<String, Vec<FakeToken>>,
        pub deleted: Vec<String>,
        /// Every request line ("METHOD /path"), for assertions.
        pub requests: Vec<String>,
    }
    pub struct Fake {
        pub url: String,
        pub state: Arc<Mutex<State>>,
        pub fail_create: Arc<AtomicBool>,
    }
    impl Fake {
        /// Every token secret ever issued for `relay`, live or not.
        pub fn live(&self, relay: &str) -> Vec<FakeToken> {
            self.state
                .lock()
                .unwrap()
                .relays
                .get(relay)
                .cloned()
                .unwrap_or_default()
        }
    }
    fn hex() -> String {
        uuid::Uuid::new_v4().simple().to_string()
    }
    fn token(relay: &str, operations: Vec<String>, expires: String, label: String) -> FakeToken {
        let jti = hex();
        FakeToken {
            // A JWT-shaped secret whose claims name the relay (`sub`).
            secret: format!("eyJhbGciOiJFZERTQSJ9.sub-{relay}.jti-{jti}-sig"),
            jti,
            operations,
            expires,
            label,
        }
    }
    fn view(t: &FakeToken, secret: bool) -> Value {
        let mut v = json!({"jti": t.jti, "operations": t.operations, "expires": t.expires, "label": t.label, "created": "2026-11-02T00:00:00Z"});
        if secret {
            v["secret"] = json!(t.secret);
        }
        v
    }
    fn issuers(tokens: Vec<Value>) -> Value {
        json!([{"type":"cloudflare_jwt","issuer":"cloudflare","cloudflare_tokens": tokens}])
    }

    pub fn serve() -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/client/v4", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State::default()));
        let fail_create = Arc::new(AtomicBool::new(false));
        let (shared, failing) = (state.clone(), fail_create.clone());
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let (state, failing) = (shared.clone(), failing.clone());
                thread::spawn(move || {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let (head, body) = loop {
                        let Ok(n) = stream.read(&mut chunk) else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        let text = String::from_utf8_lossy(&buf).to_string();
                        if let Some(end) = text.find("\r\n\r\n") {
                            let length = text[..end]
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                                })
                                .unwrap_or(0);
                            if buf.len() >= end + 4 + length {
                                break (
                                    text[..end].to_owned(),
                                    text[end + 4..end + 4 + length].to_owned(),
                                );
                            }
                        }
                    };
                    let mut lines = head.lines();
                    let first = lines.next().unwrap_or("").to_owned();
                    let mut parts = first.split(' ');
                    let (method, path) = (
                        parts.next().unwrap_or("").to_owned(),
                        parts.next().unwrap_or("").to_owned(),
                    );
                    let authorized = lines.any(|l| {
                        l.to_ascii_lowercase().starts_with("authorization:")
                            && l.trim_end().ends_with(&format!("Bearer {API_TOKEN}"))
                    });
                    let body: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                    let (status, reply) =
                        handle(&state, &failing, authorized, &method, &path, &body);
                    let text = reply.to_string();
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                        text.len()
                    );
                });
            }
        });
        Fake {
            url,
            state,
            fail_create,
        }
    }
    fn ok(result: Value) -> Value {
        json!({"success": true, "errors": [], "messages": [], "result": result})
    }
    fn err(code: u64) -> Value {
        json!({"success": false, "errors": [{"code": code, "message": "refused"}], "messages": [], "result": null})
    }
    fn handle(
        state: &Mutex<State>,
        failing: &AtomicBool,
        authorized: bool,
        method: &str,
        path: &str,
        body: &Value,
    ) -> (&'static str, Value) {
        let mut s = state.lock().unwrap();
        s.requests.push(format!("{method} {path}"));
        if !authorized {
            return ("403 Forbidden", err(10000));
        }
        let prefix = format!("/client/v4/accounts/{ACCOUNT}/moq/relays");
        let Some(rest) = path.strip_prefix(&prefix) else {
            return ("404 Not Found", err(7003));
        };
        let parts: Vec<&str> = rest.split('/').filter(|p| !p.is_empty()).collect();
        match (method, parts.as_slice()) {
            ("POST", []) => {
                if failing.load(Ordering::SeqCst) {
                    return ("500 Internal Server Error", err(21000));
                }
                let relay = hex();
                let defaults = vec![
                    token(
                        &relay,
                        vec!["publish".into(), "subscribe".into()],
                        "2027-11-02T00:00:00Z".into(),
                        "Default publish/subscribe".into(),
                    ),
                    token(
                        &relay,
                        vec!["subscribe".into()],
                        "2027-11-02T00:00:00Z".into(),
                        "Default subscribe".into(),
                    ),
                ];
                let shown: Vec<Value> = defaults.iter().map(|t| view(t, true)).collect();
                s.relays.insert(relay.clone(), defaults);
                (
                    "201 Created",
                    ok(
                        json!({"uid": relay, "name": body["name"], "config": {"upstreams": {}}, "issuers": issuers(shown)}),
                    ),
                )
            }
            ("DELETE", [relay]) => {
                if s.relays.remove(*relay).is_none() {
                    return ("404 Not Found", err(21001));
                }
                s.deleted.push(relay.to_string());
                ("200 OK", ok(json!({})))
            }
            ("POST", [relay, "tokens"]) => {
                let Some(tokens) = s.relays.get_mut(*relay) else {
                    return ("404 Not Found", err(21001));
                };
                if tokens.len() >= 10 {
                    return ("400 Bad Request", err(21020));
                }
                let operations: Vec<String> = body["operations"]
                    .as_array()
                    .map(|o| {
                        o.iter()
                            .filter_map(|v| v.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
                let Some(expires) = body["expires"].as_str() else {
                    return ("400 Bad Request", err(21021));
                };
                let t = token(
                    relay,
                    operations,
                    expires.to_owned(),
                    body["label"].as_str().unwrap_or("").to_owned(),
                );
                tokens.push(t.clone());
                (
                    "201 Created",
                    ok(json!({"issuers": issuers(vec![view(&t, true)])})),
                )
            }
            ("DELETE", [relay, "tokens", jti]) => {
                let Some(tokens) = s.relays.get_mut(*relay) else {
                    return ("404 Not Found", err(21001));
                };
                let before = tokens.len();
                tokens.retain(|t| t.jti != *jti);
                if tokens.len() == before {
                    return ("404 Not Found", err(21002));
                }
                ("200 OK", ok(json!({})))
            }
            _ => ("404 Not Found", err(7003)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(fake: &fake::Fake) -> Cloudflare {
        Cloudflare::new(&fake.url, fake::ACCOUNT, fake::API_TOKEN).unwrap()
    }

    #[test]
    fn relays_are_created_without_their_default_tokens_and_tokens_are_scoped() {
        let fake = fake::serve();
        let cf = client(&fake);
        let relay = cf.create_relay("enginebox-session").unwrap();
        assert!(valid_id(&relay));
        assert!(
            fake.live(&relay).is_empty(),
            "the year-long defaults are revoked"
        );
        let at = 1_900_000_000;
        let watch = cf.mint(&relay, false, at, "watch").unwrap();
        let publish = cf.mint(&relay, true, at, "node").unwrap();
        let live = fake.live(&relay);
        assert_eq!(live.len(), 2);
        assert_eq!(live[0].operations, ["subscribe"]);
        assert_eq!(live[1].operations, ["publish", "subscribe"]);
        assert_eq!(live[0].expires, "2030-03-17T17:46:40Z");
        assert_eq!(watch.expires_at, at);
        assert!(watch.secret.contains(&relay));
        // Debug never shows a secret.
        assert!(!format!("{watch:?}").contains(&watch.secret));
        cf.revoke(&relay, &watch.jti).unwrap();
        cf.revoke(&relay, &watch.jti).unwrap(); // already gone
        assert_eq!(fake.live(&relay).len(), 1);
        assert_eq!(fake.live(&relay)[0].jti, publish.jti);
        cf.delete_relay(&relay).unwrap();
        cf.delete_relay(&relay).unwrap(); // already gone
        assert!(fake.state.lock().unwrap().relays.is_empty());
    }

    #[test]
    fn failures_are_value_free_and_bad_configuration_is_refused() {
        let fake = fake::serve();
        fake.fail_create
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let error = client(&fake).create_relay("x").unwrap_err().to_string();
        assert_eq!(
            error,
            "Cloudflare MoQ relay creation failed (HTTP 500, code 21000)"
        );
        let wrong =
            Cloudflare::new(&fake.url, fake::ACCOUNT, "another-api-token-value-0002").unwrap();
        let error = wrong
            .mint(&"a".repeat(32), false, 1, "x")
            .unwrap_err()
            .to_string();
        assert!(!error.contains("another-api-token"), "{error}");
        assert!(error.contains("HTTP 403"), "{error}");
        assert!(Cloudflare::new(&fake.url, "not-an-account", fake::API_TOKEN).is_err());
        assert!(Cloudflare::new(&fake.url, fake::ACCOUNT, "short").is_err());
        assert!(Cloudflare::new(&fake.url, fake::ACCOUNT, "has space in the api token").is_err());
        assert!(!format!("{:?}", client(&fake)).contains(fake::API_TOKEN));
        // Unreachable: no address, no secret in the message.
        let gone = Cloudflare::new("http://127.0.0.1:9", fake::ACCOUNT, fake::API_TOKEN).unwrap();
        assert_eq!(
            gone.create_relay("x").unwrap_err().to_string(),
            "Cloudflare is unreachable"
        );
    }
}
