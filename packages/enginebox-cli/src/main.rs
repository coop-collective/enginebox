use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use enginebox_core::{EngineBox, GuestConfig};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    path::PathBuf,
    time::Duration,
};

#[derive(Parser)]
#[command(
    version,
    about = "Engine Box native workspace and fleet administration"
)]
struct Args {
    #[arg(long, global = true, env = "ENGINEBOX_HOST")]
    host: Option<String>,
    /// Registered fleet host ID for capacity-checked lifecycle operations.
    #[arg(
        long,
        global = true,
        env = "ENGINEBOX_FLEET_HOST",
        conflicts_with = "direct"
    )]
    fleet_host: Option<String>,
    /// Explicit administrator maintenance: bypass fleet admission/reattachment.
    #[arg(long, global = true)]
    direct: bool,
    #[arg(
        long,
        global = true,
        env = "ENGINEBOX_PROJECT",
        default_value = "enginebox"
    )]
    project: String,
    #[command(subcommand)]
    command: Commands,
}
#[derive(Subcommand)]
enum Commands {
    /// Optional checkout storage, independent of VM and private agent state.
    Storage(enginebox_storage_layerfs::cli::Storage),
    /// Inspect this computer's fleet capabilities without modifying it.
    Host {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Write a bootstrap configuration; existing files are never overwritten.
    Init {
        #[arg(long, default_value = "enginebox.toml")]
        output: PathBuf,
    },
    /// Inspect the configured Linux host before provisioning.
    Doctor {
        #[arg(long)]
        config: PathBuf,
    },
    /// Show an explicit host provisioning plan.
    Plan {
        #[arg(long)]
        config: PathBuf,
    },
    /// Apply a bootstrap plan, retaining unrelated host resources.
    Apply {
        #[arg(long)]
        config: PathBuf,
    },
    List,
    Status {
        name: String,
    },
    Pause {
        name: String,
    },
    Resume {
        name: String,
    },
    Hibernate {
        name: String,
    },
    Stop {
        name: String,
    },
    Snapshot {
        name: String,
        snapshot: String,
    },
    Fork {
        source: String,
        snapshot: String,
        target: String,
    },
    Restore {
        name: String,
        snapshot: String,
        #[arg(long)]
        disk_only: bool,
    },
    Exec {
        name: String,
        #[arg(long, default_value_t = 60)]
        timeout: u64,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },
    Shell {
        name: String,
    },
    /// Show the private desktop URL and SSH forwarding command.
    Desktop {
        name: String,
    },
    /// Save a workspace desktop screenshot to a local PNG file.
    Screenshot {
        name: String,
        output: PathBuf,
    },
    /// Call the authenticated controller. JSON is read from stdin for POST.
    Api {
        path: String,
        #[arg(long)]
        post: bool,
    },
    /// List native controller agent connections.
    Agents,
    /// Run a named fleet MCP tool with JSON arguments from stdin.
    Tool {
        name: String,
    },
    /// Launch the browser MCP companion using a private agent browser profile.
    Browser {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}
fn companion(binary: &str, args: &[String]) -> Result<()> {
    let current = std::env::current_exe()?;
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    let executable = current
        .parent()
        .context("Executable has no directory")?
        .join(format!("{binary}{suffix}"));
    ensure!(
        executable.is_file(),
        "Install the complete Engine Box release; {binary} is missing"
    );
    let status = std::process::Command::new(executable).args(args).status()?;
    ensure!(status.success(), "{binary} exited unsuccessfully");
    Ok(())
}
fn input() -> Result<Value> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 1024 * 1024, "Input exceeds 1 MiB");
    Ok(serde_json::from_slice(&bytes)?)
}
fn output(v: Value) -> Result<()> {
    serde_json::to_writer_pretty(std::io::stdout(), &v)?;
    println!();
    Ok(())
}
fn main() -> Result<()> {
    let args = Args::parse();
    match args.command {
        Commands::Storage(storage) => {
            ensure!(
                args.host.as_deref().is_none_or(|host| host == "local")
                    && args.fleet_host.is_none(),
                "Storage commands are node-local; SSH to the selected Linux node and use --host local"
            );
            let result = enginebox_storage_layerfs::cli::run(storage)?;
            let failed = result["ok"] == false;
            output(result)?;
            if failed {
                std::process::exit(2);
            }
            return Ok(());
        }
        Commands::Host { args } => return companion("enginebox-host", &args),
        Commands::Init { output } => {
            return companion(
                "enginebox-host",
                &[
                    "bootstrap".into(),
                    "init".into(),
                    "--output".into(),
                    output.to_string_lossy().into_owned(),
                ],
            );
        }
        Commands::Doctor { config } => {
            return companion(
                "enginebox-host",
                &[
                    "bootstrap".into(),
                    "doctor".into(),
                    "--config".into(),
                    config.to_string_lossy().into_owned(),
                ],
            );
        }
        Commands::Plan { config } => {
            return companion(
                "enginebox-host",
                &[
                    "bootstrap".into(),
                    "plan".into(),
                    "--config".into(),
                    config.to_string_lossy().into_owned(),
                ],
            );
        }
        Commands::Apply { config } => {
            return companion(
                "enginebox-host",
                &[
                    "bootstrap".into(),
                    "apply".into(),
                    "--config".into(),
                    config.to_string_lossy().into_owned(),
                ],
            );
        }
        Commands::Browser { args } => return companion("enginebox-browser", &args),
        Commands::Api { path, post } => {
            return output(
                enginebox_cli::FleetClient::from_env()?
                    .request(&path, if post { Some(input()?) } else { None })?,
            );
        }
        Commands::Agents => {
            return output(enginebox_cli::FleetClient::from_env()?.request("/v1/agents", None)?);
        }
        Commands::Tool { name } => {
            let (path, body) = enginebox_cli::tool_request(&name, &input()?)?;
            return output(enginebox_cli::FleetClient::from_env()?.request(&path, body)?);
        }
        _ => {}
    }
    ensure!(
        std::env::var("ENGINEBOX_GUEST").unwrap_or_default() != "1",
        "Host operations are unavailable inside a guest"
    );
    let lifecycle = match &args.command {
        Commands::Pause { name } => Some((name, "pause")),
        Commands::Resume { name } => Some((name, "resume")),
        Commands::Hibernate { name } => Some((name, "hibernate")),
        Commands::Stop { name } => Some((name, "stop")),
        _ => None,
    };
    if let Some((name, action)) = lifecycle
        && !args.direct
    {
        let host = args.fleet_host.as_deref().context(
            "Set --fleet-host or ENGINEBOX_FLEET_HOST to a registered host ID; direct administrator maintenance requires --direct",
        )?;
        enginebox_core::identifier(host, false)?;
        enginebox_core::identifier(name, true)?;
        return output(enginebox_cli::FleetClient::from_env()?.request(
            &format!("/v1/hosts/{host}/instances/{name}/actions"),
            Some(json!({"action":action})),
        )?);
    }
    let mutation_name = match &args.command {
        Commands::Pause { name }
        | Commands::Resume { name }
        | Commands::Hibernate { name }
        | Commands::Stop { name }
        | Commands::Snapshot { name, .. }
        | Commands::Restore { name, .. } => Some(name),
        Commands::Fork { source, .. } => Some(source),
        _ => None,
    };
    ensure!(
        mutation_name.is_none() || args.direct,
        "Snapshot/fork/restore are private administrator maintenance; use --direct only after quiescing fleet work"
    );
    let engine = EngineBox::new(
        args.host
            .as_deref()
            .context("Set --host or ENGINEBOX_HOST to an SSH target or local")?,
        &args.project,
        GuestConfig::from_env()?,
    )?;
    let _host_lock = if let Some(name) = mutation_name {
        let host = enginebox_core::Host {
            target: args.host.clone().unwrap(),
            project: args.project.clone(),
            guest: engine.guest.clone(),
            ..Default::default()
        };
        let lock = enginebox_core::operation_lock(&host)?;
        enginebox_core::guest::assert_remote_idle(&host, name)?;
        Some(lock)
    } else {
        None
    };
    let value = match args.command {
        Commands::List => engine.list()?,
        Commands::Status { name } => engine.status(&name)?,
        Commands::Pause { name } => engine.lifecycle(&name, "pause")?,
        Commands::Resume { name } => engine.lifecycle(&name, "resume")?,
        Commands::Hibernate { name } => engine.lifecycle(&name, "hibernate")?,
        Commands::Stop { name } => engine.lifecycle(&name, "stop")?,
        Commands::Snapshot { name, snapshot } => engine.snapshot(&name, &snapshot)?,
        Commands::Desktop { name } => engine.desktop(&name)?,
        Commands::Screenshot { name, output } => {
            let bytes = engine.execute(
                &name,
                &["enginebox-guest".into(), "screenshot".into()],
                Duration::from_secs(30),
            )?;
            ensure!(
                bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
                "Guest returned an invalid screenshot"
            );
            std::fs::write(&output, bytes)?;
            json!({"output": output})
        }
        Commands::Fork {
            source,
            snapshot,
            target,
        } => engine.fork(&source, &snapshot, &target)?,
        Commands::Restore {
            name,
            snapshot,
            disk_only,
        } => engine.restore(&name, &snapshot, !disk_only)?,
        Commands::Exec {
            name,
            timeout,
            argv,
        } => {
            let result = engine.execute(&name, &argv, Duration::from_secs(timeout))?;
            std::io::stdout().write_all(&result)?;
            return Ok(());
        }
        Commands::Shell { name } => {
            let mut argv = engine.command(&engine.exec_args(&name, &["bash".into()])?);
            if argv.first().is_some_and(|p| p == "ssh")
                && let Some(flag) = argv.iter_mut().find(|p| p.as_str() == "-T")
            {
                *flag = "-tt".into();
            }
            let status = std::process::Command::new(&argv[0])
                .args(&argv[1..])
                .status()?;
            ensure!(status.success(), "Workspace shell failed");
            json!({"ok":true})
        }
        _ => unreachable!(),
    };
    output(value)
}
