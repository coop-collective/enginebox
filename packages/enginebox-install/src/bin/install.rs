use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use enginebox_install::{
    bootstrap::{self, Role},
    release::{Installation, download},
};
use semver::Version;
use std::{path::PathBuf, time::Duration};
#[derive(Parser)]
#[command(
    version,
    about = "Inspect, install, pin and roll back Engine Box. Planning is read-only; changes require explicit commands."
)]
struct Args {
    #[arg(long)]
    prefix: PathBuf,
    #[command(subcommand)]
    command: Action,
}
#[derive(Subcommand)]
enum Action {
    /// Read-only network checks; never changes VPN, firewall, routes or enrollment.
    NetworkCheck(enginebox_install::network::Options),
    /// Read-only OS/dependency inspection. No package manager runs without --apply-dependencies.
    Plan {
        #[arg(long, value_enum, default_value = "controller")]
        role: Role,
        #[arg(long)]
        apply_dependencies: bool,
    },
    /// Verify and stage an immutable pinned release. The trusted manifest hash must be supplied explicitly.
    Stage {
        #[arg(long)]
        version: Version,
        #[arg(long)]
        manifest: String,
        #[arg(long)]
        manifest_sha256: String,
        #[arg(long)]
        archive: String,
    },
    /// Initial/offline activation only. A running supervisor prevents this command; use update instead.
    Activate {
        #[arg(long)]
        version: Version,
    },
    Status,
    /// Health-check, drain, and switch a running supervisor to an already staged release.
    Update {
        #[arg(long)]
        version: Version,
        #[arg(long, default_value = "http://127.0.0.1:8765")]
        url: String,
        #[arg(long)]
        token_file: PathBuf,
    },
    /// Switch to the last active version after the same compatibility/health/drain checks.
    Rollback {
        #[arg(long, default_value = "http://127.0.0.1:8765")]
        url: String,
        #[arg(long)]
        token_file: PathBuf,
    },
    /// Print a user-service definition; --apply installs and starts it without overwriting another service.
    Service {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        apply: bool,
    },
    /// Publish the root-owned Linux node helper on sudo secure_path. Refuses user-writable installations.
    ExposeHost {
        #[arg(long)]
        apply: bool,
    },
}
#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let install = Installation::inspect(args.prefix)?;
    match args.command {
        Action::NetworkCheck(options) => enginebox_install::network::print(&options).await?,
        Action::Plan {
            role,
            apply_dependencies,
        } => {
            let plan = bootstrap::plan(role)?;
            println!("{}", serde_json::to_string_pretty(&plan)?);
            if apply_dependencies {
                bootstrap::apply_dependencies(&plan)?;
            }
        }
        Action::Stage {
            version,
            manifest,
            manifest_sha256,
            archive,
        } => {
            // Reject malformed pins before any filesystem changes or network activity.
            enginebox_install::release::validate_digest(&manifest_sha256)?;
            ensure!(
                version.build.is_empty(),
                "release pin cannot include build metadata"
            );
            let temp = tempfile::tempdir()?;
            let manifest = if manifest.starts_with("https://") {
                let p = temp.path().join("manifest.json");
                download(&manifest, &p, 65536).await?;
                p
            } else {
                PathBuf::from(manifest)
            };
            let archive = if archive.starts_with("https://") {
                let p = temp.path().join("release.tar.gz");
                download(&archive, &p, 4 * 1024 * 1024 * 1024).await?;
                p
            } else {
                PathBuf::from(archive)
            };
            install.initialize()?;
            let _lock = install.lock()?;
            let receipt = install.stage(&version, &manifest, &manifest_sha256, &archive)?;
            let example = bootstrap::write_example_config(&install)?;
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"staged":receipt.version,"target":receipt.target,"active":install.active()?,"example_config":example})
                )?
            );
        }
        Action::Activate { version } => {
            let _supervisor = install
                .named_lock("supervisor.lock")
                .context("supervisor is running; use update for a drained online cutover")?;
            let _lock = install.lock()?;
            println!(
                "{}",
                serde_json::to_string_pretty(&install.activate(&version)?)?
            );
        }
        Action::Status => println!("{}", serde_json::to_string_pretty(&install.active()?)?),
        Action::Update {
            version,
            url,
            token_file,
        } => {
            management(
                &url,
                "update",
                &token_file,
                serde_json::json!({"version":version}),
            )
            .await?
        }
        Action::Rollback { url, token_file } => {
            management(&url, "rollback", &token_file, serde_json::json!({})).await?
        }
        Action::Service { config, apply } => {
            let plan = bootstrap::service_plan(&install, &config)?;
            println!("{}", serde_json::to_string_pretty(&plan)?);
            if apply {
                bootstrap::apply_service(&plan)?;
            }
        }
        Action::ExposeHost { apply } => println!(
            "{}",
            serde_json::to_string_pretty(&bootstrap::expose_host(&install, apply)?)?
        ),
    }
    Ok(())
}
async fn management(
    url: &str,
    action: &str,
    token_file: &std::path::Path,
    value: serde_json::Value,
) -> Result<()> {
    let base = reqwest::Url::parse(url)?;
    ensure!(
        base.scheme() == "http"
            && matches!(base.host_str(), Some("127.0.0.1" | "[::1]" | "localhost")),
        "management requests must use the local supervisor HTTP loopback address"
    );
    enginebox_install::release::reject_symlink_components(token_file)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            std::fs::metadata(token_file)?.permissions().mode() & 0o077 == 0,
            "token file must have owner-only permissions"
        );
    }
    let token = std::fs::read_to_string(token_file)?;
    ensure!(token.trim().len() >= 32, "API token too short");
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(1000))
        .redirect(reqwest::redirect::Policy::none())
        .build()?
        .post(format!(
            "{}/__enginebox/{action}",
            url.trim_end_matches('/')
        ))
        .bearer_auth(token.trim())
        .json(&value)
        .send()
        .await?;
    let status = response.status();
    let body: serde_json::Value = response.json().await?;
    ensure!(status.is_success(), "supervisor update rejected: {body}");
    println!("{}", serde_json::to_string_pretty(&body)?);
    Ok(())
}
