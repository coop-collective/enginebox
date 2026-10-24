//! Exact-source engine builds for trusted native Enginebox workers.
//! Engine execution is isolated by the existing native workspace boundary.
pub mod files;
pub mod model;
pub mod recipe;
use anyhow::{Context, Result, ensure};
use enginebox_agents::{process, security};
use model::{BuildResult, LeaseRecord, Manifest, Recipe, Source};
use serde::de::DeserializeOwned;
use serde_json::json;
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub fn read_json<T: DeserializeOwned>(path: &Path, max: u64) -> Result<T> {
    security::check_components(path)?;
    let file = files::open_regular(path)?;
    ensure!(file.metadata()?.len() <= max, "JSON file exceeds limit");
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= max, "JSON file exceeds limit");
    Ok(serde_json::from_slice(&bytes)?)
}
fn now_ms() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}
struct Control {
    path: Option<PathBuf>,
    attempt: Option<String>,
    deadline: Instant,
}
impl Control {
    fn new(path: Option<&Path>, timeout: u64) -> Result<Self> {
        let attempt = path
            .map(|p| read_json::<LeaseRecord>(p, 4096))
            .transpose()?
            .map(|record| record.attempt);
        let control = Self {
            path: path.map(Path::to_path_buf),
            attempt,
            deadline: Instant::now() + Duration::from_secs(timeout),
        };
        control.check()?;
        Ok(control)
    }
    fn check(&self) -> Result<()> {
        ensure!(Instant::now() < self.deadline, "Build timeout");
        if let Some(path) = &self.path {
            let record: LeaseRecord = read_json(path, 4096).context("Cannot verify build lease")?;
            ensure!(
                !record.cancelled
                    && Some(&record.attempt) == self.attempt.as_ref()
                    && !record.attempt.is_empty()
                    && record.expires_at_ms > now_ms()?,
                "Build lease expired, cancelled or replaced"
            );
        }
        Ok(())
    }
}
fn write_sync(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    files::permissions(path, false)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
fn sync_tree_directories(path: &Path) -> Result<()> {
    for item in fs::read_dir(path)? {
        let item = item?;
        let meta = fs::symlink_metadata(item.path())?;
        if meta.is_dir() {
            sync_tree_directories(&item.path())?;
        }
    }
    sync_directory(path)
}
fn result(path: &Path, manifest: &Manifest, hit: bool) -> Result<BuildResult> {
    Ok(BuildResult {
        artifact_id: files::digest(manifest)?,
        manifest_path: path.join("manifest.json"),
        files_root: path.join("files"),
        cache_hit: hit,
    })
}
fn verify_artifact(
    path: &Path,
    expected: &Manifest,
    check: &dyn Fn() -> Result<()>,
) -> Result<Manifest> {
    security::check_components(path)?;
    let manifest: Manifest = read_json(&path.join("manifest.json"), 16 * 1024 * 1024)?;
    let actual = files::artifact_files(&path.join("files"), check)?;
    let mut wanted = expected.clone();
    wanted.files = actual;
    wanted.total_bytes = wanted.files.iter().try_fold(0u64, |sum, f| {
        sum.checked_add(f.bytes).context("Artifact size overflow")
    })?;
    ensure!(
        manifest == wanted && manifest.files.iter().any(|f| f.path == manifest.entrypoint),
        "Cached artifact integrity/binding mismatch"
    );
    check()?;
    Ok(manifest)
}
/// The caller supplies an immutable source export and a protected owner recipe.
/// A lease is mandatory for fleet jobs; standalone local builds can omit it.
pub fn build(
    recipe: &Recipe,
    source: &Source,
    root: &Path,
    state: &Path,
    lease: Option<&Path>,
) -> Result<BuildResult> {
    recipe::validate(recipe)?;
    let control = Control::new(lease, recipe.timeout_seconds)?;
    let check = || control.check();
    ensure!(
        source.schema == 1
            && files::identifier(&source.project)
            && files::hash_string(&source.generation)
            && files::hash_string(&source.tree_hash),
        "Invalid source identity"
    );
    ensure!(
        !source.layerfs.revision.is_empty()
            && source.layerfs.revision.len() <= 128
            && source
                .layerfs
                .revision
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
            && !source.layerfs.project.is_empty()
            && !source.layerfs.reference.is_empty()
            && !source.layerfs.root.is_empty(),
        "Missing frozen LayerFS identity"
    );
    ensure!(
        files::digest(&source.entries)? == source.tree_hash,
        "Source treeHash mismatch"
    );
    security::check_components(root)?;
    let root = files::absolute(root)?;
    ensure!(
        state.is_absolute(),
        "Build state requires an absolute private directory"
    );
    security::private_dir(state)?;
    let state = files::absolute(state)?;
    ensure!(
        !root.starts_with(&state) && !state.starts_with(&root),
        "Source and state directories must be separate"
    );
    files::verify_source(&root, &source.entries, &check)?;
    let (recipe_hash, toolchain) = recipe::fingerprint(recipe, &check)?;
    let namespace = files::digest(
        &json!({"project":source.project,"recipe":recipe_hash,"toolchain":toolchain.fingerprint}),
    )?;
    let key = files::digest(
        &json!({"project":source.project,"generation":source.generation,"sourceTree":source.tree_hash,"recipe":recipe_hash,"toolchain":toolchain.fingerprint}),
    )?;
    let _lease = security::Lease::acquire(&state, &format!("locks/{namespace}.lock"))?;
    let artifacts = state.join("artifacts");
    security::private_dir(&artifacts)?;
    let artifact = artifacts.join(&key);
    let mut manifest = Manifest {
        schema: 1,
        kind: "engine-build".into(),
        project: source.project.clone(),
        generation: source.generation.clone(),
        source_tree: source.tree_hash.clone(),
        recipe: recipe_hash,
        toolchain,
        entrypoint: recipe.entrypoint.clone(),
        files: vec![],
        total_bytes: 0,
    };
    if artifact.exists() {
        let verified = verify_artifact(&artifact, &manifest, &check)?;
        return result(&artifact, &verified, true);
    }
    let work = state.join("work").join(namespace);
    security::private_dir(&work)?;
    let scratch = work.join("source");
    security::private_dir(&scratch)?;
    files::materialize(
        &root,
        &source.entries,
        &scratch,
        recipe::caches(recipe),
        &check,
    )?;
    recipe::prepare(recipe, &scratch)?;
    let stage = artifacts.join(format!(".building-{}", uuid::Uuid::new_v4()));
    security::private_dir(&stage)?;
    let output = stage.join("files");
    security::private_dir(&output)?;
    let complete = (|| -> Result<BuildResult> {
        let entry_parent = output
            .join(&recipe.entrypoint)
            .parent()
            .unwrap()
            .to_path_buf();
        security::private_dir(&entry_parent)?;
        let environment = recipe::environment(recipe, &work)?;
        let commands = recipe::commands(recipe, &scratch, &output)?;
        for (index, argv) in commands.iter().enumerate() {
            check()?;
            let remaining = control.deadline.saturating_duration_since(Instant::now());
            let cancelled = || Ok(check().is_err());
            let execution = process::bounded_controlled(
                argv,
                Some(&environment),
                Some(&scratch),
                &[],
                remaining,
                recipe.max_log_bytes,
                process::Control {
                    lease: Some(&_lease),
                    cancelled: &cancelled,
                },
            )?;
            // Private bounded logs are diagnostic only, never validation evidence.
            let log = work.join(format!("step-{index}.log"));
            if log.exists() {
                fs::remove_file(&log)?;
            }
            write_sync(&log, execution.output.as_bytes())?;
            check()?;
            ensure!(
                execution.exit_code == 0 && !execution.timed_out,
                "Engine build step {index} failed (exit {}, timeout {}); private log: {}",
                execution.exit_code,
                execution.timed_out,
                log.display()
            );
            if recipe.engine == "godot" && index == 0 {
                ensure!(
                    execution
                        .output
                        .lines()
                        .any(|line| line.trim() == recipe.version),
                    "Installed Godot version does not match protected recipe"
                );
            }
        }
        // Rehash protected tools after execution; installer/toolchain drift cannot
        // accidentally publish a build under an earlier cache identity.
        ensure!(
            recipe::fingerprint(recipe, &check)?.0 == manifest.recipe,
            "Recipe/toolchain changed during build"
        );
        manifest.files = files::artifact_files(&output, &check)?;
        ensure!(
            manifest
                .files
                .iter()
                .any(|file| file.path == manifest.entrypoint),
            "Engine did not produce its playable entrypoint"
        );
        if recipe.target == "linux" {
            ensure!(
                manifest
                    .files
                    .iter()
                    .any(|f| f.path == manifest.entrypoint && f.executable),
                "Linux entrypoint must be executable"
            );
        }
        manifest.total_bytes = manifest.files.iter().try_fold(0u64, |sum, file| {
            sum.checked_add(file.bytes)
                .context("Artifact size overflow")
        })?;
        for file in &manifest.files {
            files::open_regular(&output.join(&file.path))?.sync_all()?;
        }
        sync_tree_directories(&output)?;
        write_sync(
            &stage.join("manifest.json"),
            files::canonical(&serde_json::to_value(&manifest)?).as_bytes(),
        )?;
        sync_directory(&stage)?;
        check()?;
        fs::rename(&stage, &artifact)?;
        sync_directory(&artifacts)?;
        // Lease loss after atomic publication is still reported as failure. The
        // immutable cache entry can be reverified by a later authorized attempt.
        check()?;
        result(&artifact, &manifest, false)
    })();
    if stage.exists() {
        let _ = fs::remove_dir_all(stage);
    }
    complete
}
