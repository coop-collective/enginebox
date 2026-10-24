mod capture;
mod config;
mod controller;
mod input;
mod peer;
use anyhow::{Context, Result, bail};
use config::{Config, Identity};
use controller::{AuthorityLost, Controller, Poll, SessionGone};
use std::{
    collections::HashMap,
    io::Read,
    os::fd::AsRawFd,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

#[tokio::main]
async fn main() -> Result<()> {
    let raw: Vec<_> = std::env::args().skip(1).collect();
    if raw.as_slice() == ["--version"] {
        println!("enginebox-stream-host {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let enroll_only = raw.iter().any(|v| v == "--enroll-only");
    let mut args = raw.into_iter().filter(|v| v != "--enroll-only");
    let config_path = PathBuf::from(
        args.next()
            .unwrap_or_else(|| "/etc/enginebox/stream-host.json".into()),
    );
    let state = PathBuf::from(
        args.next()
            .unwrap_or_else(|| "/var/lib/enginebox-stream".into()),
    );
    if args.next().is_some() {
        bail!(
            "usage: enginebox-stream-host [config.json] [private-state-directory]; first enrollment reads invitation from stdin"
        )
    }
    let config = Config::load(&config_path)?;
    let mut identity = Identity::load(&state, &config)?;
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(state.join("process.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("another stream host owns this identity volume")
    }
    if identity.as_ref().is_none_or(|i| !i.enrolled) {
        eprintln!(
            "Enrollment required. Read the short-lived invitation from stdin; it will not be stored."
        );
        let mut invitation = String::new();
        std::io::stdin()
            .take(4097)
            .read_to_string(&mut invitation)?;
        if invitation.len() > 4096 {
            bail!("invitation exceeds bound")
        }
        let invitation = if invitation.trim_start().starts_with('{') {
            let v: serde_json::Value = serde_json::from_str(&invitation)?;
            if v["controller"].as_str() != Some(&config.controller)
                || v["project"].as_str() != Some(&config.project)
            {
                bail!("invitation scope does not match local config")
            }
            v["invitation"]
                .as_str()
                .context("missing invitation")?
                .to_string()
        } else {
            invitation.trim().to_string()
        };
        let mut id = match identity.take() {
            Some(id) => {
                let expected = Identity::create(&config, &invitation)?;
                if id.token.split('.').next() != expected.token.split('.').next() {
                    bail!("invitation belongs to another node")
                }
                id
            }
            None => Identity::create(&config, &invitation)?,
        };
        id.save(&state)?;
        Controller::new(config.controller.clone(), invitation)?
            .enroll(&id.hash())
            .await?;
        id.enrolled = true;
        id.save(&state)?;
        identity = Some(id);
    }
    let identity = identity.context("missing identity")?;
    if enroll_only {
        eprintln!(
            "Enrollment saved. Start the host with the same config and private state volume."
        );
        return Ok(());
    }
    let node = identity
        .token
        .split('.')
        .next()
        .context("invalid identity")?
        .to_string();
    let mut controller = Controller::new(config.controller.clone(), identity.token)?;
    let epoch = controller.hello().await?;
    eprintln!(
        "Stream host enrolled; project={}, epoch={epoch}; waiting for authorized sessions",
        config.project
    );
    run(config, controller, node, epoch).await
}
async fn run(config: Config, controller: Controller, node: String, epoch: u64) -> Result<()> {
    let api = peer::api()?;
    let frames = Arc::new(AtomicU64::new(0));
    let inputs = Arc::new(AtomicU64::new(0));
    let mut capture: Option<capture::Capture> = None;
    let mut peers: HashMap<String, peer::Peer> = HashMap::new();
    let mut poll = tokio::time::interval(Duration::from_secs(2));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut watchdog = tokio::time::interval(Duration::from_millis(100));
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let result = loop {
        let update: Result<()> = tokio::select! {
            _=tokio::signal::ctrl_c()=>break Ok(()),_=term.recv()=>break Ok(()),
            _=watchdog.tick()=>{let bad=capture.as_mut().is_some_and(|c|!c.healthy());let dead:Vec<_>=peers.iter().filter(|(_,p)|bad||!p.permit.valid()).map(|(id,_)|id.clone()).collect();for id in dead{if let Some(p)=peers.remove(&id){let session=p.session.clone();p.stop().await;let _=controller.end(&session).await;}}Ok(())},
            _=heartbeat.tick()=>controller.heartbeat(peers.len(),frames.load(Ordering::Relaxed),inputs.load(Ordering::Relaxed)).await,
            _=poll.tick()=>{match controller.poll().await {Ok(update)=>reconcile(&config,&controller,&node,epoch,&api,update,&mut capture,&mut peers,frames.clone(),inputs.clone()).await,Err(e)=>Err(e)}},
        };
        if let Err(error) = update {
            for (_, p) in peers.drain() {
                p.stop().await;
            }
            if let Some(c) = capture.take() {
                c.stop().await;
            }
            if error.downcast_ref::<AuthorityLost>().is_some() {
                break Err(error);
            }
            eprintln!("Stream authority unavailable; streams stopped: {error}");
        }
        if peers.is_empty() {
            if let Some(c) = capture.take() {
                c.stop().await;
            }
        }
    };
    for (_, p) in peers.drain() {
        p.stop().await;
    }
    if let Some(c) = capture.take() {
        c.stop().await;
    }
    result
}
#[allow(clippy::too_many_arguments)]
async fn reconcile(
    config: &Config,
    controller: &Controller,
    node: &str,
    epoch: u64,
    api: &webrtc::api::API,
    update: Poll,
    capture: &mut Option<capture::Capture>,
    peers: &mut HashMap<String, peer::Peer>,
    frames: Arc<AtomicU64>,
    inputs: Arc<AtomicU64>,
) -> Result<()> {
    if update.sessions.len() > 8
        || update.sessions.iter().any(|s| {
            s.project != config.project
                || s.node_id != node
                || s.node_epoch != epoch
                || s.state != "open"
                || !matches!(s.mode.as_str(), "watch" | "control")
        })
    {
        bail!("invalid authority session envelope")
    }
    let received = std::time::Instant::now();
    let active: std::collections::HashSet<_> = update
        .sessions
        .iter()
        .filter(|s| s.expires > update.now)
        .map(|s| s.id.clone())
        .collect();
    let removed: Vec<_> = peers
        .keys()
        .filter(|id| !active.contains(*id))
        .cloned()
        .collect();
    for id in removed {
        if let Some(p) = peers.remove(&id) {
            p.stop().await;
        }
    }
    for session in update.sessions {
        if session
            .ice_expires
            .is_some_and(|expiry| expiry <= update.now)
        {
            bail!("TURN credentials expired")
        }
        let ttl = Duration::from_millis(
            session
                .expires
                .min(session.ice_expires.unwrap_or(u64::MAX))
                .saturating_sub(update.now)
                .min(60_000),
        )
        .saturating_sub(received.elapsed());
        if ttl.is_zero() {
            continue;
        }
        if let Some(p) = peers.get(&session.id) {
            if p.session.mode != session.mode {
                bail!("session permission changed without a new admission")
            }
            p.permit.renew(ttl);
        }
        if !peers.contains_key(&session.id) {
            if peers.len() >= config.max_peers {
                controller.end(&session).await?;
                continue;
            }
            if capture.is_none() {
                *capture =
                    Some(capture::Capture::start(config, frames.clone(), inputs.clone()).await?);
            }
            let c = capture.as_ref().unwrap();
            let p = peer::Peer::start(
                api,
                session.clone(),
                input::Permit::new(session.mode == "control", ttl),
                &session.ice_servers,
                session
                    .ice_transport_policy
                    .as_deref()
                    .context("missing ICE policy")?,
                c.input.clone(),
                c.frames.subscribe(),
            )
            .await?;
            peers.insert(session.id.clone(), p);
        }
        let result = exchange(
            controller,
            &session,
            peers.get_mut(&session.id).unwrap(),
            update.now,
        )
        .await;
        if let Err(error) = result {
            if error.downcast_ref::<SessionGone>().is_some()
                || error.downcast_ref::<PeerFault>().is_some()
            {
                if let Some(p) = peers.remove(&session.id) {
                    p.stop().await;
                }
                let _ = controller.end(&session).await;
                eprintln!("Stream peer closed: {error}");
            } else {
                return Err(error);
            }
        }
    }
    Ok(())
}
#[derive(Debug)]
struct PeerFault;
impl std::fmt::Display for PeerFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid or failed peer negotiation")
    }
}
impl std::error::Error for PeerFault {}
async fn exchange(
    controller: &Controller,
    session: &controller::Session,
    p: &mut peer::Peer,
    now: u64,
) -> Result<()> {
    let signals = controller.signals(session, p.cursor).await?;
    if signals.signals.len() > 130
        || signals.session.id != session.id
        || signals.session.expires <= now
        || signals.session.state != "open"
    {
        bail!("invalid session signal envelope")
    }
    for signal in signals.signals {
        if signal.sequence <= p.cursor {
            continue;
        }
        match signal.signal["type"].as_str() {
            Some("offer") => p
                .offer(signal.signal["sdp"].as_str().ok_or(PeerFault)?)
                .await
                .map_err(|_| PeerFault)?,
            Some("ice") => p
                .ice(signal.signal["candidate"].clone())
                .await
                .map_err(|_| PeerFault)?,
            _ => return Err(PeerFault.into()),
        };
        p.cursor = signal.sequence;
    }
    if signals.cursor < p.cursor {
        bail!("signal cursor moved backwards")
    }
    p.cursor = signals.cursor;
    p.collect_answer().await.map_err(|_| PeerFault)?;
    if let Some(answer) = &p.answer {
        controller.answer(session, &p.answer_id, answer).await?;
    }
    Ok(())
}
