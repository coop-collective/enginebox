//! The WebRTC fallback of an engine session (docs/ENGINE-SESSIONS.md,
//! "WebRTC fallback"): a viewer that cannot reach the MoQ relay (UDP 443
//! blocked, as some VPNs do) asks for a peer, sends one complete offer, and
//! reads back one complete answer from the session's stream host. Media is
//! relayed through Cloudflare TURN on both sides; the engine machine still
//! opens no port.
//!
//! Peers live in memory only: a controller restart ends them and viewers ask
//! again. A peer ends when its lease (renewed by the viewer) or its TURN
//! credentials run out, when its session stops being `ready` or moves to
//! another generation, or when either side ends it. The viewer holds a
//! `client_token` for its one peer; the stream host holds the session's host
//! token, which the node received with the session's other secrets. Only
//! SHA-256 digests of either are stored.
use super::{EngineSessions, Platform, Release, Session, State, equal, random_hex, refuse};
use anyhow::{Result, ensure};
use serde_json::{Value, json};

/// A peer lives this long past its last renewal, capped at its ICE expiry.
pub const PEER_LEASE_MS: u64 = 60_000;
/// TURN credentials (and so a peer's longest life).
pub const ICE_TTL_SECONDS: u64 = 3600;
/// Open peers per session (the stream host's own bound) and overall.
pub const MAX_PEERS_PER_SESSION: usize = 8;
pub const MAX_PEERS: usize = 64;
pub const MAX_SDP: usize = 64 * 1024;
/// Ended peers are forgotten after this long.
const RETAIN_MS: u64 = 10 * 60_000;
pub const NOT_RUNNING: &str = "The live editor is not running right now.";
pub const NOT_CONFIGURED: &str = "WebRTC fallback is not configured.";
pub const ENDED: &str = "This stream connection has ended.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Watch,
    Control,
}
impl Mode {
    fn label(self) -> &'static str {
        match self {
            Mode::Watch => "watch",
            Mode::Control => "control",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Peer {
    id: String,
    session: String,
    project: String,
    generation: u64,
    mode: Mode,
    client_hash: String,
    ice: Vec<Value>,
    ice_expires: u64,
    expires: u64,
    offer: Option<String>,
    /// The host's answer and the operation id it was sent with.
    answer: Option<(String, String)>,
    ended_at: Option<u64>,
}
impl Peer {
    fn open(&self) -> bool {
        self.ended_at.is_none()
    }
    fn view(&self) -> Value {
        json!({
            "id": self.id, "mode": self.mode.label(),
            "state": if self.open() { "open" } else { "ended" },
            "expires_at": self.expires,
        })
    }
    /// The stream host's view, shaped like its machine-session `Session`.
    fn host_view(&self) -> Value {
        json!({
            "id": self.id, "project": self.project, "nodeId": self.session,
            "nodeEpoch": self.generation, "mode": self.mode.label(),
            "state": if self.open() { "open" } else { "ended" },
            "expires": self.expires, "iceServers": self.ice,
            "iceTransportPolicy": "relay", "iceExpires": self.ice_expires,
        })
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
pub fn digest(secret: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(secret.as_bytes()))
}
/// A new per-session host token (`ehp_<64 hex>`).
pub fn host_token() -> String {
    format!("ehp_{}", random_hex(32))
}
fn valid_peer_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn string<'a>(body: &'a Value, key: &str) -> Result<&'a str> {
    body[key]
        .as_str()
        .ok_or_else(|| refuse(400, format!("Send {key}.")))
}

impl EngineSessions {
    /// Ends peers whose session left `ready`, changed generation or is gone,
    /// and peers past their lease; forgets long-ended ones.
    fn sweep_peers(&self, sessions: &std::collections::BTreeMap<String, Session>, at: u64) {
        let mut peers = self.peers.lock().unwrap();
        for p in peers.values_mut().filter(|p| p.open()) {
            let live = sessions
                .get(&p.session)
                .is_some_and(|s| s.state == State::Ready && s.generation == p.generation);
            if !live || p.expires <= at {
                p.ended_at = Some(at);
            }
        }
        peers.retain(|_, p| p.ended_at.is_none_or(|e| at.saturating_sub(e) < RETAIN_MS));
    }

    /// `POST …/{id}/peers {mode}`: a new WebRTC peer of a ready session.
    /// Watching is open to whoever may ask for a watch token; control only
    /// to the starter (or the owner token), and it replaces their previous
    /// control peer.
    pub fn peer_create(
        &self,
        org: &str,
        project: &str,
        actor: &str,
        id: &str,
        body: &Value,
        platform: &dyn Platform,
    ) -> Result<(u16, Value)> {
        let _release = Release(self, platform);
        crate::keys(body, &["mode"], &["mode"]).map_err(|_| refuse(400, "Send {mode}."))?;
        let mode = match body["mode"].as_str() {
            Some("watch") => Mode::Watch,
            Some("control") => Mode::Control,
            _ => return Err(refuse(400, "mode must be watch or control.")),
        };
        let s = self.ready_session(org, project, actor, id, platform)?;
        ensure!(
            s.mode == super::Mode::Editor,
            refuse(409, super::WORLD_HAS_NO_STREAM)
        );
        ensure!(
            mode == Mode::Watch || actor == "owner" || actor == s.started_by,
            refuse(
                403,
                "Only the member who started the session can control it."
            )
        );
        let issuer = platform
            .turn()?
            .ok_or_else(|| refuse(503, NOT_CONFIGURED))?;
        // Network call: never under a lock.
        let ice = issuer
            .ice_servers(ICE_TTL_SECONDS)
            .map_err(|error| refuse(502, format!("{error}")))?;
        let at = now_ms();
        let ice_expires = at + ICE_TTL_SECONDS * 1000;
        let client_token = format!("epc_{}", random_hex(32));
        let peer = Peer {
            id: random_hex(16),
            session: s.id.clone(),
            project: s.project.clone(),
            generation: s.generation,
            mode,
            client_hash: digest(&client_token),
            ice: ice.clone(),
            ice_expires,
            expires: (at + PEER_LEASE_MS).min(ice_expires),
            offer: None,
            answer: None,
            ended_at: None,
        };
        {
            let sessions = self.sessions.lock().unwrap();
            self.sweep_peers(&sessions, at);
        }
        let mut peers = self.peers.lock().unwrap();
        if mode == Mode::Control {
            for p in peers
                .values_mut()
                .filter(|p| p.open() && p.session == s.id && p.mode == Mode::Control)
            {
                p.ended_at = Some(at);
            }
        }
        let open = peers.values().filter(|p| p.open());
        ensure!(
            open.clone().filter(|p| p.session == s.id).count() < MAX_PEERS_PER_SESSION
                && open.count() < MAX_PEERS,
            refuse(
                429,
                "Too many viewers are connected over the fallback stream right now."
            )
        );
        let view = peer.view();
        peers.insert(peer.id.clone(), peer);
        drop(peers);
        platform.audit(
            org,
            actor,
            "engine.peer_open",
            &s.id,
            true,
            json!({"mode": mode.label()}),
        );
        Ok((
            201,
            json!({
                "peer": view, "client_token": client_token, "ice_servers": ice,
                "ice_transport_policy": "relay", "ice_expires_at": ice_expires,
                "width": s.width, "height": s.height,
            }),
        ))
    }

    fn ready_session(
        &self,
        org: &str,
        project: &str,
        actor: &str,
        id: &str,
        platform: &dyn Platform,
    ) -> Result<Session> {
        let mut sessions = self.sessions.lock().unwrap();
        self.tick(&mut sessions, platform);
        let s = sessions
            .get(id)
            .filter(|s| super::valid_session_id(id) && Self::visible(s, org, Some(project), actor))
            .ok_or_else(|| refuse(404, "Unknown engine session."))?
            .clone();
        ensure!(s.state == State::Ready, refuse(409, NOT_RUNNING));
        Ok(s)
    }

    /// `POST …/{id}/peers/{pid}/{offer|poll|renew|end} {client_token, sdp?}`.
    #[allow(clippy::too_many_arguments)]
    pub fn peer_client(
        &self,
        org: &str,
        project: &str,
        actor: &str,
        id: &str,
        pid: &str,
        action: &str,
        body: &Value,
        platform: &dyn Platform,
    ) -> Result<(u16, Value)> {
        let _release = Release(self, platform);
        let allowed: &[&str] = if action == "offer" {
            &["client_token", "sdp"]
        } else {
            &["client_token"]
        };
        crate::keys(body, allowed, allowed)
            .map_err(|_| refuse(400, format!("Send {{{}}}.", allowed.join(", "))))?;
        let token = string(body, "client_token")?;
        ensure!(
            valid_peer_id(pid),
            refuse(404, "Unknown stream connection.")
        );
        let at = now_ms();
        {
            let mut sessions = self.sessions.lock().unwrap();
            self.tick(&mut sessions, platform);
            ensure!(
                sessions
                    .get(id)
                    .is_some_and(|s| Self::visible(s, org, Some(project), actor)),
                refuse(404, "Unknown engine session.")
            );
            self.sweep_peers(&sessions, at);
        }
        let mut peers = self.peers.lock().unwrap();
        let p = peers
            .get_mut(pid)
            .filter(|p| p.session == id)
            .ok_or_else(|| refuse(404, "Unknown stream connection."))?;
        ensure!(
            equal(&digest(token), &p.client_hash),
            refuse(403, "This stream connection belongs to someone else.")
        );
        ensure!(p.open(), refuse(410, ENDED));
        match action {
            "offer" => {
                let sdp = string(body, "sdp")?;
                ensure!(
                    !sdp.is_empty() && sdp.len() <= MAX_SDP,
                    refuse(400, "The offer is empty or too large.")
                );
                ensure!(
                    p.offer.is_none(),
                    refuse(409, "This stream connection already has an offer.")
                );
                p.offer = Some(sdp.to_owned());
                Ok((200, json!({"peer": p.view()})))
            }
            "poll" => Ok((
                200,
                json!({"peer": p.view(), "answer": p.answer.as_ref().map(|(sdp, _)| sdp)}),
            )),
            "renew" => {
                p.expires = (at + PEER_LEASE_MS).min(p.ice_expires);
                Ok((200, json!({"peer": p.view()})))
            }
            "end" => {
                p.ended_at = Some(at);
                Ok((200, json!({"peer": p.view()})))
            }
            _ => Err(refuse(404, "Unknown stream connection operation.")),
        }
    }

    /// The ready session a host token belongs to.
    fn host_session(&self, token: &str, platform: &dyn Platform) -> Result<Session> {
        ensure!(
            token.len() == 68 && token.starts_with("ehp_"),
            refuse(401, "Stream host credential required.")
        );
        let hash = digest(token);
        let mut sessions = self.sessions.lock().unwrap();
        self.tick(&mut sessions, platform);
        let s = sessions
            .values()
            .find(|s| s.peer_host_hash.as_deref().is_some_and(|h| equal(h, &hash)))
            .ok_or_else(|| refuse(401, "Stream host credential expired or revoked."))?
            .clone();
        ensure!(
            s.state == State::Ready,
            refuse(410, "The engine session is not streaming.")
        );
        self.sweep_peers(&sessions, now_ms());
        Ok(s)
    }

    /// `POST /v1/node/engine-peers/poll {}` from the session's stream host.
    pub fn peer_host_poll(&self, token: &str, platform: &dyn Platform) -> Result<Value> {
        let s = self.host_session(token, platform)?;
        let peers = self.peers.lock().unwrap();
        let rows: Vec<Value> = peers
            .values()
            .filter(|p| p.open() && p.session == s.id && p.generation == s.generation)
            .map(Peer::host_view)
            .collect();
        Ok(json!({"now": now_ms(), "sessions": rows}))
    }

    /// The host's side of one peer: `GET …/{pid}/signals?after=N`,
    /// `POST …/{pid}/signals {id, signal:{type:"answer", sdp}}` and
    /// `POST …/{pid}/end {id}`.
    pub fn peer_host(
        &self,
        token: &str,
        pid: &str,
        action: &str,
        after: Option<u64>,
        body: &Value,
        platform: &dyn Platform,
    ) -> Result<Value> {
        let s = self.host_session(token, platform)?;
        ensure!(valid_peer_id(pid), refuse(410, ENDED));
        let mut peers = self.peers.lock().unwrap();
        let p = peers
            .get_mut(pid)
            .filter(|p| p.session == s.id && p.generation == s.generation)
            .ok_or_else(|| refuse(410, ENDED))?;
        ensure!(p.open(), refuse(410, ENDED));
        match (action, after) {
            ("signals", Some(after)) => {
                let signals: Vec<Value> = match &p.offer {
                    Some(sdp) if after < 1 => {
                        vec![json!({"sequence": 1, "signal": {"type": "offer", "sdp": sdp}})]
                    }
                    _ => vec![],
                };
                let cursor = u64::from(p.offer.is_some()) + u64::from(p.answer.is_some());
                Ok(json!({"signals": signals, "cursor": cursor, "session": p.host_view()}))
            }
            ("signals", None) => {
                crate::keys(body, &["id", "signal"], &["id", "signal"])
                    .map_err(|_| refuse(400, "Send {id, signal}."))?;
                crate::keys(&body["signal"], &["type", "sdp"], &["type", "sdp"])
                    .map_err(|_| refuse(400, "Send an answer."))?;
                let op = string(body, "id")?;
                let sdp = string(&body["signal"], "sdp")?;
                ensure!(
                    body["signal"]["type"] == "answer"
                        && !op.is_empty()
                        && op.len() <= 128
                        && !sdp.is_empty()
                        && sdp.len() <= MAX_SDP,
                    refuse(400, "Send one answer.")
                );
                // Refusals here are about this peer only (410): the stream
                // host drops the peer and keeps serving the others. A host
                // restarted mid-session sees peers its previous process
                // already answered; their viewers reconnect with new peers.
                ensure!(p.offer.is_some(), refuse(410, ENDED));
                match &p.answer {
                    Some((sent, sent_op)) if sent_op == op && sent == sdp => {}
                    Some(_) => {
                        p.ended_at = Some(now_ms());
                        return Err(refuse(410, ENDED));
                    }
                    None => p.answer = Some((sdp.to_owned(), op.to_owned())),
                }
                Ok(json!({"sequence": 2, "session": p.host_view()}))
            }
            ("end", None) => {
                p.ended_at = Some(now_ms());
                Ok(json!({"session": p.host_view()}))
            }
            _ => Err(refuse(404, "Unknown stream host operation.")),
        }
    }
}
