use anyhow::{Result, ensure};
use clap::{Parser, Subcommand};
use enginebox_core::{bootstrap, capabilities, capacity, guest, model::GuestConfig, state};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    path::PathBuf,
};
#[derive(Parser)]
#[command(
    name = "enginebox-host",
    version,
    about = "Read-only host inventory and explicit Engine Box provisioning"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
    #[arg(long)]
    json: bool,
    #[arg(long)]
    docker: bool,
    #[arg(long, default_value = ".")]
    storage_path: PathBuf,
    #[arg(long)]
    cpus: Option<u64>,
    #[arg(long)]
    memory_gib: Option<f64>,
    #[arg(long)]
    disk_gib: Option<f64>,
}
#[derive(Subcommand)]
enum Commands {
    /// Atomically update helper executables in one owned, running VM; homes and native CLIs stay intact.
    SyncGuestTools {
        #[arg(long)]
        project: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        binary_dir: Option<PathBuf>,
        #[arg(long, hide = true)]
        already_locked: bool,
    },
    Inventory {
        #[arg(long)]
        pool: String,
        #[arg(long)]
        project: String,
    },
    Bootstrap {
        #[command(subcommand)]
        command: Bootstrap,
    },
    #[command(hide = true)]
    Lock,
    #[command(hide = true)]
    Readiness,
    #[command(hide = true)]
    GuestIdentity,
    #[command(hide = true)]
    ResetBaseIdentity,
    #[command(hide = true)]
    CleanBaseCheck {
        #[arg(long)]
        home: PathBuf,
    },
    #[command(hide = true)]
    PrepareGuest,
    #[command(hide = true)]
    SetupUserMcp,
}
#[derive(Subcommand)]
enum Bootstrap {
    Init {
        #[arg(long)]
        output: PathBuf,
    },
    Doctor {
        #[arg(long)]
        config: PathBuf,
    },
    Plan {
        #[arg(long)]
        config: PathBuf,
    },
    Apply {
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        binary_dir: Option<PathBuf>,
    },
    #[command(hide = true)]
    Inspect,
}
fn stdin_json() -> Result<Value> {
    let mut data = Vec::new();
    std::io::stdin()
        .take(1024 * 1024 + 1)
        .read_to_end(&mut data)?;
    ensure!(data.len() <= 1024 * 1024, "Input exceeds 1 MiB");
    Ok(serde_json::from_slice(&data)?)
}
fn execute(cli: Cli) -> Result<Value> {
    match cli.command {
        Some(Commands::SyncGuestTools {
            project,
            name,
            binary_dir,
            already_locked,
        }) => {
            let _guard = if already_locked {
                None
            } else {
                Some(state::root_host_lock()?)
            };
            guest::sync_guest_tools(&project, &name, binary_dir.as_deref())
        }
        None => capabilities::inspect(
            &cli.storage_path,
            cli.docker,
            cli.cpus,
            cli.memory_gib,
            cli.disk_gib,
        ),
        Some(Commands::Inventory { pool, project }) => capacity::local_inventory(&pool, &project),
        Some(Commands::Lock) => {
            let _guard = state::root_host_lock()?;
            println!("locked");
            std::io::stdout().flush()?;
            let mut bytes = [0u8; 1024];
            while std::io::stdin().read(&mut bytes)? > 0 {}
            Ok(Value::Null)
        }
        Some(Commands::Readiness) => {
            guest::readiness(&serde_json::from_value::<GuestConfig>(stdin_json()?)?)
        }
        Some(Commands::GuestIdentity) => guest::guest_identity(),
        Some(Commands::ResetBaseIdentity) => guest::reset_base_identity(),
        Some(Commands::CleanBaseCheck { home }) => guest::clean_base_check(&home),
        Some(Commands::PrepareGuest) => guest::prepare_guest(&stdin_json()?),
        Some(Commands::SetupUserMcp) => guest::setup_user_mcp(&stdin_json()?),
        Some(Commands::Bootstrap { command }) => match command {
            Bootstrap::Init { output } => {
                ensure!(!output.exists(), "Configuration already exists");
                let value = toml::Value::try_from(bootstrap::defaults())?;
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let mut f = options.open(&output)?;
                f.write_all(toml::to_string_pretty(&value)?.as_bytes())?;
                Ok(
                    json!({"created":output,"next":"Set explicit target, storage pool and managed network, then run ebox doctor/plan before apply"}),
                )
            }
            Bootstrap::Doctor { config } | Bootstrap::Plan { config } => {
                let c = bootstrap::load_config(&config)?;
                let observed = bootstrap::inventory(&c)?;
                bootstrap::build_plan(&c, &observed)
            }
            Bootstrap::Inspect => bootstrap::inspect_local(&bootstrap::normalize(stdin_json()?)?),
            Bootstrap::Apply { config, binary_dir } => match config {
                Some(path) => {
                    bootstrap::apply(&bootstrap::load_config(&path)?, binary_dir.as_deref())
                }
                None => bootstrap::apply_local(
                    &bootstrap::normalize(stdin_json()?)?,
                    binary_dir.as_deref(),
                ),
            },
        },
    }
}
fn main() {
    let cli = Cli::parse();
    match execute(cli) {
        Ok(v) => {
            if !v.is_null() {
                println!("{}", serde_json::to_string_pretty(&v).expect("JSON report"));
            }
            if v["ok"] == false
                || v["allocation"]["status"] == "rejected"
                || v["docker"]["allocation"]["status"] == "rejected"
            {
                std::process::exit(2);
            }
        }
        Err(e) => {
            eprintln!("Engine Box: {e:#}");
            std::process::exit(1);
        }
    }
}
