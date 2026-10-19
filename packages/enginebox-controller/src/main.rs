use anyhow::{Context, Result, ensure};
use clap::Parser;
use std::{net::SocketAddr, path::PathBuf, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn lifeline() -> Result<()> {
    let Some(address) = std::env::var_os("ENGINEBOX_SUPERVISOR_LIFELINE") else {
        std::future::pending::<()>().await;
        return Ok(());
    };
    let address: SocketAddr = address
        .to_str()
        .context("Invalid supervisor lifeline")?
        .parse()?;
    ensure!(
        address.ip().is_loopback(),
        "Supervisor lifeline must use loopback"
    );
    let token = std::env::var("ENGINEBOX_SUPERVISOR_LIFELINE_TOKEN")?;
    ensure!(
        token.len() >= 32 && token.bytes().all(|b| (33..=126).contains(&b)),
        "Invalid supervisor lifeline token"
    );
    let mut socket = tokio::net::TcpStream::connect(address).await?;
    socket.write_all(format!("{token}\n").as_bytes()).await?;
    let mut data = [0; 1];
    while socket.read(&mut data).await? > 0 {}
    Ok(())
}

#[derive(Parser)]
#[command(
    version,
    about = "Engine Box native controller. Bind privately behind enginebox-supervisor for upgrades."
)]
struct Args {
    #[arg(long)]
    config: PathBuf,
    #[arg(long, default_value = "127.0.0.1:8765")]
    listen: SocketAddr,
    #[arg(long)]
    port: Option<u16>,
    #[arg(long, env = "ENGINEBOX_STATE_DIR")]
    state_dir: Option<PathBuf>,
}
#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let mut listen = args.listen;
    if let Some(port) = args.port {
        listen.set_port(port);
    }
    ensure!(
        listen.ip().is_loopback(),
        "The controller must bind loopback behind an authenticated TLS gateway"
    );
    let token = match std::env::var_os("ENGINEBOX_API_TOKEN_FILE") {
        Some(path) => std::fs::read_to_string(path)?.trim().to_owned(),
        None => std::env::var("ENGINEBOX_API_TOKEN")
            .context("Set ENGINEBOX_API_TOKEN_FILE or ENGINEBOX_API_TOKEN")?,
    };
    let root = args.state_dir.unwrap_or_else(|| {
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".local/state/enginebox")
    });
    let controller = enginebox_controller::Controller::new(&args.config, &root, token)?;
    if std::env::var("ENGINEBOX_START_STANDBY").unwrap_or_default() != "1" {
        let active = controller.clone();
        tokio::task::spawn_blocking(move || active.activate()).await??;
    }
    let background = controller.start_background();
    let listener = tokio::net::TcpListener::bind(listen).await?;
    eprintln!(
        "Engine Box controller {} listening on {}",
        env!("CARGO_PKG_VERSION"),
        listener.local_addr()?
    );
    let shutdown = controller.clone();
    axum::serve(listener,enginebox_controller::router(controller)).with_graceful_shutdown(async move {
        #[cfg(unix)] {let mut terminate=tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");tokio::select!{_ = tokio::signal::ctrl_c()=>{},_ = terminate.recv()=>{},_ = lifeline()=>{}}}
        #[cfg(not(unix))] {tokio::select!{_ = tokio::signal::ctrl_c()=>{},_ = lifeline()=>{}}}
        while shutdown.drain()["drained"]!=true{tokio::time::sleep(Duration::from_millis(100)).await;}
    }).await?;
    background.abort();
    Ok(())
}
