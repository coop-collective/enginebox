use anyhow::Result;
use clap::{Parser, Subcommand};
use enginebox_install::contributor;
use enginebox_install::native_node::{self, Profile, Worker};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "enginebox-node",
    version,
    about = "Native node: register CI/agent workers and inspect or enroll GPU workers"
)]
struct Args {
    #[command(subcommand)]
    command: Action,
}
#[derive(Subcommand)]
enum Action {
    RegisterWorker {
        #[arg(long)]
        controller: String,
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        cpu: u32,
        #[arg(long)]
        memory_gib: u32,
        #[arg(long)]
        disk_gib: u32,
        #[arg(long, hide = true)]
        test_loopback_http: bool,
    },
    RunWorker {
        #[arg(long)]
        config: PathBuf,
        #[arg(long, hide = true)]
        test_loopback_http: bool,
    },
    #[command(hide = true)]
    ExecuteWorker,
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
        Action::RegisterWorker {
            controller,
            config,
            cpu,
            memory_gib,
            disk_gib,
            test_loopback_http,
        } => {
            let code = native_node::read_enrollment_stdin().await?;
            let result = contributor::register(
                &config,
                &controller,
                &code,
                enginebox_node_protocol::Budget {
                    cpu,
                    memory_gib,
                    disk_gib,
                },
                test_loopback_http,
            )
            .await?;
            println!("{result}");
        }
        Action::RunWorker {
            config,
            test_loopback_http,
        } => contributor::run(&config, test_loopback_http).await?,
        Action::ExecuteWorker => {
            use tokio::io::AsyncReadExt;
            let mut input = Vec::new();
            tokio::io::stdin()
                .take(64 * 1024 + 1)
                .read_to_end(&mut input)
                .await?;
            anyhow::ensure!(input.len() <= 64 * 1024, "Worker request too large");
            let input = serde_json::from_slice(&input)?;
            let result = tokio::task::spawn_blocking(move || contributor::execute(input)).await?;
            println!("{}",result.unwrap_or_else(|_|serde_json::json!({"ok":false,"error":"Native worker failed; check toolchain, provider sign-in and the submitted request"})));
        }
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
