use anyhow::Result;
use clap::{Parser, Subcommand};
use enginebox_engine_builds::{
    build,
    model::{Recipe, Source},
    read_json, recipe,
};
use std::path::PathBuf;
#[derive(Parser)]
#[command(
    name = "enginebox-build",
    about = "Protected exact-source native engine builds"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Fingerprint {
        #[arg(long)]
        recipe: PathBuf,
    },
    Build {
        #[arg(long)]
        recipe: PathBuf,
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        state: PathBuf,
        #[arg(long)]
        lease: Option<PathBuf>,
    },
}
fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Fingerprint { recipe: path } => {
            let value: Recipe = read_json(&path, 65536)?;
            println!(
                "{}",
                serde_json::to_string(&recipe::public_fingerprint(&value)?)?
            );
        }
        Command::Build {
            recipe: path,
            source,
            root,
            state,
            lease,
        } => {
            let value: Recipe = read_json(&path, 65536)?;
            let source: Source = read_json(&source, 32 * 1024 * 1024)?;
            println!(
                "{}",
                serde_json::to_string(&build(&value, &source, &root, &state, lease.as_deref())?)?
            );
        }
    }
    Ok(())
}
