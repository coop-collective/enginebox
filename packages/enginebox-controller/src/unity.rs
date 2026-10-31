//! Unity account sign-in (docs/ORGS.md, "Unity accounts").
//!
//! One call: `POST <login url>` with `{grant_type: "password", username,
//! password}`, ten-second timeout, no redirects. The answer is classified so
//! callers never handle Unity's raw reply: the password, the token and the
//! response body are never logged or returned.
use serde_json::{Value, json};
use std::time::Duration;

/// Unity's password login endpoint; `ENGINEBOX_UNITY_LOGIN_URL` overrides it.
pub const LOGIN_URL: &str = "https://core.cloud.unity3d.com/api/login";
pub const URL_ENV: &str = "ENGINEBOX_UNITY_LOGIN_URL";
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// The most of Unity's reply that is read.
const MAX_REPLY: usize = 64 * 1024;

/// What Unity said about a username and password.
#[derive(Debug, PartialEq, Eq)]
pub enum Login {
    /// Signed in; a short-lived access token.
    Token(String),
    /// Unity rejected the username or password.
    Invalid,
    /// The account needs a second factor that a password sign-in cannot give.
    TwoFactor,
    /// Unity could not be asked or gave no usable answer; a value-free reason:
    /// `unreachable`, `service_error` or `unexpected_response`.
    Unknown(&'static str),
}

#[derive(Clone, Debug)]
pub struct Client {
    url: String,
    timeout: Duration,
}

impl Client {
    /// The configured endpoint: `ENGINEBOX_UNITY_LOGIN_URL`, else Unity's.
    pub fn from_env() -> Self {
        Self::new(
            &std::env::var(URL_ENV)
                .ok()
                .filter(|u| !u.trim().is_empty())
                .unwrap_or_else(|| LOGIN_URL.to_owned()),
        )
    }
    pub fn new(url: &str) -> Self {
        Self {
            url: url.trim().to_owned(),
            timeout: TIMEOUT,
        }
    }
    #[cfg(test)]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sign in. Blocking: call it off the async runtime.
    pub fn login(&self, username: &str, password: &str) -> Login {
        let Ok(client) = reqwest::blocking::Client::builder()
            .timeout(self.timeout)
            .connect_timeout(self.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
        else {
            return Login::Unknown("unreachable");
        };
        let response = match client
            .post(&self.url)
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&json!({"grant_type": "password", "username": username, "password": password}))
            .send()
        {
            Ok(response) => response,
            Err(_) => return Login::Unknown("unreachable"),
        };
        let status = response.status().as_u16();
        let mut bytes = Vec::new();
        {
            use std::io::Read;
            if response
                .take(MAX_REPLY as u64)
                .read_to_end(&mut bytes)
                .is_err()
            {
                return Login::Unknown("unreachable");
            }
        }
        classify(status, &bytes)
    }
}

/// Map Unity's reply to a [`Login`]. A token means the password worked.
/// Otherwise a second-factor demand wins over every other reading, so such an
/// account is never reported as a wrong password.
pub fn classify(status: u16, body: &[u8]) -> Login {
    let value: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    if (200..300).contains(&status)
        && let Some(token) = ["access_token", "accessToken"]
            .iter()
            .find_map(|k| value[*k].as_str().filter(|t| !t.is_empty()))
    {
        return Login::Token(token.to_owned());
    }
    if asks_second_factor(body) {
        return Login::TwoFactor;
    }
    if (200..300).contains(&status) {
        return Login::Unknown("unexpected_response");
    }
    if status == 401 || value["code"] == "143.002" {
        return Login::Invalid;
    }
    if status >= 500 || status == 429 {
        return Login::Unknown("service_error");
    }
    Login::Unknown("unexpected_response")
}

/// Whether a reply asks for two-factor sign-in or a verification step. Words
/// are compared, not substrings, so identifiers do not match by accident.
fn asks_second_factor(body: &[u8]) -> bool {
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    let words: Vec<&str> = text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    words.iter().any(|w| {
        w.starts_with("tfa")
            || w.starts_with("2fa")
            || w.starts_with("mfa")
            || w.starts_with("twofactor")
            || w.starts_with("verification")
            || *w == "otp"
    }) || words.windows(2).any(|p| p == ["two", "factor"])
}

/// A local stand-in for Unity's login endpoint (tests only). The password
/// picks the reply: `right-password` and `camel-password` sign in, `tfa-password`
/// asks for a second factor, `down-password` is a 500, `slow-password` never
/// answers in time, and anything else is Unity's "Invalid Credential".
#[cfg(test)]
pub mod fake {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{Arc, Mutex},
        thread,
        time::Duration,
    };

    pub const TOKEN: &str = "fake-unity-access-token-0001";

    /// Start the server; returns its URL and every request body it received.
    pub fn serve() -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/api/login", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let log = log.clone();
                thread::spawn(move || {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let body = loop {
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
                                break text[end + 4..end + 4 + length].to_owned();
                            }
                        }
                    };
                    log.lock().unwrap().push(body.clone());
                    let password = serde_json::from_str::<serde_json::Value>(&body)
                        .ok()
                        .and_then(|v| v["password"].as_str().map(str::to_owned))
                        .unwrap_or_default();
                    let (status, reply) = match password.as_str() {
                        "right-password" => (
                            "200 OK",
                            format!(r#"{{"access_token":"{TOKEN}","expires_in":3600}}"#),
                        ),
                        "camel-password" => ("200 OK", format!(r#"{{"accessToken":"{TOKEN}"}}"#)),
                        "tfa-password" => (
                            "200 OK",
                            r#"{"tfa":{"type":"email"},"message":"TFA required"}"#.to_owned(),
                        ),
                        "down-password" => (
                            "500 Internal Server Error",
                            r#"{"message":"oops"}"#.to_owned(),
                        ),
                        "slow-password" => {
                            thread::sleep(Duration::from_secs(3));
                            ("200 OK", "{}".to_owned())
                        }
                        _ => (
                            "401 Unauthorized",
                            r#"{"message":"Invalid Credential","code":"143.002"}"#.to_owned(),
                        ),
                    };
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                        reply.len()
                    );
                });
            }
        });
        (url, seen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_are_classified() {
        assert_eq!(
            classify(200, br#"{"access_token":"t1"}"#),
            Login::Token("t1".into())
        );
        assert_eq!(
            classify(200, br#"{"accessToken":"t2"}"#),
            Login::Token("t2".into())
        );
        // A token is a token, whatever letters it happens to contain.
        assert_eq!(
            classify(200, br#"{"access_token":"x-tfa-2fa"}"#),
            Login::Token("x-tfa-2fa".into())
        );
        assert_eq!(
            classify(200, br#"{"other":1}"#),
            Login::Unknown("unexpected_response")
        );
        assert_eq!(
            classify(401, br#"{"message":"Invalid Credential","code":"143.002"}"#),
            Login::Invalid
        );
        assert_eq!(
            classify(401, br#"{"message":"TFA required","code":"143.003"}"#),
            Login::TwoFactor
        );
        assert_eq!(
            classify(403, br#"{"message":"Email verification required"}"#),
            Login::TwoFactor
        );
        assert_eq!(
            classify(502, b"bad gateway"),
            Login::Unknown("service_error")
        );
        assert_eq!(classify(429, b""), Login::Unknown("service_error"));
        assert_eq!(classify(302, b""), Login::Unknown("unexpected_response"));
    }

    #[test]
    fn the_fake_server_answers_like_unity() {
        let (url, seen) = fake::serve();
        let client = Client::new(&url).with_timeout(Duration::from_secs(1));
        assert_eq!(
            client.login("a@example.com", "right-password"),
            Login::Token(fake::TOKEN.into())
        );
        assert_eq!(
            client.login("a@example.com", "camel-password"),
            Login::Token(fake::TOKEN.into())
        );
        assert_eq!(client.login("a@example.com", "nope-nope"), Login::Invalid);
        assert_eq!(
            client.login("a@example.com", "tfa-password"),
            Login::TwoFactor
        );
        assert_eq!(
            client.login("a@example.com", "down-password"),
            Login::Unknown("service_error")
        );
        assert_eq!(
            client.login("a@example.com", "slow-password"),
            Login::Unknown("unreachable")
        );
        let first: Value = serde_json::from_str(&seen.lock().unwrap()[0]).unwrap();
        assert_eq!(
            first,
            json!({"grant_type":"password","username":"a@example.com","password":"right-password"})
        );
        // Nothing listens here.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead = format!("http://{}/api/login", closed.local_addr().unwrap());
        drop(closed);
        assert_eq!(
            Client::new(&dead).login("a@example.com", "right-password"),
            Login::Unknown("unreachable")
        );
    }
}
