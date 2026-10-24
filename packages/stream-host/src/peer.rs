use crate::{
    controller::{IceServer, Session},
    input::{Input, Permit},
};
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
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
                ..Default::default()
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
        let accepted = Arc::new(AtomicBool::new(false));
        pc.on_data_channel(Box::new(move |channel| {
            let (p, i, accepted) = (p.clone(), i.clone(), accepted.clone());
            Box::pin(async move {
                if channel.label() != "input" || !p.control || accepted.swap(true, Ordering::SeqCst)
                {
                    let _ = channel.close().await;
                    return;
                }
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
            tasks: vec![rtcp, send],
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
