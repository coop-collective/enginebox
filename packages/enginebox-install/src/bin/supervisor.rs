use anyhow::{Context, Result, ensure};
use clap::Parser;
use enginebox_install::{
    release::Installation,
    supervisor::{SupervisorConfig, run},
};
use serde::Deserialize;
use std::{net::SocketAddr, path::PathBuf, time::Duration};
#[derive(Parser)]
#[command(
    name = "enginebox-supervisor",
    version,
    about = "Stable Engine Box API/dashboard front door; controller versions update without replacing this process"
)]
struct Args {
    #[arg(long)]
    config: PathBuf,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    prefix: PathBuf,
    fleet_config: PathBuf,
    token_file: PathBuf,
    #[serde(default)]
    environment_file: Option<PathBuf>,
    listen: SocketAddr,
    #[serde(default)]
    dashboard_listen: Option<SocketAddr>,
    #[serde(default = "drain")]
    drain_timeout_seconds: u64,
}
fn drain() -> u64 {
    300
}
fn private_file(path: &std::path::Path) -> Result<String> {
    enginebox_install::release::reject_symlink_components(path)?;
    let meta = std::fs::metadata(path)?;
    ensure!(meta.is_file(), "private input must be a regular file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            meta.permissions().mode() & 0o077 == 0,
            "private input must have owner-only permissions: {}",
            path.display()
        );
    }
    Ok(std::fs::read_to_string(path)?)
}
fn main() -> Result<()> {
    let args = Args::parse();
    let config: Config = serde_json::from_str(&std::fs::read_to_string(args.config)?)?;
    // Load before starting any threads: Rust 2024 environment mutation is otherwise unsafe.
    if let Some(path) = &config.environment_file {
        let values: std::collections::BTreeMap<String, String> =
            serde_json::from_str(&private_file(path)?)?;
        for (key, value) in values {
            ensure!(
                [
                    "ENGINEBOX_DASHBOARD_PASSWORD",
                    "ENGINEBOX_SESSION_SECRET",
                    "ENGINEBOX_DASHBOARD_ORIGIN",
                    "PATH"
                ]
                .contains(&key.as_str()),
                "unsupported service environment setting"
            );
            ensure!(!value.contains('\0'), "invalid environment value");
            unsafe {
                std::env::set_var(key, value);
            }
        }
    }
    let token = private_file(&config.token_file)?.trim().to_owned();
    ensure!(token.len() >= 32, "API token too short");
    let config = SupervisorConfig {
        install: Installation::inspect(config.prefix)?,
        fleet_config: config.fleet_config,
        api_listen: config.listen,
        dashboard_listen: config.dashboard_listen,
        token,
        drain_timeout: Duration::from_secs(config.drain_timeout_seconds),
    };
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("start supervisor runtime")?
        .block_on(run(config))
}
