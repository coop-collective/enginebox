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
    /// Shared Linux checkout storage, with pinned preview qualification.
    Layerfs {
        #[arg(long, global = true)]
        config: Option<PathBuf>,
        #[command(subcommand)]
        command: Command,
    },
}
#[derive(Subcommand)]
enum Command {
    /// Export owned main or a closed branch as verified source for a dev preview.
    DevExport {
        project: String,
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Observe the current accepted LayerFS main revision.
    Main { project: String },
    /// Promote or merge a closed checkpointed branch to LayerFS main.
    Promote {
        project: String,
        name: String,
        #[arg(long)]
        expected_main: String,
        #[arg(long)]
        merge: bool,
        #[arg(long, default_value = "Promote LayerFS workspace")]
        message: String,
    },
    /// Inspect the GitHub connection or enable/disable outbound sync.
    GithubSettings {
        project: String,
        #[arg(long)]
        enabled: Option<bool>,
    },
    /// Accept a LayerFS main promotion and start optional GitHub reconciliation.
    GithubPromote {
        project: String,
        name: String,
        #[arg(long)]
        expected_main: String,
        #[arg(long)]
        merge: bool,
        #[arg(long, default_value = "Promote LayerFS workspace")]
        message: String,
        #[arg(long)]
        token_file: Option<PathBuf>,
    },
    /// Advance an export, obtain a conflict agent task, or accept its resolution.
    GithubDelivery {
        project: String,
        #[arg(long, value_parser=["export","deliver","resolve","retry","agent-task"])]
        action: String,
        #[arg(long)]
        attempt: Option<String>,
        #[arg(long)]
        token_file: Option<PathBuf>,
    },
    /// Fetch a GitHub branch and review its exact committed source tree.
    GithubPreview {
        name: String,
        #[arg(long)]
        repository: String,
        #[arg(long)]
        branch: Option<String>,
        /// Private node credential JSON; never stored in the imported project.
        #[arg(long)]
        token_file: Option<PathBuf>,
    },
    /// Import a reviewed GitHub tree as a LayerFS-owned project.
    GithubImport {
        name: String,
        #[arg(long)]
        revision: String,
        #[arg(long)]
        approval: String,
        #[arg(long, required = true)]
        credential_free: bool,
    },
    /// Fork the current published GitHub generation without changing older forks.
    GithubFork {
        project: String,
        name: String,
        #[arg(long)]
        from: Option<String>,
    },
    /// Inspect prerequisites and immutable artifacts without changing Docker.
    Doctor,
    /// Freeze main or an immutable reference and back up its Store for publication.
    Snapshot {
        project: String,
        name: String,
        #[arg(long, default_value = "main")]
        reference: String,
    },
    /// Merge a frozen exact-base batch using immutable LayerFS references.
    Integrate {
        #[arg(long)]
        request: PathBuf,
    },
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
        /// Import the reviewed tree without Git metadata for logical integration.
        #[arg(long)]
        source_only: bool,
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
    /// Reconcile an interrupted command after proving the original is finished.
    Recover { name: String },
    /// Replace a stopped sandbox from its last completed file checkpoint.
    RestoreCheckpoint {
        name: String,
        /// Edits after that checkpoint remain only in the retained old overlay.
        #[arg(long, required = true)]
        acknowledge_uncheckpointed: bool,
    },
    /// Attach only this checkout to a stopped owned Incus VM at /project.
    IncusAttach {
        name: String,
        #[arg(long)]
        project: String,
        #[arg(long)]
        instance: String,
        #[arg(long)]
        owner: String,
        #[arg(long, default_value_t = 1001)]
        uid: u32,
        #[arg(long, default_value_t = 1001)]
        gid: u32,
    },
    /// Verify the exact VM, Docker process and exported mount identities.
    IncusVerify { name: String },
    /// Detach an export after its exact owned VM has stopped without RAM state.
    IncusDetach { name: String },
    /// Inspect interrupted attachment effects; never replay commands or mounts.
    IncusReconcile {
        name: String,
        /// Finalize only a fully proved attached/detached receipt.
        #[arg(long)]
        apply: bool,
    },
    /// Show local receipts, including interrupted operations requiring inspection.
    List,
}
pub fn run(args: Storage) -> Result<Value> {
    let Backend::Layerfs { config, command } = args.backend;
    if let Command::Preview { repo, revision } = command {
        return Ok(serde_json::to_value(source::preview(&repo, &revision)?)?);
    }
    let config = Config::load(
        &config.context("Pass --config with the node's provisioned LayerFS configuration")?,
    )?;
    if let Command::IncusReconcile { name, apply } = command {
        return Runtime::reconcile_incus(config, &name, apply);
    }
    let runtime = Runtime::new(config)?;
    match command {
        Command::DevExport { project, workspace } => {
            runtime.dev_export(&project, workspace.as_deref())
        }
        Command::Main { project } => runtime.project_main(&project),
        Command::Promote {
            project,
            name,
            expected_main,
            merge,
            message,
        } => runtime.project_promote(&project, &name, &expected_main, merge, &message),
        Command::GithubSettings { project, enabled } => runtime.github_settings(&project, enabled),
        Command::GithubPromote {
            project,
            name,
            expected_main,
            merge,
            message,
            token_file,
        } => runtime.github_promote(
            &project,
            &name,
            &expected_main,
            merge,
            &message,
            token_file.as_deref(),
        ),
        Command::GithubDelivery {
            project,
            action,
            attempt,
            token_file,
        } => runtime.github_delivery(&project, &action, attempt.as_deref(), token_file.as_deref()),
        Command::GithubPreview {
            name,
            repository,
            branch,
            token_file,
        } => runtime.github_preview(&name, &repository, branch.as_deref(), token_file.as_deref()),
        Command::GithubImport {
            name,
            revision,
            approval,
            credential_free: _,
        } => runtime.github_import(&name, &revision, &approval),
        Command::GithubFork {
            project,
            name,
            from,
        } => runtime.github_fork(&project, &name, from.as_deref()),
        Command::Doctor => runtime.doctor(),
        Command::Snapshot {
            project,
            name,
            reference,
        } => runtime.snapshot(&project, &name, &reference),
        Command::Integrate { request } => runtime.integrate(&request),
        Command::Import {
            name,
            repo,
            revision,
            approval,
            credential_free: _,
            source_only,
        } => runtime.import(&name, &repo, &revision, &approval, source_only),
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
        Command::Recover { name } => runtime.recover(&name),
        Command::RestoreCheckpoint {
            name,
            acknowledge_uncheckpointed: _,
        } => runtime.restore_checkpoint(&name),
        Command::IncusAttach {
            name,
            project,
            instance,
            owner,
            uid,
            gid,
        } => runtime.incus_attach(
            &name,
            crate::incus::Target {
                project,
                instance,
                owner,
                uid,
                gid,
            },
        ),
        Command::IncusVerify { name } => runtime.incus_verify(&name),
        Command::IncusDetach { name } => runtime.incus_detach(&name),
        Command::IncusReconcile { .. } => unreachable!(),
        Command::List => runtime.list(),
        Command::Preview { .. } => unreachable!(),
    }
}
