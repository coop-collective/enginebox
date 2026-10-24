use crate::{
    controller::{IceServer, Session},
    input::{Input, Permit},
};
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{sync::broadcast, task::JoinHandle};
use webrtc::{
    api::{
        API, APIBuilder,
        interceptor_registry::register_default_interceptors,
        media_engine::{MIME_TYPE_VP8, MediaEngine},
        setting_engine::{SctpMaxMessageSize, SettingEngine},
    },
    ice_transport::{ice_candidate::RTCIceCandidateInit, ice_server::RTCIceServer},
    interceptor::registry::Registry,
    media::Sample,
    peer_connection::{
        RTCPeerConnection, configuration::RTCConfiguration,
        peer_connection_state::RTCPeerConnectionState,
        policy::ice_transport_policy::RTCIceTransportPolicy,
        sdp::session_description::RTCSessionDescription,
    },
    rtp_transceiver::rtp_codec::RTCRtpCodecCapability,
    track::track_local::{TrackLocal, track_local_static_sample::TrackLocalStaticSample},
};

pub struct Peer {
    pub session: Session,
    pub permit: Arc<Permit>,
    pub pc: Arc<RTCPeerConnection>,
    pub cursor: u64,
    pub answer: Option<String>,
    pub answer_id: String,
    negotiation: Option<JoinHandle<Result<String>>>,
    offered: bool,
    tasks: Vec<JoinHandle<()>>,
    input: Arc<Mutex<Input>>,
}
pub fn api() -> Result<API> {
    let mut media = MediaEngine::default();
    media.register_default_codecs()?;
    let registry = register_default_interceptors(Registry::new(), &mut media)?;
    let mut settings = SettingEngine::default();
    settings.set_ice_timeouts(
        Some(Duration::from_secs(5)),
        Some(Duration::from_secs(5)),
        Some(Duration::from_secs(2)),
    );
    settings.set_sctp_max_message_size_can_send(SctpMaxMessageSize::Bounded(1024));
    Ok(APIBuilder::new()
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .with_setting_engine(settings)
        .build())
}
impl Peer {
    pub async fn start(
        api: &API,
        session: Session,
        permit: Arc<Permit>,
        ice: &[IceServer],
        policy: &str,
        input: Arc<Mutex<Input>>,
        mut frames: broadcast::Receiver<Bytes>,
    ) -> Result<Self> {
        if ice.len() > 8 || !matches!(policy, "all" | "relay") {
            bail!("invalid ICE policy")
        }
        let mut servers = Vec::new();
        for server in ice {
            if server.urls.is_empty()
                || server.urls.len() > 8
                || server.username.len() > 1024
                || server.credential.len() > 2048
                || server.urls.iter().any(|u| {
                    u.len() > 2048
                        || !matches!(
                            u.split(':').next(),
                            Some("stun" | "stuns" | "turn" | "turns")
                        )
                })
            {
                bail!("invalid ICE server")
            }
            servers.push(RTCIceServer {
                urls: server.urls.clone(),
                username: server.username.clone(),
                credential: server.credential.clone(),
            });
        }
        if policy == "relay"
            && !ice.iter().any(|s| {
                s.urls
                    .iter()
                    .any(|u| u.starts_with("turn:") || u.starts_with("turns:"))
            })
        {
            bail!("relay policy requires TURN")
        }
        let pc = Arc::new(
            api.new_peer_connection(RTCConfiguration {
                ice_servers: servers,
                ice_transport_policy: if policy == "relay" {
                    RTCIceTransportPolicy::Relay
                } else {
                    RTCIceTransportPolicy::All
                },
                ..Default::default()
            })
            .await?,
        );
        let active = permit.clone();
        pc.on_peer_connection_state_change(Box::new(move |state| {
            let p = active.clone();
            Box::pin(async move {
                if matches!(
                    state,
                    RTCPeerConnectionState::Failed
                        | RTCPeerConnectionState::Closed
                        | RTCPeerConnectionState::Disconnected
                ) {
                    p.revoke();
                }
            })
        }));
        let track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_VP8.into(),
                ..Default::default()
            },
            "video".into(),
            "enginebox-owned-display".into(),
        ));
        let sender = pc
            .add_track(track.clone() as Arc<dyn TrackLocal + Send + Sync>)
            .await?;
        let rtcp = tokio::spawn(async move {
            let mut b = [0; 1500];
            while sender.read(&mut b).await.is_ok() {}
        });
        let active = permit.clone();
        let send = tokio::spawn(async move {
            while active.valid() {
                match frames.recv().await {
                    Ok(data) => {
                        if !active.valid() {
                            break;
                        }
                        if !matches!(
                            tokio::time::timeout(
                                Duration::from_millis(250),
                                track.write_sample(&Sample {
                                    data,
                                    duration: Duration::from_nanos(1_000_000_000 / 30),
                                    ..Default::default()
                                }),
                            )
                            .await,
                            Ok(Ok(()))
                        ) {
                            active.revoke();
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        });
        let (p, i) = (permit.clone(), input.clone());
        // Independent of controller HTTP: expiry releases held keys even while
        // the main poll is waiting for a network timeout.
        let (watch_permit, watch_input, watch_pc) = (permit.clone(), input.clone(), pc.clone());
        let watchdog = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(50));
            loop {
                tick.tick().await;
                if !watch_permit.valid() {
                    watch_permit.revoke();
                    if watch_permit.control {
                        watch_input.lock().unwrap().release_all();
                    }
                    let _ = tokio::time::timeout(Duration::from_secs(2), watch_pc.close()).await;
                    break;
                }
            }
        });
        let accepted = Arc::new(AtomicBool::new(false));
        let channel_count = Arc::new(AtomicUsize::new(0));
        pc.on_data_channel(Box::new(move |channel| {
            let (p, i, accepted, channel_count) = (
                p.clone(),
                i.clone(),
                accepted.clone(),
                channel_count.clone(),
            );
            Box::pin(async move {
                if channel_count.fetch_add(1, Ordering::SeqCst) >= 4 {
                    p.revoke();
                }
                if channel.label() != "input"
                    || !p.control
                    || accepted.swap(true, Ordering::SeqCst)
                    || !p.valid()
                {
                    // on_data_channel precedes handle_open in webrtc-rs. Closing
                    // only here can miss the later SCTP stream; close on_open too.
                    let rejected = Arc::downgrade(&channel);
                    channel.on_open(Box::new(move || {
                        let rejected = rejected.clone();
                        Box::pin(async move {
                            if let Some(channel) = rejected.upgrade() {
                                let _ = channel.close().await;
                            }
                        })
                    }));
                    let _ = channel.close().await;
                    return;
                }
                let (close_permit, close_input) = (p.clone(), i.clone());
                channel.on_close(Box::new(move || {
                    let (p, i) = (close_permit.clone(), close_input.clone());
                    Box::pin(async move {
                        p.revoke();
                        i.lock().unwrap().release_all();
                    })
                }));
                channel.on_message(Box::new(move |message| {
                    let (p, i) = (p.clone(), i.clone());
                    Box::pin(async move {
                        if message.is_string && message.data.len() <= 1024 {
                            let _ = i.lock().unwrap().apply(&message.data, &p);
                        }
                    })
                }));
            })
        }));
        Ok(Self {
            answer_id: format!("answer-{}", uuid::Uuid::new_v4()),
            session,
            permit,
            pc,
            cursor: 0,
            answer: None,
            negotiation: None,
            offered: false,
            tasks: vec![rtcp, send, watchdog],
            input,
        })
    }
    pub async fn offer(&mut self, sdp: &str) -> Result<()> {
        if self.offered || sdp.len() > 65536 {
            bail!("duplicate or oversized offer")
        }
        self.pc
            .set_remote_description(RTCSessionDescription::offer(sdp.into())?)
            .await?;
        self.offered = true;
        let (pc, permit) = (self.pc.clone(), self.permit.clone());
        self.negotiation = Some(tokio::spawn(async move {
            let negotiation = async {
                let answer = pc.create_answer(None).await?;
                let mut gathered = pc.gathering_complete_promise().await;
                pc.set_local_description(answer).await?;
                gathered.recv().await;
                pc.local_description().await.context("missing local answer")
            };
            let answer = tokio::time::timeout(Duration::from_secs(15), negotiation)
                .await
                .context("ICE gathering timeout")??;
            if !permit.valid() {
                bail!("session expired during negotiation")
            }
            Ok(answer.sdp)
        }));
        Ok(())
    }
    pub async fn collect_answer(&mut self) -> Result<()> {
        if self.negotiation.as_ref().is_some_and(|t| t.is_finished()) {
            self.answer = Some(self.negotiation.take().unwrap().await??);
        }
        Ok(())
    }
    pub async fn ice(&self, value: serde_json::Value) -> Result<()> {
        if value.is_null() {
            return Ok(());
        }
        if value.to_string().len() > 4096 {
            bail!("oversized ICE candidate")
        }
        let candidate: RTCIceCandidateInit = serde_json::from_value(value)?;
        self.pc.add_ice_candidate(candidate).await?;
        Ok(())
    }
    pub async fn stop(mut self) {
        self.permit.revoke();
        if let Some(t) = self.negotiation.take() {
            t.abort();
        }
        if self.permit.control {
            self.input.lock().unwrap().release_all();
        }
        for t in self.tasks.drain(..) {
            t.abort();
        }
        let _ = tokio::time::timeout(Duration::from_secs(2), self.pc.close()).await;
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.permit.revoke();
        if let Some(t) = &self.negotiation {
            t.abort();
        }
        for t in &self.tasks {
            t.abort();
        }
        if self.permit.control {
            self.input.lock().unwrap().release_all();
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "requires the owned Linux test-runtime image with Xvfb/native fixture"]
    async fn lease_watchdog_releases_held_key_while_controller_http_is_blocked() -> Result<()> {
        use crate::{capture::Capture, config::Config};
        use std::sync::atomic::AtomicU64;
        let config: Config = serde_json::from_value(
            serde_json::json!({"controller":"https://fixture.example","project":"fixture","width":640,"height":360,"command":{"executable":"/usr/local/bin/enginebox-test-window","directory":"/app"}}),
        )?;
        let capture = Capture::start(
            &config,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
        )
        .await?;
        let session: Session = serde_json::from_value(
            serde_json::json!({"id":"lease-test","project":"fixture","nodeId":"native-fixture","nodeEpoch":1,"mode":"control","state":"open","expires":60000}),
        )?;
        let permit = Permit::new(true, Duration::from_secs(5));
        let peer = Peer::start(
            &api()?,
            session,
            permit.clone(),
            &[],
            "all",
            capture.input.clone(),
            capture.frames.subscribe(),
        )
        .await?;
        permit.renew(Duration::from_millis(250));
        capture
            .input
            .lock()
            .unwrap()
            .apply(br#"{"type":"keydown","key":"KeyA"}"#, &permit)?;
        assert!(capture.input.lock().unwrap().server_has_pressed_keys()?);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let blocked = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(3)).await;
        });
        let response = reqwest::Client::new()
            .get(format!("http://{address}/poll"))
            .timeout(Duration::from_secs(1))
            .send()
            .await;
        assert!(response.is_err());
        assert!(!permit.valid());
        assert!(
            !capture.input.lock().unwrap().server_has_pressed_keys()?,
            "independent deadline must release the native key during HTTP await"
        );
        peer.stop().await;
        capture.stop().await;
        blocked.abort();
        Ok(())
    }
}
