use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

const MARKER: &str = "enginebox-versioned-install-v1\n";
const MAX_ARCHIVE: u64 = 4 * 1024 * 1024 * 1024;
const MAX_EXPANDED: u64 = 12 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: Version,
    pub target: String,
    pub archive_sha256: String,
    pub state_schema: u32,
    pub read_schema_min: u32,
    pub read_schema_max: u32,
    #[serde(default)]
    pub dashboard: bool,
}
impl Manifest {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version.build.is_empty(),
            "build metadata is not accepted in release pins"
        );
        ensure!(
            self.read_schema_min <= self.state_schema && self.state_schema <= self.read_schema_max,
            "invalid state schema range"
        );
        ensure!(self.state_schema > 0, "state schema must be positive");
        validate_digest(&self.archive_sha256)?;
        ensure!(
            [
                "x86_64-unknown-linux-gnu",
                "aarch64-unknown-linux-gnu",
                "x86_64-apple-darwin",
                "aarch64-apple-darwin",
                "x86_64-pc-windows-msvc",
                "aarch64-pc-windows-msvc"
            ]
            .contains(&self.target.as_str()),
            "unsupported release target"
        );
        Ok(())
    }
    pub fn compatible_with(&self, schema: u32) -> bool {
        self.read_schema_min <= schema
            && schema <= self.read_schema_max
            && self.state_schema == schema
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Activation {
    pub current: Version,
    pub previous: Option<Version>,
    pub state_schema: u32,
}
#[derive(Debug, Clone)]
pub struct Installation {
    pub root: PathBuf,
}
pub struct InstallLock(File);
impl Drop for InstallLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

pub fn current_target() -> &'static str {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "linux") => "x86_64-unknown-linux-gnu",
        ("aarch64", "linux") => "aarch64-unknown-linux-gnu",
        ("x86_64", "macos") => "x86_64-apple-darwin",
        ("aarch64", "macos") => "aarch64-apple-darwin",
        ("x86_64", "windows") => "x86_64-pc-windows-msvc",
        ("aarch64", "windows") => "aarch64-pc-windows-msvc",
        _ => "unsupported",
    }
}
pub fn validate_digest(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "expected a lowercase SHA256 digest"
    );
    Ok(())
}
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        digest.update(&buf[..n]);
    }
    Ok(hex::encode(digest.finalize()))
}
pub fn reject_symlink_components(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "installation paths must be absolute");
    let mut part = PathBuf::new();
    let mut components = path.components().peekable();
    while let Some(c) = components.next() {
        ensure!(
            !matches!(c, Component::ParentDir | Component::CurDir),
            "dot components are not allowed"
        );
        part.push(c);
        // A Windows drive prefix such as \\?\D: is not a complete filesystem
        // path until RootDir is appended. Probe the root and every descendant.
        if matches!(c, Component::Prefix(_)) {
            continue;
        }
        let meta = match fs::symlink_metadata(&part) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect installation path {}", part.display()));
            }
        };
        ensure!(
            !meta.file_type().is_symlink(),
            "symlink in installation path: {}",
            part.display()
        );
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            ensure!(
                meta.file_attributes() & 0x400 == 0,
                "Windows reparse point in installation path: {}",
                part.display()
            );
        }
        ensure!(
            components.peek().is_none() || meta.is_dir(),
            "non-directory ancestor in installation path: {}",
            part.display()
        );
    }
    Ok(())
}
pub fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(windows)]
    {
        // Replace the protected DACL, including existing explicit grants.
        // Pass the path as data in a child-only variable, never shell code.
        let script = r#"$ErrorActionPreference='Stop'; $sid=[System.Security.Principal.WindowsIdentity]::GetCurrent().User; $acl=[System.Security.AccessControl.DirectorySecurity]::new(); $acl.SetAccessRuleProtection($true,$false); $acl.SetOwner($sid); $rule=[System.Security.AccessControl.FileSystemAccessRule]::new($sid,[System.Security.AccessControl.FileSystemRights]::FullControl,[System.Security.AccessControl.InheritanceFlags]'ContainerInherit,ObjectInherit',[System.Security.AccessControl.PropagationFlags]::None,[System.Security.AccessControl.AccessControlType]::Allow); $acl.AddAccessRule($rule); Set-Acl -LiteralPath $env:ENGINEBOX_PRIVATE_DIRECTORY -AclObject $acl"#;
        let result = std::process::Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                script,
            ])
            .env("ENGINEBOX_PRIVATE_DIRECTORY", path)
            .output()?;
        ensure!(
            result.status.success(),
            "could not restrict installation directory ACL"
        );
    }
    Ok(())
}
pub fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("missing parent")?;
    let mut pending = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut pending, value)?;
    pending.write_all(b"\n")?;
    pending.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        pending
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(windows))]
    {
        pending.persist(path).map_err(|e| e.error)?;
        File::open(parent)?.sync_all()?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        };
        let source: Vec<u16> = pending
            .path()
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        let target: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let result = unsafe {
            MoveFileExW(
                source.as_ptr(),
                target.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        ensure!(
            result != 0,
            "atomic replacement failed: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}
impl Installation {
    pub fn inspect(root: PathBuf) -> Result<Self> {
        reject_symlink_components(&root)?;
        ensure!(
            root.parent().is_some(),
            "filesystem root cannot be an installation prefix"
        );
        if root.exists() {
            ensure!(root.is_dir(), "installation prefix is not a directory");
            let marker = root.join(".enginebox-install");
            ensure!(
                !marker.is_symlink() && fs::read_to_string(marker).unwrap_or_default() == MARKER,
                "existing prefix is not owned by this installer"
            );
            for name in [
                "releases",
                "state",
                "etc",
                "logs",
                "current.json",
                "update.lock",
                "supervisor.lock",
            ] {
                reject_symlink_components(&root.join(name))?;
            }
        }
        Ok(Self { root })
    }
    pub fn initialize(&self) -> Result<()> {
        Self::inspect(self.root.clone())?;
        if !self.root.exists() {
            private_dir(&self.root)?;
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.root.join(".enginebox-install"))?;
            f.write_all(MARKER.as_bytes())?;
            f.sync_all()?;
        }
        for name in ["releases", "state", "etc", "logs"] {
            private_dir(&self.root.join(name))?;
        }
        Ok(())
    }
    pub fn lock(&self) -> Result<InstallLock> {
        self.named_lock("update.lock")
    }
    pub fn named_lock(&self, name: &str) -> Result<InstallLock> {
        ensure!(
            ["update.lock", "supervisor.lock"].contains(&name),
            "invalid lock name"
        );
        let path = self.root.join(name);
        reject_symlink_components(&path)?;
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        f.try_lock_exclusive()
            .context("another installation or supervisor operation is in progress")?;
        Ok(InstallLock(f))
    }
    pub fn release_dir(&self, version: &Version) -> PathBuf {
        self.root.join("releases").join(version.to_string())
    }
    pub fn active(&self) -> Result<Option<Activation>> {
        let path = self.root.join("current.json");
        reject_symlink_components(&path)?;
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_reader(File::open(path)?)?))
    }
    pub fn manifest(&self, version: &Version) -> Result<Manifest> {
        let dir = self.release_dir(version);
        reject_symlink_components(&dir)?;
        let path = dir.join("manifest.json");
        reject_symlink_components(&path)?;
        let manifest: Manifest = serde_json::from_reader(File::open(path)?)?;
        manifest.validate()?;
        ensure!(
            manifest.version == *version && manifest.target == current_target(),
            "release version or target mismatch"
        );
        Ok(manifest)
    }
    pub fn activate(&self, version: &Version) -> Result<Activation> {
        let target = self.manifest(version)?;
        let old = self.active()?;
        if let Some(ref active) = old {
            ensure!(
                target.compatible_with(active.state_schema),
                "release cannot read/write current state without a migration; online activation and rollback refused"
            );
        }
        let activation = Activation {
            current: version.clone(),
            previous: old.filter(|v| v.current != *version).map(|v| v.current),
            state_schema: target.state_schema,
        };
        atomic_json(&self.root.join("current.json"), &activation)?;
        Ok(activation)
    }
    /// Caller holds the update lock. The manifest digest must originate from a trusted channel.
    pub fn stage(
        &self,
        version: &Version,
        manifest_path: &Path,
        expected_manifest_hash: &str,
        archive: &Path,
    ) -> Result<Manifest> {
        validate_digest(expected_manifest_hash)?;
        ensure!(
            fs::metadata(manifest_path)?.len() <= 65536,
            "manifest too large"
        );
        ensure!(
            sha256_file(manifest_path)? == expected_manifest_hash,
            "manifest SHA256 mismatch"
        );
        let manifest: Manifest = serde_json::from_reader(File::open(manifest_path)?)?;
        manifest.validate()?;
        ensure!(
            manifest.version == *version,
            "manifest does not match pinned target version"
        );
        ensure!(
            manifest.target == current_target(),
            "release does not match this operating system/architecture"
        );
        ensure!(
            fs::metadata(archive)?.len() <= MAX_ARCHIVE,
            "archive too large"
        );
        ensure!(
            sha256_file(archive)? == manifest.archive_sha256,
            "archive SHA256 mismatch"
        );
        let dest = self.release_dir(version);
        ensure!(
            !dest.exists(),
            "target version already staged; immutable releases cannot be overwritten"
        );
        let staging = tempfile::Builder::new()
            .prefix(".stage-")
            .tempdir_in(self.root.join("releases"))?;
        extract_archive(archive, staging.path())?;
        for bin in [
            "enginebox-controller",
            "enginebox-supervisor",
            "enginebox-install",
            "enginebox-host",
            "ebox",
            "enginebox-api",
            "enginebox-fleet-mcp",
            "enginebox-guest",
            "enginebox-mcp",
            "enginebox-browser",
            "enginebox-agent-clients",
        ] {
            let name = if cfg!(windows) {
                format!("{bin}.exe")
            } else {
                bin.into()
            };
            ensure!(
                staging.path().join("bin").join(name).is_file(),
                "release missing required binary {bin}"
            );
        }
        for name in ["LICENSE", "NOTICE", "LICENSE-APACHE-2.0"] {
            ensure!(
                staging.path().join(name).is_file(),
                "release missing {name}"
            );
        }
        if manifest.dashboard {
            ensure!(
                staging.path().join("dashboard/scripts/start.mjs").is_file(),
                "dashboard entrypoint missing"
            );
        }
        atomic_json(&staging.path().join("manifest.json"), &manifest)?;
        atomic_json(
            &staging.path().join("verification.json"),
            &serde_json::json!({"manifest_sha256":expected_manifest_hash,"archive_sha256":manifest.archive_sha256,"target":manifest.target}),
        )?;
        let staged = staging.keep();
        fs::rename(&staged, &dest).context("publish staged release")?;
        Ok(manifest)
    }
}
fn archive_path(path: &Path) -> Result<PathBuf> {
    ensure!(
        !path.as_os_str().is_empty() && !path.is_absolute(),
        "absolute or empty archive path"
    );
    let mut clean = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::Normal(v) => {
                let text = v.to_str().context("non UTF-8 archive path")?;
                ensure!(
                    !text.contains(['\\', ':'])
                        && !text.chars().any(char::is_control)
                        && !text.ends_with(['.', ' ']),
                    "unsafe archive path"
                );
                let stem = text.split('.').next().unwrap_or("").to_ascii_uppercase();
                ensure!(
                    ![
                        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6",
                        "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6",
                        "LPT7", "LPT8", "LPT9"
                    ]
                    .contains(&stem.as_str()),
                    "reserved archive path"
                );
                clean.push(v);
            }
            _ => bail!("archive path traversal"),
        }
    }
    ensure!(clean.components().count() <= 64, "archive path too deep");
    Ok(clean)
}
pub fn extract_archive(archive: &Path, destination: &Path) -> Result<()> {
    let gzip = flate2::read::GzDecoder::new(File::open(archive)?);
    let mut archive = tar::Archive::new(gzip);
    let mut seen = HashSet::new();
    let mut size = 0u64;
    let mut count = 0usize;
    for entry in archive.entries()? {
        let mut entry = entry?;
        count += 1;
        ensure!(count <= 250_000, "too many archive entries");
        let kind = entry.header().entry_type();
        ensure!(
            kind.is_file() || kind.is_dir(),
            "archive links and special files are forbidden"
        );
        let relative = archive_path(&entry.path()?)?;
        if relative.as_os_str().is_empty() {
            ensure!(kind.is_dir(), "empty file path");
            continue;
        }
        // Case-folding also rejects aliases on case-insensitive Windows/macOS filesystems.
        ensure!(
            seen.insert(relative.to_string_lossy().to_lowercase()),
            "duplicate archive path"
        );
        ensure!(
            !["manifest.json", "verification.json"]
                .iter()
                .any(|p| relative == Path::new(p)),
            "reserved release metadata path"
        );
        let target = destination.join(&relative);
        if kind.is_dir() {
            fs::create_dir_all(target)?;
            continue;
        }
        size = size
            .checked_add(entry.size())
            .context("archive size overflow")?;
        ensure!(size <= MAX_EXPANDED, "expanded archive too large");
        fs::create_dir_all(target.parent().context("missing parent")?)?;
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&target)?;
        std::io::copy(&mut entry, &mut output)?;
        output.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let executable = entry.header().mode()? & 0o111 != 0;
            fs::set_permissions(
                &target,
                fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }),
            )?;
        }
    }
    Ok(())
}
/// HTTPS only, including every redirect hop, bounded response size.
/// The caller independently verifies the trusted digest before reading/executing it.
pub async fn download(url: &str, destination: &Path, maximum: u64) -> Result<()> {
    let url = reqwest::Url::parse(url)?;
    ensure!(
        url.scheme() == "https" && url.username().is_empty() && url.password().is_none(),
        "downloads require credential-free HTTPS URLs"
    );
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 8
                || attempt.url().scheme() != "https"
                || !attempt.url().username().is_empty()
                || attempt.url().password().is_some()
            {
                attempt.error("unsafe download redirect")
            } else {
                attempt.follow()
            }
        }))
        .timeout(std::time::Duration::from_secs(900))
        .build()?;
    let mut response = client.get(url).send().await?.error_for_status()?;
    ensure!(
        !response.status().is_redirection(),
        "download redirects refused; use the final trusted HTTPS URL"
    );
    ensure!(
        response.content_length().unwrap_or(0) <= maximum,
        "download exceeds size limit"
    );
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)?;
    let mut size = 0;
    while let Some(chunk) = response.chunk().await? {
        size += chunk.len() as u64;
        ensure!(size <= maximum, "download exceeds size limit");
        file.write_all(&chunk)?;
    }
    file.sync_all()?;
    Ok(())
}
