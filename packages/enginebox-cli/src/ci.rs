use anyhow::{Context, Result, ensure};
use clap::{Args, Subcommand};
use enginebox_ci::{
    Node,
    executor::{self, Execution},
    model::{Os, architecture},
};
use serde_json::{Value, json};
use std::{
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Args)]
pub struct Ci {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Report native OS/architecture; does not provision images or claim toolchain readiness.
    Inspect,
    /// List registered CI node capabilities.
    Nodes,
    /// Desired CI toolchains and per-node installation status.
    Templates,
    /// Register a versioned toolchain recipe (new recipes start disabled).
    TemplateRegister {
        #[arg(long)]
        manifest: PathBuf,
    },
    TemplateEnable {
        id: String,
    },
    TemplateDisable {
        id: String,
    },
    /// Explicitly retry a failed installation on one node.
    TemplateRetry {
        id: String,
        #[arg(long)]
        node: String,
    },
    /// Register a prepared native worker using a JSON manifest.
    Register {
        #[arg(long)]
        manifest: PathBuf,
    },
    /// Resolve a CI trigger without executing it.
    Plan {
        #[arg(long)]
        request: PathBuf,
    },
    /// Execute a trigger once per durable request_id; poll ebox ci job.
    Trigger {
        #[arg(long)]
        request: PathBuf,
    },
    Job {
        id: String,
    },
    /// Node-side SSH protocol; reads a bounded JSON request from stdin.
    Execute,
    /// Configure a fresh official GitHub runner, then execute one job.
    GithubRunner {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        directory: PathBuf,
        #[arg(long)]
        repository: String,
    },
    GithubStatus {
        repository: String,
        run: u64,
    },
    GithubCancel {
        repository: String,
        run: u64,
    },
}
fn file(path: &Path) -> Result<Value> {
    limited_file(path, 16384)
}
fn limited_file(path: &Path, limit: u64) -> Result<Value> {
    let mut data = Vec::new();
    std::fs::File::open(path)?
        .take(limit + 1)
        .read_to_end(&mut data)?;
    ensure!(data.len() as u64 <= limit, "CI JSON exceeds its size limit");
    enginebox_core::templates::decode_json(&data)
}
pub fn run(args: Ci) -> Result<Value> {
    ensure!(
        std::env::var("ENGINEBOX_GUEST").unwrap_or_default() != "1",
        "CI operator commands are unavailable in agent guest mode"
    );
    match args.command {
        Command::Inspect => Ok(
            json!({"schema_version":1,"os":Os::current()?,"architecture":architecture()?,"execution":"native","concurrent_jobs":1,"read_only":true,"image_provisioning":false,"template_installation":1,"capabilities_verified":false}),
        ),
        Command::Execute => {
            let mut data = Vec::new();
            std::io::stdin().take(131073).read_to_end(&mut data)?;
            ensure!(data.len() <= 131072, "CI execution request too large");
            let value = enginebox_core::templates::decode_json(&data)?;
            if value.get("operation").is_some() {
                let request: enginebox_ci::provision::InstallRequest =
                    serde_json::from_value(value)?;
                return Ok(serde_json::to_value(enginebox_ci::provision::install_at(
                    &request,
                    &executor::root()?,
                )?)?);
            }
            let execution: Execution = serde_json::from_value(value)?;
            executor::execute_managed_at(&execution.node, &execution.request, &executor::root()?, execution.template.as_ref()).map_err(|_| anyhow::anyhow!("CI automation failed; inspect node readiness, commit access, script and timeout"))
        }
        Command::GithubRunner {
            manifest,
            directory,
            repository,
        } => {
            let node: Node = serde_json::from_value(file(&manifest)?)?;
            executor::github_runner(&node, &directory, &repository)
        }
        command => {
            let client = crate::FleetClient::from_env()?;
            let (path, body) = match command {
                Command::Nodes => ("/v1/ci".into(), None),
                Command::Templates => ("/v1/ci/templates".into(), None),
                Command::TemplateRegister { manifest } => {
                    let manifest: enginebox_ci::templates::Template =
                        serde_json::from_value(limited_file(&manifest, 32768)?)?;
                    manifest.validate()?;
                    (
                        "/v1/ci/templates".into(),
                        Some(json!({"manifest":manifest})),
                    )
                }
                Command::TemplateEnable { id }
                | Command::TemplateDisable { id }
                | Command::TemplateRetry { id, .. }
                    if !enginebox_ci::model::identifier(&id) =>
                {
                    anyhow::bail!("Invalid CI template ID")
                }
                Command::TemplateEnable { id } => {
                    (format!("/v1/ci/templates/{id}/enable"), Some(json!({})))
                }
                Command::TemplateDisable { id } => {
                    (format!("/v1/ci/templates/{id}/disable"), Some(json!({})))
                }
                Command::TemplateRetry { id, node } => {
                    ensure!(enginebox_ci::model::identifier(&node), "Invalid CI node ID");
                    (
                        format!("/v1/ci/templates/{id}/retry"),
                        Some(json!({"node":node})),
                    )
                }
                Command::Register { manifest } => ("/v1/ci/nodes".into(), Some(file(&manifest)?)),
                Command::Plan { request } => ("/v1/ci/plan".into(), Some(file(&request)?)),
                Command::Trigger { request } => ("/v1/ci/runs".into(), Some(file(&request)?)),
                Command::Job { id } => {
                    ensure!(
                        id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()),
                        "Invalid job ID"
                    );
                    (format!("/v1/jobs/{id}"), None)
                }
                Command::GithubStatus { repository, run } => {
                    ensure!(
                        enginebox_ci::model::repository(&repository) && run > 0,
                        "Invalid GitHub run"
                    );
                    (format!("/v1/ci/github/{repository}/runs/{run}"), None)
                }
                Command::GithubCancel { repository, run } => {
                    ensure!(
                        enginebox_ci::model::repository(&repository) && run > 0,
                        "Invalid GitHub run"
                    );
                    (
                        format!("/v1/ci/github/{repository}/runs/{run}/cancel"),
                        Some(json!({})),
                    )
                }
                _ => anyhow::bail!("Unexpected CI command"),
            };
            client.request(&path, body).context("CI API request failed")
        }
    }
}
