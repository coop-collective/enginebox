use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub const SDK_DIGEST: &str = "f571de4c12a0201d4daaed5ec86a1472303ca7b88ec73728195ca3be390deb02";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema: u32,
    pub experimental: bool,
    /// One administrator/trust boundary. Never a provider home.
    pub root: PathBuf,
    /// Pinned upstream sdk/js directory, containing the built native addon.
    pub sdk: PathBuf,
    pub daemon: PathBuf,
    pub native_sha256: String,
    pub daemon_sha256: String,
    /// A local immutable Docker image ID, not a mutable tag.
    pub image: String,
    pub max_workspaces: usize,
    pub cpus_per_workspace: u32,
    pub memory_mib_per_workspace: u32,
    pub disk_reserve_gib: u64,
    pub import_limit_gib: u64,
    /// Optional immutable generation. The operator must prohibit other writers.
    pub pnpm_store: Option<String>,
    pub pnpm_metadata: Option<String>,
}
pub fn identifier(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 48
            && value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && value.as_bytes()[0].is_ascii_alphanumeric()
            && value.as_bytes()[value.len() - 1].is_ascii_alphanumeric(),
        "Use a 1–48 character lowercase identifier with digits or internal hyphens"
    );
    Ok(())
}
pub fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
pub fn safe_path(path: &Path) -> Result<()> {
    let s = path.to_str().context("Paths must be UTF-8")?;
    ensure!(
        path.is_absolute()
            && !s.contains([',', '\n', '\r', '\0'])
            && !path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir)),
        "Use an absolute path without parent components, commas or controls"
    );
    Ok(())
}
pub fn real_path(path: &Path) -> Result<()> {
    safe_path(path)?;
    for part in path.ancestors() {
        ensure!(
            !fs::symlink_metadata(part)?.file_type().is_symlink(),
            "Symlink path refused: {}",
            part.display()
        );
    }
    Ok(())
}
impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let value: Self = serde_json::from_slice(&fs::read(path)?)?;
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1 && self.experimental,
            "LayerFS requires schema 1 and explicit experimental=true"
        );
        for p in [&self.root, &self.sdk, &self.daemon] {
            safe_path(p)?;
        }
        ensure!(
            self.root != Path::new("/") && self.root.components().count() >= 3,
            "Choose a dedicated private state directory"
        );
        ensure!(
            self.image.strip_prefix("sha256:").is_some_and(digest),
            "Use the exact local Docker image sha256 ID"
        );
        ensure!(
            digest(&self.native_sha256) && digest(&self.daemon_sha256),
            "Expected artifact SHA-256 hashes required"
        );
        ensure!(
            (1..=64).contains(&self.max_workspaces)
                && (1..=64).contains(&self.cpus_per_workspace)
                && (512..=131072).contains(&self.memory_mib_per_workspace),
            "Invalid workspace resource budget"
        );
        ensure!(
            (1..=1024).contains(&self.disk_reserve_gib)
                && (1..=1024).contains(&self.import_limit_gib),
            "Invalid storage budget"
        );
        ensure!(
            self.pnpm_store.is_some() == self.pnpm_metadata.is_some(),
            "Supply both immutable pnpm store and metadata volumes"
        );
        for name in [&self.pnpm_store, &self.pnpm_metadata]
            .into_iter()
            .flatten()
        {
            identifier(name)?;
        }
        Ok(())
    }
    pub fn verify_artifacts(&self) -> Result<()> {
        real_path(&self.sdk)?;
        real_path(&self.daemon)?;
        fn walk(root: &Path, relative: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
            for e in fs::read_dir(root.join(relative))? {
                let e = e?;
                let p = relative.join(e.file_name());
                let m = fs::symlink_metadata(root.join(&p))?;
                ensure!(!m.file_type().is_symlink(), "SDK symlink refused");
                if m.is_dir() {
                    walk(root, &p, out)?;
                } else {
                    ensure!(m.is_file(), "SDK special file refused");
                    out.push(p);
                }
            }
            Ok(())
        }
        let mut files = vec![PathBuf::from("package.json")];
        walk(&self.sdk, Path::new("src"), &mut files)?;
        files.sort();
        let mut hash = Sha256::new();
        for p in files {
            real_path(&self.sdk.join(&p))?;
            hash.update(p.to_str().context("SDK filename")?.as_bytes());
            hash.update([0]);
            hash.update(Sha256::digest(fs::read(self.sdk.join(p))?));
        }
        ensure!(
            format!("{:x}", hash.finalize()) == SDK_DIGEST,
            "SDK source does not match qualified LayerFS revision {}",
            crate::REVISION
        );
        for (path, expected) in [
            (self.sdk.join("native/layerfs.node"), &self.native_sha256),
            (self.daemon.clone(), &self.daemon_sha256),
        ] {
            real_path(&path)?;
            ensure!(
                format!("{:x}", Sha256::digest(fs::read(&path)?)) == *expected,
                "Artifact hash mismatch: {}",
                path.display()
            );
        }
        Ok(())
    }
}
