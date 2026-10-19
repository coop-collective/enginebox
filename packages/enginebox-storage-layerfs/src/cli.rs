use crate::{config::Config, runtime::Runtime, source};
use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Args)]
pub struct Storage {
    #[command(subcommand)]
    backend: Backend,
}
#[derive(Subcommand)]
enum Backend {
    /// Experimental Linux/Docker checkouts, separate from Incus VM storage.
    Layerfs {
        #[arg(long, global = true)]
        config: Option<PathBuf>,
        #[command(subcommand)]
        command: Command,
    },
}
#[derive(Subcommand)]
enum Command {
    /// Inspect prerequisites and immutable artifacts without changing Docker.
    Doctor,
    /// Review committed paths and obtain the import approval digest.
    Preview {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long, default_value = "HEAD")]
        revision: String,
    },
    /// Import only an approved Git commit into a new, private project Store.
    Import {
        name: String,
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        revision: String,
        #[arg(long)]
        approval: String,
        /// Assert that the reviewed tracked content is suitable for reuse.
        #[arg(long, required = true)]
        credential_free: bool,
    },
    /// Fork a project's base into a separately limited writable sandbox.
    Fork {
        project: String,
        name: String,
        #[arg(long)]
        from: Option<String>,
    },
    /// Run argv directly at /project as the sandbox's unprivileged user.
    Exec {
        name: String,
        #[arg(long, default_value_t = 120)]
        timeout: u64,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },
    /// Install from an immutable pnpm cache, offline, with scripts disabled.
    Deps { name: String },
    /// Commit file changes; this is not a durable or VM-memory checkpoint.
    Checkpoint {
        name: String,
        #[arg(long, default_value = "Engine Box file checkpoint")]
        message: String,
    },
    /// Remount a clean checkpoint at the same /project path.
    Remount { name: String },
    /// Close a clean workspace; refuses uncheckpointed edits, never discards.
    Close { name: String },
    /// Show local receipts, including interrupted operations requiring inspection.
    List,
}
pub fn run(args: Storage) -> Result<Value> {
    let Backend::Layerfs { config, command } = args.backend;
    if let Command::Preview { repo, revision } = command {
        return Ok(serde_json::to_value(source::preview(&repo, &revision)?)?);
    }
    let runtime = Runtime::new(Config::load(
        &config.context("Pass --config with an explicit experimental LayerFS configuration")?,
    )?)?;
    match command {
        Command::Doctor => runtime.doctor(),
        Command::Import {
            name,
            repo,
            revision,
            approval,
            credential_free: _,
        } => runtime.import(&name, &repo, &revision, &approval),
        Command::Fork {
            project,
            name,
            from,
        } => runtime.fork(&project, &name, from.as_deref()),
        Command::Exec {
            name,
            timeout,
            argv,
        } => runtime.operate(&name, "exec", &argv, timeout, ""),
        Command::Deps { name } => runtime.operate(&name, "deps", &[], 300, ""),
        Command::Checkpoint { name, message } => {
            runtime.operate(&name, "checkpoint", &[], 120, &message)
        }
        Command::Remount { name } => runtime.operate(&name, "remount", &[], 120, ""),
        Command::Close { name } => runtime.operate(&name, "close", &[], 120, ""),
        Command::List => runtime.list(),
        Command::Preview { .. } => unreachable!(),
    }
}
