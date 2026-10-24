use anyhow::{Context, Result, ensure};
use clap::Parser;
use enginebox_install::release::{Manifest, sha256_file};
use semver::Version;
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
};
#[derive(Parser)]
#[command(
    name = "enginebox-release",
    version,
    disable_version_flag = true,
    about = "Build a deterministic Engine Box release archive and pinned integrity manifest from already-tested binaries"
)]
struct Args {
    #[arg(long)]
    version: Version,
    #[arg(long)]
    target: String,
    #[arg(long)]
    bin_dir: PathBuf,
    #[arg(long)]
    license: PathBuf,
    #[arg(long)]
    notice: PathBuf,
    #[arg(long)]
    dashboard: Option<PathBuf>,
    #[arg(long, requires = "dashboard")]
    dashboard_start: Option<PathBuf>,
    #[arg(long)]
    output: PathBuf,
    #[arg(long, default_value_t = 1)]
    state_schema: u32,
    /// Explicit binaries; repeat for each. Required service/operator binaries are always included.
    #[arg(long)]
    binary: Vec<String>,
}
fn add_file(
    builder: &mut tar::Builder<flate2::write::GzEncoder<File>>,
    source: &Path,
    destination: &Path,
    executable: bool,
) -> Result<()> {
    ensure!(
        source.is_file() && !source.is_symlink(),
        "release input must be a regular non-symlink file: {}",
        source.display()
    );
    let mut input = File::open(source)?;
    let mut header = tar::Header::new_gnu();
    header.set_size(input.metadata()?.len());
    header.set_mode(if executable { 0o755 } else { 0o644 });
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    builder.append_data(&mut header, destination, &mut input)?;
    Ok(())
}
fn add_tree(
    builder: &mut tar::Builder<flate2::write::GzEncoder<File>>,
    source: &Path,
    destination: &Path,
    root: &Path,
) -> Result<()> {
    let mut entries: Vec<_> = fs::read_dir(source)?.collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|v| v.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        let text = name.to_string_lossy();
        ensure!(
            ![".env", ".state", ".git", ".enginebox", ".fleet.local.toml"].contains(&text.as_ref())
                && !text.starts_with(".env."),
            "private input found in release dashboard"
        );
        let canonical = path.canonicalize()?;
        ensure!(
            canonical.starts_with(root),
            "dashboard symlink escapes its standalone artifact"
        );
        ensure!(
            !path.is_symlink(),
            "dashboard contains a symlink; materialize standalone dependencies inside the artifact before packaging: {}",
            path.display()
        );
        let dest = destination.join(name);
        if path.is_dir() {
            add_tree(builder, &path, &dest, root)?;
        } else {
            add_file(builder, &path, &dest, false)?;
        }
    }
    Ok(())
}
fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(!args.output.exists(), "output directory already exists");
    ensure!(
        args.dashboard.is_some() == args.dashboard_start.is_some(),
        "dashboard and dashboard-start must be provided together"
    );
    let parent = args
        .output
        .parent()
        .context("output requires a parent directory")?;
    fs::create_dir_all(parent)?;
    let staging = tempfile::tempdir_in(parent)?;
    let archive_name = format!("enginebox-{}-{}.tar.gz", args.version, args.target);
    let archive = staging.path().join(&archive_name);
    let encoder = flate2::GzBuilder::new()
        .mtime(0)
        .write(File::create(&archive)?, flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    let mut binaries = vec![
        "ebox".to_string(),
        "enginebox-controller".into(),
        "enginebox-ci-worker".into(),
        "enginebox-supervisor".into(),
        "enginebox-install".into(),
        "enginebox-host".into(),
        "enginebox-api".into(),
        "enginebox-fleet-mcp".into(),
        "enginebox-guest".into(),
        "enginebox-mcp".into(),
        "enginebox-browser".into(),
        "enginebox-agent-clients".into(),
    ];
    binaries.extend(args.binary);
    binaries.sort();
    binaries.dedup();
    for binary in binaries {
        ensure!(
            binary
                .chars()
                .all(|v| v.is_ascii_alphanumeric() || v == '-'),
            "invalid binary name"
        );
        let name = if args.target.contains("windows") {
            format!("{binary}.exe")
        } else {
            binary
        };
        add_file(
            &mut builder,
            &args.bin_dir.join(&name),
            &Path::new("bin").join(name),
            true,
        )?;
    }
    add_file(&mut builder, &args.license, Path::new("LICENSE"), false)?;
    add_file(&mut builder, &args.notice, Path::new("NOTICE"), false)?;
    add_file(
        &mut builder,
        &args.license.with_file_name("LICENSE-APACHE-2.0"),
        Path::new("LICENSE-APACHE-2.0"),
        false,
    )?;
    if let Some(dashboard) = &args.dashboard {
        add_tree(
            &mut builder,
            dashboard,
            Path::new("dashboard"),
            &dashboard.canonicalize()?,
        )?;
        add_file(
            &mut builder,
            args.dashboard_start.as_ref().unwrap(),
            Path::new("dashboard/scripts/start.mjs"),
            false,
        )?;
    }
    let encoder = builder.into_inner()?;
    encoder.finish()?.sync_all()?;
    let manifest = Manifest {
        version: args.version,
        target: args.target,
        archive_sha256: sha256_file(&archive)?,
        state_schema: args.state_schema,
        read_schema_min: args.state_schema,
        read_schema_max: args.state_schema,
        dashboard: args.dashboard.is_some(),
    };
    manifest.validate()?;
    let manifest_name = format!("enginebox-{}-{}.json", manifest.version, manifest.target);
    let manifest_path = staging.path().join(&manifest_name);
    fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;
    fs::write(
        staging.path().join(format!(
            "enginebox-{}-{}.sha256",
            manifest.version, manifest.target
        )),
        format!(
            "{}  {archive_name}\n{}  {manifest_name}\n",
            manifest.archive_sha256,
            sha256_file(&manifest_path)?
        ),
    )?;
    let directory = staging.keep();
    fs::rename(directory, &args.output)?;
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"version":manifest.version,"target":manifest.target,"archive":args.output.join(archive_name),"manifest":args.output.join(manifest_name),"manifest_sha256":sha256_file(&args.output.join(manifest_path.file_name().unwrap()))?,"qualification":"artifact build only; tests and host qualification must precede publication"})
        )?
    );
    Ok(())
}
