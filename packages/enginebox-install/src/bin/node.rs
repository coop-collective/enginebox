use anyhow::Result;
use clap::{Parser, Subcommand};
use enginebox_install::native_node::{self, Profile, Worker};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "enginebox-node",
    version,
    about = "Native GPU node: inspect, enroll and supervise an owner-configured worker"
)]
struct Args {
    #[command(subcommand)]
    command: Action,
}
#[derive(Subcommand)]
enum Action {
    Inspect {
        #[arg(long, value_enum)]
        profile: Profile,
        #[arg(long)]
        ffmpeg: PathBuf,
        #[arg(long)]
        qualify: bool,
    },
    Enroll {
        #[arg(long)]
        controller: String,
        #[arg(long)]
        config: PathBuf,
        #[arg(long, value_enum)]
        profile: Profile,
        #[arg(long)]
        ffmpeg: PathBuf,
        /// Optional JSON containing a local executable, SHA-256, args and working directory.
        #[arg(long)]
        worker: Option<PathBuf>,
        #[arg(long, hide = true)]
        test_loopback_http: bool,
    },
    Run {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        once: bool,
        #[arg(long, hide = true)]
        test_loopback_http: bool,
    },
}
#[tokio::main]
async fn main() -> Result<()> {
    match Args::parse().command {
        Action::Inspect {
            profile,
            ffmpeg,
            qualify,
        } => println!(
            "{}",
            serde_json::to_string_pretty(&native_node::inspect(profile, &ffmpeg, qualify).await?)?
        ),
        Action::Enroll {
            controller,
            config,
            profile,
            ffmpeg,
            worker,
            test_loopback_http,
        } => {
            let worker: Option<Worker> = worker
                .map(|path| -> Result<_> { Ok(serde_json::from_slice(&std::fs::read(path)?)?) })
                .transpose()?;
            let token = native_node::read_enrollment_stdin().await?;
            println!(
                "{}",
                native_node::enroll(
                    &config,
                    &controller,
                    &token,
                    profile,
                    ffmpeg,
                    worker,
                    test_loopback_http
                )
                .await?
            );
        }
        Action::Run {
            config,
            once,
            test_loopback_http,
        } => native_node::run(&config, once, test_loopback_http).await?,
    }
    Ok(())
}
