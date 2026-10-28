//! Read-only, credential-free egress checks. Transport success is never a media
//! session, TURN allocation, tunnel authorization or engine qualification.
use anyhow::{Result, ensure};
use futures_util::{StreamExt, stream::FuturesUnordered};
use serde::Serialize;
use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::net::{TcpStream, UdpSocket, lookup_host};

const DEADLINE: Duration = Duration::from_secs(8);

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Profile {
    Control,
    Streaming,
}

#[derive(Clone, Debug, clap::Args)]
pub struct Options {
    /// Controller HTTPS origin. Its /health identity is verified without credentials.
    #[arg(long)]
    pub controller: Option<String>,
    #[arg(long, value_enum, default_value = "streaming")]
    pub profile: Profile,
    /// Fail this command when the streaming QUIC transport cannot connect.
    #[arg(long)]
    pub require_quic: bool,
    /// Also check both Cloudflare Tunnel regions over TCP/7844 (not tunnel authentication).
    #[arg(long)]
    pub with_tunnel: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Passed,
    Failed,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    pub id: &'static str,
    pub status: Status,
    pub required: bool,
    pub elapsed_ms: u64,
    pub code: &'static str,
    pub scope: &'static str,
    pub hint: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub schema: u32,
    pub checked_at_unix_ms: u64,
    pub profile: Profile,
    pub required_checks_passed: bool,
    pub has_warnings: bool,
    pub streaming_qualified: bool,
    pub media_session: &'static str,
    pub turn_allocation: &'static str,
    pub tunnel_authorization: &'static str,
    pub network_settings_changed: bool,
    pub checks: Vec<Check>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failure {
    Dns,
    Connect,
    Tls,
    Quic,
    Timeout,
    Response,
    Local,
}
impl Failure {
    fn code(self) -> &'static str {
        match self {
            Self::Dns => "dns-failed",
            Self::Connect => "connection-failed",
            Self::Tls => "tls-verification-or-handshake-failed",
            Self::Quic => "quic-handshake-failed",
            Self::Timeout => "timeout",
            Self::Response => "unexpected-response",
            Self::Local => "local-probe-failed",
        }
    }
}
type Outcome = std::result::Result<(), Failure>;

#[derive(Clone)]
enum Probe {
    Controller(reqwest::Url),
    Quic(&'static str, u16),
    Tls(&'static str, u16),
    Stun(&'static str, u16),
    Tcp(&'static str, u16),
}

fn tls_config(alpn: &[&[u8]], roots: rustls::RootCertStore) -> rustls::ClientConfig {
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("supported TLS versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|value| value.to_vec()).collect();
    config
}
fn roots() -> rustls::RootCertStore {
    rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned())
}
async fn addresses(host: &str, port: u16) -> std::result::Result<Vec<SocketAddr>, Failure> {
    let all = lookup_host((host, port)).await.map_err(|_| Failure::Dns)?;
    // Race both families; an unusable IPv6 route must not suppress working IPv4.
    let mut selected = Vec::new();
    for address in all {
        if !selected.contains(&address)
            && selected
                .iter()
                .filter(|a: &&SocketAddr| a.is_ipv4() == address.is_ipv4())
                .count()
                < 2
        {
            selected.push(address);
        }
    }
    if selected.is_empty() {
        Err(Failure::Dns)
    } else {
        Ok(selected)
    }
}
fn bind_for(address: SocketAddr) -> SocketAddr {
    if address.is_ipv4() {
        ([0, 0, 0, 0], 0).into()
    } else {
        ([0u16; 8], 0).into()
    }
}

struct Endpoint(quinn::Endpoint);
impl Drop for Endpoint {
    fn drop(&mut self) {
        self.0.close(0u32.into(), b"network check complete");
    }
}
async fn quic(address: SocketAddr, host: &str, config: Arc<rustls::ClientConfig>) -> Outcome {
    let mut endpoint =
        Endpoint(quinn::Endpoint::client(bind_for(address)).map_err(|_| Failure::Local)?);
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from((*config).clone())
        .map_err(|_| Failure::Local)?;
    endpoint
        .0
        .set_default_client_config(quinn::ClientConfig::new(Arc::new(crypto)));
    let connection = endpoint
        .0
        .connect(address, host)
        .map_err(|_| Failure::Local)?
        .await
        .map_err(|error| {
            if matches!(error, quinn::ConnectionError::TimedOut) {
                Failure::Timeout
            } else {
                Failure::Quic
            }
        })?;
    connection.close(0u32.into(), b"network check complete");
    endpoint.0.close(0u32.into(), b"network check complete");
    let _ = tokio::time::timeout(Duration::from_millis(200), endpoint.0.wait_idle()).await;
    Ok(())
}
async fn tls(address: SocketAddr, host: &str, config: Arc<rustls::ClientConfig>) -> Outcome {
    let stream = TcpStream::connect(address)
        .await
        .map_err(|_| Failure::Connect)?;
    let name =
        rustls::pki_types::ServerName::try_from(host.to_owned()).map_err(|_| Failure::Local)?;
    tokio_rustls::TlsConnector::from(config)
        .connect(name, stream)
        .await
        .map_err(|_| Failure::Tls)?;
    Ok(())
}

fn binding_success(bytes: &[u8], transaction: &[u8]) -> bool {
    if bytes.len() < 20
        || bytes[0..2] != [1, 1]
        || bytes[4..8] != [0x21, 0x12, 0xa4, 0x42]
        || &bytes[8..20] != transaction
    {
        return false;
    }
    let length = u16::from_be_bytes([bytes[2], bytes[3]]) as usize;
    if !length.is_multiple_of(4) || length + 20 != bytes.len() {
        return false;
    }
    let mut offset = 20;
    let mut mapped = false;
    while offset + 4 <= bytes.len() {
        let kind = u16::from_be_bytes([bytes[offset], bytes[offset + 1]]);
        let size = u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]) as usize;
        let start = offset + 4;
        let end = start + size;
        if end > bytes.len() {
            return false;
        }
        if kind == 0x0020 && size >= 4 {
            mapped |= matches!((bytes[start + 1], size), (1, 8) | (2, 20));
        }
        offset = start + size.div_ceil(4) * 4;
    }
    mapped && offset == bytes.len()
}
async fn stun(address: SocketAddr) -> Outcome {
    let socket = UdpSocket::bind(bind_for(address))
        .await
        .map_err(|_| Failure::Local)?;
    socket
        .connect(address)
        .await
        .map_err(|_| Failure::Connect)?;
    let id = uuid::Uuid::new_v4();
    let transaction = &id.as_bytes()[..12];
    let mut request = [0u8; 20];
    request[1] = 1;
    request[4..8].copy_from_slice(&[0x21, 0x12, 0xa4, 0x42]);
    request[8..20].copy_from_slice(transaction);
    socket.send(&request).await.map_err(|_| Failure::Connect)?;
    let mut reply = [0u8; 4096];
    let count = socket
        .recv(&mut reply)
        .await
        .map_err(|_| Failure::Connect)?;
    if binding_success(&reply[..count], transaction) {
        Ok(())
    } else {
        Err(Failure::Response)
    }
}

async fn controller(origin: &reqwest::Url) -> Outcome {
    let client = reqwest::Client::builder()
        .timeout(DEADLINE)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| Failure::Local)?;
    let url = origin.join("/health").map_err(|_| Failure::Local)?;
    let mut response = client.get(url).send().await.map_err(|error| {
        if error.is_timeout() {
            Failure::Timeout
        } else {
            Failure::Connect
        }
    })?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(Failure::Response);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Failure::Response)? {
        if bytes.len() + chunk.len() > 16384 {
            return Err(Failure::Response);
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| Failure::Response)?;
    if value["ok"] == true && value["service"] == "enginebox-fleet" && value["schema"] == 1 {
        Ok(())
    } else {
        Err(Failure::Response)
    }
}

async fn execute(probe: &Probe) -> Outcome {
    if let Probe::Controller(origin) = probe {
        return controller(origin).await;
    }
    let (host, port) = match probe {
        Probe::Quic(h, p) | Probe::Tls(h, p) | Probe::Stun(h, p) | Probe::Tcp(h, p) => (*h, *p),
        _ => unreachable!(),
    };
    let addresses = addresses(host, port).await?;
    let config = Arc::new(tls_config(
        if matches!(probe, Probe::Quic(..)) {
            &[b"h3"]
        } else {
            &[]
        },
        roots(),
    ));
    let mut pending = FuturesUnordered::new();
    for address in addresses {
        let config = config.clone();
        pending.push(async move {
            match probe {
                Probe::Quic(..) => quic(address, host, config).await,
                Probe::Tls(..) => tls(address, host, config).await,
                Probe::Stun(..) => stun(address).await,
                Probe::Tcp(..) => TcpStream::connect(address)
                    .await
                    .map(|_| ())
                    .map_err(|_| Failure::Connect),
                _ => unreachable!(),
            }
        });
    }
    let mut failure = Failure::Connect;
    while let Some(result) = pending.next().await {
        match result {
            Ok(()) => return Ok(()),
            Err(error) => failure = error,
        }
    }
    Err(failure)
}
async fn check(
    id: &'static str,
    required: bool,
    scope: &'static str,
    hint: &'static str,
    probe: Probe,
) -> Check {
    let started = Instant::now();
    let result = tokio::time::timeout(DEADLINE, execute(&probe))
        .await
        .unwrap_or(Err(Failure::Timeout));
    Check {
        id,
        required,
        scope,
        hint,
        elapsed_ms: started.elapsed().as_millis() as u64,
        status: if result.is_ok() {
            Status::Passed
        } else {
            Status::Failed
        },
        code: result.err().map(Failure::code).unwrap_or("reachable"),
    }
}

pub async fn inspect(options: &Options) -> Result<Report> {
    ensure!(
        options.profile != Profile::Control || options.controller.is_some(),
        "The control profile requires --controller HTTPS_ORIGIN"
    );
    ensure!(
        !options.require_quic || options.profile == Profile::Streaming,
        "--require-quic requires --profile streaming"
    );
    let origin = options
        .controller
        .as_deref()
        .map(|v| crate::native_node::controller_origin(v, false))
        .transpose()?;
    let mut pending = FuturesUnordered::new();
    if let Some(origin) = origin {
        pending.push(check("controller-https", true, "Verified HTTPS and Enginebox /health identity; no enrollment performed", "Check DNS, HTTPS access, controller URL and certificate trust. A captive portal or redirect is not controller success.", Probe::Controller(origin)));
    }
    if options.profile == Profile::Streaming {
        pending.push(check("moq-quic-transport", options.require_quic, "Verified QUIC/TLS with h3 ALPN to the media relay; no authenticated MoQ session", "This path needs outbound UDP/443 and replies. Compare VPN/network routes; a working HTTPS page or Tunnel does not prove QUIC. No VPN or firewall settings were changed.", Probe::Quic("draft-16.cloudflare.mediaoverquic.com",443)));
        pending.push(check("turn-tls-transport", false, "Verified TLS to TURN over TCP/443; no TURN allocation or media tested", "Check TCP/443 and certificate trust. Passing this check alone does not enable or qualify a WebRTC fallback.", Probe::Tls("turn.cloudflare.com",443)));
        pending.push(check("stun-udp-round-trip", false, "STUN binding response with matching transaction on UDP/3478; no peer or TURN session", "Check outbound UDP/3478 and replies. STUN success does not prove UDP/443, direct peer connectivity, or MoQ.", Probe::Stun("stun.cloudflare.com",3478)));
    }
    if options.with_tunnel {
        for (id, host) in [
            ("tunnel-region1-tcp", "region1.v2.argotunnel.com"),
            ("tunnel-region2-tcp", "region2.v2.argotunnel.com"),
        ] {
            pending.push(check(id, false, "TCP/7844 connectivity only; no tunnel registration or authorization", "Tunnel requires its own outbound path. Use cloudflared tunnel diag (2026.5.2+) and verify the running connector; HTTP/2 tunnel fallback does not carry this MoQ socket.", Probe::Tcp(host,7844)));
        }
    }
    let mut checks = Vec::new();
    while let Some(result) = pending.next().await {
        checks.push(result);
    }
    checks.sort_by_key(|c| c.id);
    Ok(Report {
        schema: 1,
        checked_at_unix_ms: SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64,
        profile: options.profile,
        required_checks_passed: checks
            .iter()
            .all(|c| !c.required || c.status == Status::Passed),
        has_warnings: checks
            .iter()
            .any(|c| !c.required && c.status == Status::Failed),
        streaming_qualified: false,
        media_session: "not-tested",
        turn_allocation: "not-tested",
        tunnel_authorization: "not-tested",
        network_settings_changed: false,
        checks,
    })
}

pub async fn print(options: &Options) -> Result<()> {
    let report = inspect(options).await?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    ensure!(
        report.required_checks_passed,
        "Required network checks failed; see the diagnostic codes and hints above"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn certificate(alpn: &[&[u8]]) -> (rustls::ServerConfig, rustls::RootCertStore) {
        let issued = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let certificate = issued.cert.der().clone();
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(issued.signing_key.serialize_der());
        let mut roots = rustls::RootCertStore::empty();
        roots.add(certificate.clone()).unwrap();
        let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![certificate], key.into())
        .unwrap();
        config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
        (config, roots)
    }

    #[tokio::test]
    async fn real_quic_requires_certificate_trust_and_matching_alpn() {
        let (config, trusted) = certificate(&[b"h3"]);
        let config = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(config).unwrap(),
        ));
        let endpoint = quinn::Endpoint::server(config, "127.0.0.1:0".parse().unwrap()).unwrap();
        let address = endpoint.local_addr().unwrap();
        let server = tokio::spawn(async move {
            while let Some(incoming) = endpoint.accept().await {
                if let Ok(connection) = incoming.await {
                    connection.closed().await;
                }
            }
        });
        let valid = tokio::time::timeout(
            Duration::from_secs(3),
            quic(
                address,
                "localhost",
                Arc::new(tls_config(&[b"h3"], trusted.clone())),
            ),
        )
        .await
        .unwrap();
        assert_eq!(valid, Ok(()));
        let untrusted = tokio::time::timeout(
            Duration::from_secs(3),
            quic(
                address,
                "localhost",
                Arc::new(tls_config(&[b"h3"], rustls::RootCertStore::empty())),
            ),
        )
        .await
        .unwrap();
        assert_eq!(untrusted, Err(Failure::Quic));
        let wrong_protocol = tokio::time::timeout(
            Duration::from_secs(3),
            quic(
                address,
                "localhost",
                Arc::new(tls_config(&[b"not-h3"], trusted)),
            ),
        )
        .await
        .unwrap();
        assert_eq!(wrong_protocol, Err(Failure::Quic));
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn dropped_quic_replies_fail_with_bounded_timeout() {
        // Reproduces the VPN symptom: UDP sends succeed, no handshake comes back.
        let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let result = check(
            "owned-blackhole",
            true,
            "fixture",
            "fixture",
            Probe::Quic("127.0.0.1", silent.local_addr().unwrap().port()),
        )
        .await;
        assert_eq!(result.status, Status::Failed);
        assert_eq!(result.code, "timeout");
        assert!(result.elapsed_ms >= 7900 && result.elapsed_ms < 12000);
        let mut bytes = [0u8; 1500];
        assert!(silent.try_recv(&mut bytes).unwrap() >= 1200);
    }

    #[tokio::test]
    async fn tls_verifies_peer_identity() {
        let (config, trusted) = certificate(&[]);
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let _ = acceptor.accept(stream).await;
            }
        });
        assert_eq!(
            tls(
                address,
                "localhost",
                Arc::new(tls_config(&[], trusted.clone()))
            )
            .await,
            Ok(())
        );
        assert_eq!(
            tls(address, "wrong.example", Arc::new(tls_config(&[], trusted))).await,
            Err(Failure::Tls)
        );
        assert_eq!(
            tls(
                address,
                "localhost",
                Arc::new(tls_config(&[], rustls::RootCertStore::empty()))
            )
            .await,
            Err(Failure::Tls)
        );
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn stun_requires_correlated_binding_response() {
        for corrupt in [false, true] {
            let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let address = socket.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let mut bytes = [0u8; 128];
                let (size, peer) = socket.recv_from(&mut bytes).await.unwrap();
                assert_eq!(size, 20);
                bytes[0] = 1;
                bytes[1] = 1;
                bytes[3] = 12;
                if corrupt {
                    bytes[8] ^= 1;
                }
                bytes[20..32]
                    .copy_from_slice(&[0, 0x20, 0, 8, 0, 1, 0x21, 0x12, 0x5e, 0x12, 0xa4, 0x43]);
                socket.send_to(&bytes[..32], peer).await.unwrap();
            });
            assert_eq!(
                stun(address).await,
                if corrupt {
                    Err(Failure::Response)
                } else {
                    Ok(())
                }
            );
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn controller_requires_identity_and_never_follows_redirects() {
        for (status, body, expected) in [
            (
                200,
                r#"{"ok":true,"service":"enginebox-fleet","schema":1}"#,
                Ok(()),
            ),
            (200, r#"{"ok":true}"#, Err(Failure::Response)),
            (
                200,
                "<html>sign in to this network</html>",
                Err(Failure::Response),
            ),
            (302, "", Err(Failure::Response)),
        ] {
            let app = axum::Router::new().route(
                "/health",
                axum::routing::get(move || async move {
                    (
                        axum::http::StatusCode::from_u16(status).unwrap(),
                        [("location", "http://127.0.0.1:1/never-follow")],
                        body,
                    )
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url =
                reqwest::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            // HTTP is only supplied to this private helper in the fixture. Public
            // inspect() validates a bare HTTPS origin before any network activity.
            assert_eq!(controller(&url).await, expected);
            server.abort();
            let _ = server.await;
        }
    }

    #[tokio::test]
    async fn invalid_configuration_is_rejected_before_any_probe() {
        for controller in [
            "http://127.0.0.1",
            "https://owner:secret@example.com",
            "https://example.com/token",
            "https://example.com/?token=secret",
        ] {
            let options = Options {
                controller: Some(controller.into()),
                profile: Profile::Control,
                require_quic: false,
                with_tunnel: false,
            };
            let error = inspect(&options).await.unwrap_err().to_string();
            assert!(!error.contains("secret"));
        }
        assert!(
            inspect(&Options {
                controller: None,
                profile: Profile::Control,
                require_quic: false,
                with_tunnel: false
            })
            .await
            .is_err()
        );
        assert!(
            inspect(&Options {
                controller: Some("https://example.com".into()),
                profile: Profile::Control,
                require_quic: true,
                with_tunnel: false
            })
            .await
            .is_err()
        );
    }
}
