//! Signed run callbacks. On a terminal state the controller POSTs a small JSON
//! summary (never output or env) to the caller's HTTPS URL, signed with an
//! HMAC key the owner configured on the service token or profile.
//!
//! SSRF: HTTPS only, no credentials or fragment, and the host must resolve
//! only to public addresses; the vetted addresses are pinned for the
//! connection (no second lookup), redirects and proxies are off.
use anyhow::{Context, Result, ensure};
use enginebox_agents::service::{AgentService, FinishedRun};
use sha2::{Digest, Sha256};
use std::{
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    sync::Arc,
    time::Duration,
};

pub const SIGNATURE_HEADER: &str = "X-EngineBox-Signature";
pub const DELIVERY_HEADER: &str = "X-EngineBox-Delivery";
pub const EVENT_HEADER: &str = "X-EngineBox-Event";
/// Largest callback body. The payload is a fixed set of short fields.
pub const MAX_PAYLOAD: usize = 4096;
/// Delivery attempts and the wait before each retry.
pub const BACKOFF_SECONDS: [u64; 4] = [2, 10, 60, 300];

/// HMAC-SHA256 (RFC 2104), lowercase hex.
pub fn hmac_sha256_hex(key: &[u8], message: &[u8]) -> String {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|b| b ^ 0x36));
    inner.update(message);
    let mut outer = Sha256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    format!("{:x}", outer.finalize())
}

/// `t=<unix seconds>,v1=<hex HMAC-SHA256 of "<t>.<body>">`.
pub fn signature(secret: &str, timestamp: i64, body: &[u8]) -> String {
    let mut message = format!("{timestamp}.").into_bytes();
    message.extend_from_slice(body);
    format!(
        "t={timestamp},v1={}",
        hmac_sha256_hex(secret.as_bytes(), &message)
    )
}

/// Syntax check at launch: HTTPS, a host, no credentials or fragment, and an
/// IP literal must be public. Name resolution is checked at delivery.
pub fn validate_url(input: &str) -> Result<url::Url> {
    ensure!(
        input.len() <= 2048 && !input.chars().any(|c| c.is_whitespace() || c.is_control()),
        "callback.url must be an HTTPS URL of at most 2048 bytes"
    );
    let url = url::Url::parse(input).context("callback.url must be an HTTPS URL")?;
    ensure!(
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "callback.url must be HTTPS without credentials or fragment"
    );
    match url.host().context("callback.url needs a host")? {
        url::Host::Ipv4(ip) => ensure!(
            enginebox_core::templates::public_ip(IpAddr::V4(ip)),
            "callback.url must not target a private, loopback or link-local address"
        ),
        url::Host::Ipv6(ip) => ensure!(
            enginebox_core::templates::public_ip(IpAddr::V6(ip)),
            "callback.url must not target a private, loopback or link-local address"
        ),
        url::Host::Domain(domain) => ensure!(
            !domain.eq_ignore_ascii_case("localhost")
                && !domain.to_ascii_lowercase().ends_with(".localhost"),
            "callback.url must not target localhost"
        ),
    }
    Ok(url)
}

/// Every resolved address must be public; an empty answer is refused.
pub fn vet_addresses(addresses: &[SocketAddr]) -> Result<()> {
    ensure!(
        !addresses.is_empty()
            && addresses
                .iter()
                .all(|a| enginebox_core::templates::public_ip(a.ip())),
        "callback host must resolve only to public Internet addresses"
    );
    Ok(())
}

fn resolve(url: &url::Url) -> Result<Vec<SocketAddr>> {
    let host = url
        .host_str()
        .context("callback.url needs a host")?
        .to_owned();
    let port = url.port_or_known_default().unwrap_or(443);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let host = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        let _ = tx.send(
            (host.as_str(), port)
                .to_socket_addrs()
                .map(|i| i.collect::<Vec<_>>()),
        );
    });
    let addresses = rx
        .recv_timeout(Duration::from_secs(5))
        .context("callback DNS timed out")??;
    vet_addresses(&addresses)?;
    Ok(addresses)
}

/// One HTTP attempt: returns the response status.
pub type Transport =
    dyn Fn(&url::Url, &[SocketAddr], &[(String, String)], &[u8]) -> Result<u16> + Send + Sync;

fn https_transport(
    url: &url::Url,
    addresses: &[SocketAddr],
    headers: &[(String, String)],
    body: &[u8],
) -> Result<u16> {
    let host = url.host_str().context("callback.url needs a host")?;
    let client = reqwest::blocking::Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(5))
        .resolve_to_addrs(host, addresses)
        .build()?;
    let mut request = client.post(url.clone()).body(body.to_vec());
    for (k, v) in headers {
        request = request.header(k, v);
    }
    // The response body is never read or stored.
    Ok(request.send()?.status().as_u16())
}

/// Outcome of one delivery: final state, attempts made, last HTTP status.
#[derive(Debug, PartialEq)]
pub struct Outcome {
    pub delivered: bool,
    pub attempts: u32,
    pub last_status: Option<u16>,
}

/// Deliver with retries: 2xx succeeds; 408, 429, 5xx and network errors are
/// retried with backoff; any other status stops. The host is re-vetted on
/// every attempt so a DNS change cannot point a retry at a private address.
pub fn deliver(
    finished: &FinishedRun,
    transport: &Transport,
    resolver: &(dyn Fn(&url::Url) -> Result<Vec<SocketAddr>> + Send + Sync),
    sleep: &(dyn Fn(Duration) + Send + Sync),
    on_attempt: &(dyn Fn(u32, Option<u16>) + Send + Sync),
) -> Outcome {
    let body = serde_json::to_vec(&finished.payload).unwrap_or_default();
    let delivery = uuid::Uuid::new_v4().simple().to_string();
    let mut outcome = Outcome {
        delivered: false,
        attempts: 0,
        last_status: None,
    };
    let Ok(url) = validate_url(&finished.callback.url) else {
        return outcome;
    };
    if body.len() > MAX_PAYLOAD {
        return outcome;
    }
    for attempt in 0..=BACKOFF_SECONDS.len() {
        if attempt > 0 {
            sleep(Duration::from_secs(BACKOFF_SECONDS[attempt - 1]));
        }
        outcome.attempts += 1;
        let status = resolver(&url).and_then(|addresses| {
            let headers = vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("User-Agent".into(), "EngineBox-callback/1".into()),
                (EVENT_HEADER.into(), "agent_run.finished".into()),
                (DELIVERY_HEADER.into(), delivery.clone()),
                (
                    SIGNATURE_HEADER.into(),
                    signature(
                        &finished.callback.secret,
                        chrono::Utc::now().timestamp(),
                        &body,
                    ),
                ),
            ];
            transport(&url, &addresses, &headers, &body)
        });
        outcome.last_status = status.as_ref().ok().copied();
        on_attempt(outcome.attempts, outcome.last_status);
        match status {
            Ok(code) if (200..300).contains(&code) => {
                outcome.delivered = true;
                return outcome;
            }
            Ok(code) if code == 408 || code == 429 || code >= 500 => {}
            Ok(_) => return outcome,
            Err(_) => {}
        }
    }
    outcome
}

/// Register the production deliverer on the agent service.
pub fn install(agents: &AgentService) {
    let service = agents.clone();
    agents.set_finish_hook(Arc::new(move |finished: FinishedRun| {
        let service = service.clone();
        std::thread::spawn(move || {
            let recorder = service.clone();
            let id = finished.id.clone();
            let outcome = deliver(
                &finished,
                &https_transport,
                &resolve,
                &std::thread::sleep,
                &move |attempts, status| {
                    let _ = recorder.record_callback(&id, "delivering", attempts, status);
                },
            );
            let _ = service.record_callback(
                &finished.id,
                if outcome.delivered {
                    "delivered"
                } else {
                    "failed"
                },
                outcome.attempts,
                outcome.last_status,
            );
        });
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use enginebox_agents::service::Callback;
    use serde_json::json;
    use std::sync::Mutex;
    type Sent = (Vec<(String, String)>, Vec<u8>);

    #[test]
    fn hmac_matches_rfc_4231_and_signature_binds_timestamp_and_body() {
        // RFC 4231 test case 2.
        assert_eq!(
            hmac_sha256_hex(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Test case 6: a key longer than the block is hashed first.
        assert_eq!(
            hmac_sha256_hex(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            ),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
        let sig = signature("k".repeat(32).as_str(), 1700000000, b"{}");
        assert!(sig.starts_with("t=1700000000,v1="));
        assert_ne!(sig, signature("k".repeat(32).as_str(), 1700000001, b"{}"));
        assert_ne!(sig, signature("k".repeat(32).as_str(), 1700000000, b"{ }"));
    }

    #[test]
    fn urls_must_be_https_public_and_credential_free() {
        for bad in [
            "http://example.com/hook",
            "https://user:pw@example.com/hook",
            "https://example.com/hook#frag",
            "https://127.0.0.1/hook",
            "https://10.0.0.5/hook",
            "https://192.168.1.1/hook",
            "https://169.254.169.254/latest/meta-data",
            "https://[::1]/hook",
            "https://[fe80::1]/hook",
            "https://[::ffff:127.0.0.1]/hook",
            "https://100.64.0.1/hook",
            "https://localhost/hook",
            "https://api.localhost/hook",
            "file:///etc/passwd",
            "https://exa mple.com",
        ] {
            assert!(validate_url(bad).is_err(), "{bad} accepted");
        }
        assert!(validate_url("https://hooks.example.com:8443/run?x=1").is_ok());
        assert!(validate_url("https://93.184.216.34/hook").is_ok());
        // Names that resolve to private space are refused after resolution.
        for private in [
            "127.0.0.1:443",
            "10.1.2.3:443",
            "169.254.169.254:443",
            "[::1]:443",
        ] {
            let addr: SocketAddr = private.parse().unwrap();
            assert!(vet_addresses(&[addr]).is_err());
            let public: SocketAddr = "93.184.216.34:443".parse().unwrap();
            assert!(
                vet_addresses(&[public, addr]).is_err(),
                "mixed answer accepted"
            );
        }
        assert!(vet_addresses(&[]).is_err());
    }

    fn finished(url: &str) -> FinishedRun {
        FinishedRun {
            id: "a".repeat(32),
            callback: Callback {
                url: url.into(),
                secret: "s".repeat(40),
            },
            payload: json!({"schema":1,"run_id":"a".repeat(32),"status":"succeeded"}),
        }
    }

    #[test]
    fn delivery_signs_retries_with_backoff_and_stops_on_permanent_errors() {
        let sent: Arc<Mutex<Vec<Sent>>> = Arc::default();
        let statuses = Arc::new(Mutex::new(vec![503u16, 429, 200]));
        let sleeps: Arc<Mutex<Vec<u64>>> = Arc::default();
        let (s1, st, sl) = (sent.clone(), statuses.clone(), sleeps.clone());
        let outcome = deliver(
            &finished("https://hooks.example.com/run"),
            &move |_, addrs: &[SocketAddr], headers: &[(String, String)], body: &[u8]| {
                assert!(addrs.iter().all(|a| a.ip().to_string() == "93.184.216.34"));
                s1.lock().unwrap().push((headers.to_vec(), body.to_vec()));
                Ok(st.lock().unwrap().remove(0))
            },
            &|_| Ok(vec!["93.184.216.34:443".parse().unwrap()]),
            &move |d| sl.lock().unwrap().push(d.as_secs()),
            &|_, _| {},
        );
        assert_eq!(
            outcome,
            Outcome {
                delivered: true,
                attempts: 3,
                last_status: Some(200)
            }
        );
        assert_eq!(*sleeps.lock().unwrap(), [2, 10]);
        let sent = sent.lock().unwrap();
        let (headers, body) = &sent[2];
        let header = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        let sig = header(SIGNATURE_HEADER);
        let (t, v1) = sig.strip_prefix("t=").unwrap().split_once(",v1=").unwrap();
        let mut message = format!("{t}.").into_bytes();
        message.extend_from_slice(body);
        assert_eq!(v1, hmac_sha256_hex("s".repeat(40).as_bytes(), &message));
        // The key itself never travels.
        assert!(!headers.iter().any(|(_, v)| v.contains(&"s".repeat(40))));
        assert!(!String::from_utf8_lossy(body).contains(&"s".repeat(40)));
        // One delivery ID across retries, for receiver deduplication.
        assert_eq!(header(DELIVERY_HEADER).len(), 32);
        assert!(
            sent.iter()
                .all(|(h, _)| h.contains(&(DELIVERY_HEADER.to_string(), header(DELIVERY_HEADER))))
        );

        let calls = Arc::new(Mutex::new(0));
        let c = calls.clone();
        let outcome = deliver(
            &finished("https://hooks.example.com/run"),
            &move |_, _: &[SocketAddr], _: &[(String, String)], _: &[u8]| {
                *c.lock().unwrap() += 1;
                Ok(404)
            },
            &|_| Ok(vec!["93.184.216.34:443".parse().unwrap()]),
            &|_| {},
            &|_, _| {},
        );
        assert_eq!(*calls.lock().unwrap(), 1);
        assert!(!outcome.delivered);
        assert_eq!(outcome.last_status, Some(404));

        // Bounded: every retry used, then failed.
        let outcome = deliver(
            &finished("https://hooks.example.com/run"),
            &|_, _: &[SocketAddr], _: &[(String, String)], _: &[u8]| Ok(500),
            &|_| Ok(vec!["93.184.216.34:443".parse().unwrap()]),
            &|_| {},
            &|_, _| {},
        );
        assert_eq!(outcome.attempts as usize, BACKOFF_SECONDS.len() + 1);
        assert!(!outcome.delivered);
    }

    #[test]
    fn delivery_refuses_private_resolution_and_never_connects() {
        let connected = Arc::new(Mutex::new(false));
        let c = connected.clone();
        let outcome = deliver(
            &finished("https://rebind.example.com/run"),
            &move |_, _: &[SocketAddr], _: &[(String, String)], _: &[u8]| {
                *c.lock().unwrap() = true;
                Ok(200)
            },
            &|_| {
                let addresses = vec!["169.254.169.254:443".parse().unwrap()];
                vet_addresses(&addresses)?;
                Ok(addresses)
            },
            &|_| {},
            &|_, _| {},
        );
        assert!(!*connected.lock().unwrap());
        assert!(!outcome.delivered);
        // A literal private URL is refused before any attempt.
        let outcome = deliver(
            &finished("https://10.0.0.1/run"),
            &|_, _: &[SocketAddr], _: &[(String, String)], _: &[u8]| Ok(200),
            &|_| Ok(vec![]),
            &|_| {},
            &|_, _| {},
        );
        assert_eq!(outcome.attempts, 0);
    }
}
