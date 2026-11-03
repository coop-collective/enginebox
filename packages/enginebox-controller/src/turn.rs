//! Cloudflare TURN credentials for the engine-session WebRTC fallback
//! (docs/ENGINE-SESSIONS.md, "WebRTC fallback").
//!
//! Browsers that cannot reach the MoQ relay (UDP 443 blocked, as some VPNs
//! do) watch over WebRTC instead, with media relayed through Cloudflare TURN
//! on both sides (`iceTransportPolicy: "relay"`), including TURN over TLS on
//! port 443. The key comes from the platform's `cloudflare-turn` connection
//! (`TURN_KEY_ID`, `TURN_API_TOKEN`). Credentials are returned to callers and
//! never logged; errors carry only the HTTP status.
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::time::Duration;

/// Cloudflare Realtime's API; tests point the controller at a stand-in.
pub const API_BASE: &str = "https://rtc.live.cloudflare.com/v1";
/// The platform org's `secret_source` connection holding the TURN key.
pub const CONNECTION: &str = "cloudflare-turn";
pub const KEY_ID: &str = "TURN_KEY_ID";
pub const API_TOKEN: &str = "TURN_API_TOKEN";
const TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REPLY: u64 = 16 * 1024;

/// Mints short-lived ICE servers (TURN credentials included).
pub trait Issuer: Send + Sync {
    fn ice_servers(&self, ttl_seconds: u64) -> Result<Vec<Value>>;
}

pub struct Cloudflare {
    base: String,
    key: String,
    token: String,
}
impl std::fmt::Debug for Cloudflare {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cloudflare")
            .field("key", &self.key)
            .finish()
    }
}
impl Cloudflare {
    pub fn new(base: &str, key: &str, token: &str) -> Result<Self> {
        let (key, token) = (key.trim(), token.trim());
        ensure!(
            crate::moq::valid_id(key),
            "TURN_KEY_ID must be a 32-character Cloudflare TURN key ID."
        );
        ensure!(
            (20..=512).contains(&token.len()) && token.bytes().all(|b| (33..=126).contains(&b)),
            "TURN_API_TOKEN must be the TURN key's API token."
        );
        Ok(Self {
            base: base.trim_end_matches('/').to_owned(),
            key: key.to_owned(),
            token: token.to_owned(),
        })
    }
}
impl Issuer for Cloudflare {
    fn ice_servers(&self, ttl_seconds: u64) -> Result<Vec<Value>> {
        let client = reqwest::blocking::Client::builder()
            .timeout(TIMEOUT)
            .connect_timeout(TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("Cloudflare client unavailable")?;
        let response = client
            .post(format!(
                "{}/turn/keys/{}/credentials/generate-ice-servers",
                self.base, self.key
            ))
            .bearer_auth(&self.token)
            .json(&json!({"ttl": ttl_seconds}))
            .send()
            .map_err(|_| anyhow::anyhow!("Cloudflare TURN is unreachable"))?;
        let status = response.status().as_u16();
        let mut bytes = Vec::new();
        {
            use std::io::Read;
            response
                .take(MAX_REPLY)
                .read_to_end(&mut bytes)
                .map_err(|_| anyhow::anyhow!("Cloudflare TURN's reply could not be read"))?;
        }
        if !(200..300).contains(&status) {
            bail!("Cloudflare TURN credentials failed (HTTP {status})")
        }
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("Cloudflare TURN returned an invalid reply"))?;
        servers(&value["iceServers"])
    }
}

/// Keeps only Cloudflare STUN/TURN URLs, drops port 53 (browsers block it)
/// and requires at least one TURN server with credentials.
pub fn servers(value: &Value) -> Result<Vec<Value>> {
    let list = value
        .as_array()
        .filter(|l| !l.is_empty() && l.len() <= 8)
        .context("Cloudflare TURN returned no ICE servers")?;
    let mut out = Vec::new();
    let mut relay = false;
    for item in list {
        let urls: Vec<&str> = match &item["urls"] {
            Value::String(url) => vec![url.as_str()],
            Value::Array(urls) => urls.iter().filter_map(Value::as_str).collect(),
            _ => vec![],
        };
        ensure!(
            !urls.is_empty() && urls.len() <= 8,
            "Cloudflare TURN returned an invalid ICE server"
        );
        let kept: Vec<&str> = urls.into_iter().filter(|u| allowed(u)).collect();
        if kept.is_empty() {
            continue;
        }
        if kept.iter().any(|u| u.starts_with("turn")) {
            let (Some(username), Some(credential)) =
                (item["username"].as_str(), item["credential"].as_str())
            else {
                bail!("Cloudflare TURN returned a relay without credentials")
            };
            ensure!(
                !username.is_empty() && username.len() <= 1024 && credential.len() <= 2048,
                "Cloudflare TURN returned invalid credentials"
            );
            relay = true;
            out.push(json!({"urls": kept, "username": username, "credential": credential}));
        } else {
            out.push(json!({"urls": kept}));
        }
    }
    ensure!(relay, "Cloudflare TURN returned no relay server");
    Ok(out)
}

fn allowed(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once(':') else {
        return false;
    };
    let (address, query) = rest.split_once('?').unwrap_or((rest, ""));
    let Some((host, port)) = address.rsplit_once(':') else {
        return false;
    };
    let host_ok = match scheme {
        "stun" => host == "stun.cloudflare.com",
        "turn" | "turns" => host == "turn.cloudflare.com",
        _ => false,
    };
    host_ok
        && matches!(port, "80" | "443" | "3478" | "5349")
        && matches!(query, "" | "transport=udp" | "transport=tcp")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_cloudflare_servers_and_drops_port_53() {
        let reply = json!([
            {"urls": ["stun:stun.cloudflare.com:3478", "stun:stun.cloudflare.com:53"]},
            {"urls": ["turn:turn.cloudflare.com:3478?transport=udp", "turns:turn.cloudflare.com:443?transport=tcp",
                      "turn:turn.cloudflare.com:53?transport=udp", "turn:evil.example:443"],
             "username": "u", "credential": "c"},
        ]);
        let kept = servers(&reply).unwrap();
        assert_eq!(kept[0]["urls"], json!(["stun:stun.cloudflare.com:3478"]));
        assert_eq!(
            kept[1]["urls"],
            json!([
                "turn:turn.cloudflare.com:3478?transport=udp",
                "turns:turn.cloudflare.com:443?transport=tcp"
            ])
        );
        assert_eq!(kept[1]["username"], "u");
    }

    #[test]
    fn refuses_replies_without_a_relay() {
        assert!(servers(&json!([{"urls": ["stun:stun.cloudflare.com:3478"]}])).is_err());
        assert!(servers(&json!([{"urls": ["turn:turn.cloudflare.com:3478"]}])).is_err());
        assert!(servers(&json!([])).is_err());
        assert!(Cloudflare::new(API_BASE, "short", "x".repeat(40).as_str()).is_err());
    }
}

/// A stand-in for Cloudflare's TURN credential endpoint (tests only).
#[cfg(test)]
pub mod fake {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    pub const KEY: &str = "0123456789abcdef0123456789abcdef";
    pub const API_TOKEN: &str = "fake-cloudflare-turn-api-token-0001";
    pub const CREDENTIAL: &str = "fake-turn-credential";

    /// Answers `POST /v1/turn/keys/{KEY}/credentials/generate-ice-servers`
    /// with the bearer [`API_TOKEN`]; anything else is a 403.
    pub fn serve() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                thread::spawn(move || {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !String::from_utf8_lossy(&buf).contains("\r\n\r\n") {
                        match stream.read(&mut chunk) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&buf).to_string();
                    let ok = head.starts_with(&format!(
                        "POST /v1/turn/keys/{KEY}/credentials/generate-ice-servers "
                    )) && head.lines().any(|l| {
                        l.to_ascii_lowercase().starts_with("authorization:")
                            && l.trim_end().ends_with(&format!("Bearer {API_TOKEN}"))
                    });
                    let (status, text) = if ok {
                        ("200 OK", serde_json::json!({"iceServers": [
                            {"urls": ["stun:stun.cloudflare.com:3478", "stun:stun.cloudflare.com:53"]},
                            {"urls": ["turn:turn.cloudflare.com:3478?transport=udp", "turns:turn.cloudflare.com:443?transport=tcp", "turn:turn.cloudflare.com:53?transport=udp"],
                             "username": "fake-turn-user", "credential": CREDENTIAL},
                        ]}).to_string())
                    } else {
                        ("403 Forbidden", "{}".to_owned())
                    };
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                        text.len()
                    );
                });
            }
        });
        url
    }
}
