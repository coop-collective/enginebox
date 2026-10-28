use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde_json::Value;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path},
};
pub const LIMIT: usize = 65536;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

pub fn check_components(path: &Path) -> Result<()> {
    let mut current = std::path::PathBuf::new();
    for c in path.components() {
        ensure!(!matches!(c, Component::ParentDir), "Unsafe private path.");
        current.push(c);
        // Windows canonical paths start with a verbatim drive or UNC prefix.
        // That prefix alone (for example \\?\C:) is not a filesystem entry;
        // inspect it only after RootDir supplies the trailing separator.
        if matches!(c, Component::Prefix(_)) {
            ensure!(path.has_root(), "Private Windows paths must be rooted.");
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(m) => {
                ensure!(
                    !m.file_type().is_symlink(),
                    "Private paths must not be symbolic links."
                );
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    // Junctions and other reparse points can redirect a private
                    // path even when they are not reported as symbolic links.
                    ensure!(
                        m.file_attributes() & 0x400 == 0,
                        "Private paths must not contain Windows reparse points."
                    );
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("Inspecting private path {}", current.display()));
            }
        }
    }
    Ok(())
}
pub fn private_dir(path: &Path) -> Result<()> {
    check_components(path)?;
    if !path.exists() {
        let mut b = fs::DirBuilder::new();
        b.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            b.mode(0o700);
        }
        b.create(path)?;
    }
    let m = fs::symlink_metadata(path)?;
    ensure!(
        m.is_dir() && !m.file_type().is_symlink(),
        "Invalid private directory."
    );
    #[cfg(unix)]
    ensure!(
        m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
        "Private directory must be owned and mode 0700."
    );
    Ok(())
}
pub fn safe_path(root: &Path, relative: &str, create: bool) -> Result<std::path::PathBuf> {
    ensure!(
        !Path::new(relative).is_absolute() && !relative.is_empty(),
        "Invalid private relative path."
    );
    ensure!(
        Path::new(relative)
            .components()
            .all(|c| matches!(c, Component::Normal(_))),
        "Invalid private relative path."
    );
    check_components(root)?;
    let path = root.join(relative);
    check_components(&path)?;
    if create {
        let mut current = root.to_path_buf();
        for c in Path::new(relative)
            .parent()
            .unwrap_or(Path::new(""))
            .components()
        {
            current.push(c);
            if !current.exists() {
                private_dir(&current)?;
            } else {
                let m = fs::symlink_metadata(&current)?;
                ensure!(
                    m.is_dir() && !m.file_type().is_symlink(),
                    "Unsafe native directory."
                );
                #[cfg(unix)]
                ensure!(
                    m.uid() == unsafe { libc::geteuid() },
                    "Native directory owner mismatch."
                );
            }
        }
    }
    Ok(path)
}
pub fn read_bytes(
    root: &Path,
    relative: &str,
    limit: usize,
    strict: bool,
) -> Result<Option<Vec<u8>>> {
    #[cfg(not(unix))]
    let _ = strict;
    let path = safe_path(root, relative, false)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let mut file = match options.open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let m = file.metadata()?;
    ensure!(
        m.is_file() && m.len() <= limit as u64,
        "Invalid private file."
    );
    #[cfg(unix)]
    {
        ensure!(
            m.uid() == unsafe { libc::geteuid() } && m.nlink() == 1,
            "Private file owner/link mismatch."
        );
        if strict {
            ensure!(m.mode() & 0o077 == 0, "Private file must have mode 0600.");
        }
    }
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "Private file exceeds limit.");
    Ok(Some(bytes))
}
pub fn read_json(root: &Path, relative: &str) -> Result<Option<Value>> {
    read_bytes(root, relative, LIMIT, false)?
        .map(|b| {
            let v: Value = serde_json::from_slice(&b)?;
            ensure!(v.is_object(), "Invalid native credential file.");
            Ok(v)
        })
        .transpose()
}
pub fn write_bytes(root: &Path, relative: &str, raw: &[u8], limit: usize) -> Result<()> {
    ensure!(raw.len() <= limit, "Private file exceeds limit.");
    let path = safe_path(root, relative, true)?;
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    #[cfg(unix)]
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(raw)?;
    file.as_file().sync_all()?;
    file.persist(&path).map_err(|e| e.error)?;
    #[cfg(unix)]
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}
pub fn write_json(root: &Path, relative: &str, value: &Value) -> Result<()> {
    write_bytes(root, relative, &serde_json::to_vec(value)?, LIMIT)
}
pub fn token(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|s| (8..=16384).contains(&s.len()) && !s.chars().any(|c| c as u32 <= 32))
}
pub fn digest(v: &Value) -> String {
    use sha2::{Digest, Sha256};
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(v).unwrap_or_default())
    )
}
// Python v0.2 receipts hashed json.dumps(sort_keys=True), including its separators.
pub fn legacy_digest(v: &Value) -> String {
    use sha2::{Digest, Sha256};
    fn quote(value: &str) -> String {
        let json = serde_json::to_string(value).unwrap();
        let mut output = String::new();
        for character in json.chars() {
            if character.is_ascii() {
                output.push(character);
            } else {
                for unit in character.encode_utf16(&mut [0; 2]) {
                    output.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
        output
    }
    fn encode(v: &Value) -> String {
        match v {
            Value::Array(a) => format!("[{}]", a.iter().map(encode).collect::<Vec<_>>().join(", ")),
            Value::Object(o) => format!(
                "{{{}}}",
                o.iter()
                    .collect::<std::collections::BTreeMap<_, _>>()
                    .into_iter()
                    .map(|(k, v)| format!("{}: {}", quote(k), encode(v)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Value::String(value) => quote(value),
            _ => v.to_string(),
        }
    }
    format!("{:x}", Sha256::digest(encode(v)))
}
pub struct Lease {
    file: File,
}
impl Lease {
    pub fn acquire(root: &Path, relative: &str) -> Result<Self> {
        let path = safe_path(root, relative, true)?;
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create(true);
        #[cfg(unix)]
        opts.mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let f = opts.open(path)?;
        let m = f.metadata()?;
        ensure!(m.is_file(), "Invalid private lease.");
        #[cfg(unix)]
        ensure!(
            m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0 && m.nlink() == 1,
            "Unsafe private lease."
        );
        f.try_lock_exclusive()
            .map_err(|_| anyhow::anyhow!("Native provider already has an active operation."))?;
        Ok(Self { file: f })
    }
    /// Record a use of the leased resource in the lease file's mtime.
    pub fn touch(&self) -> Result<()> {
        self.file.set_modified(std::time::SystemTime::now())?;
        Ok(())
    }
    #[cfg(unix)]
    pub fn fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.file.as_raw_fd()
    }
    fn release_probe(self) -> Result<()> {
        // This descriptor was only acquired to observe availability, never to
        // protect a native writer. A concurrent fork can copy it before exec;
        // explicitly unlock the shared probe so that copy cannot manufacture
        // a busy lease after the observation returned false.
        FileExt::unlock(&self.file)?;
        Ok(())
    }
    pub fn active(root: &Path, relative: &str) -> Result<bool> {
        let path = safe_path(root, relative, false)?;
        if !path.exists() {
            return Ok(false);
        };
        match Self::acquire(root, relative) {
            Ok(probe) => {
                probe.release_probe()?;
                Ok(false)
            }
            Err(e) if e.to_string().contains("active operation") => Ok(true),
            Err(e) => Err(e),
        }
    }
}
// Writer leases must not explicitly unlock on Drop: inherited descriptors
// share this flock. Closing the parent leaves a surviving native child holding
// its writer lease until it exits. Only completed observational probes unlock.
pub fn remove(root: &Path, relative: &str) -> Result<()> {
    let path = safe_path(root, relative, false)?;
    if read_bytes(root, relative, LIMIT, false)?.is_some() {
        fs::remove_file(path)?;
    }
    Ok(())
}
pub fn exact_uuid(value: &str) -> bool {
    regex::Regex::new(
        r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$",
    )
    .unwrap()
    .is_match(value)
}
pub fn absolute_existing(path: &Path) -> Result<std::path::PathBuf> {
    ensure!(path.is_absolute(), "Path must be absolute.");
    check_components(path)?;
    if !path.is_dir() {
        bail!("Path must be an existing directory.")
    }
    Ok(path.canonicalize()?)
}

#[cfg(all(test, unix))]
mod lease_tests {
    use super::*;
    use std::{
        os::{
            fd::AsRawFd,
            unix::{net::UnixStream, process::CommandExt},
        },
        time::Duration,
    };

    #[test]
    fn an_observation_does_not_leave_a_lock_in_an_unrelated_pre_exec_child() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        private_dir(&root).unwrap();
        let probe = Lease::acquire(&root, "probe.lock").unwrap();
        let (mut ready, child_ready) = UnixStream::pair().unwrap();
        let (mut release, child_release) = UnixStream::pair().unwrap();
        ready
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let spawning = std::thread::spawn(move || {
            let ready_fd = child_ready.as_raw_fd();
            let release_fd = child_release.as_raw_fd();
            let mut command =
                crate::process::command(&["/bin/sleep".into(), "5".into()], None, None).unwrap();
            unsafe {
                command.pre_exec(move || {
                    let mut byte = 1u8;
                    if libc::write(ready_fd, (&byte as *const u8).cast(), 1) != 1
                        || libc::read(release_fd, (&mut byte as *mut u8).cast(), 1) != 1
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            command.spawn().unwrap()
        });
        // Freeze a concurrent fork before exec, while it still has the probe's
        // CLOEXEC descriptor. No native writer was launched with this lease.
        ready.read_exact(&mut [0]).unwrap();
        probe.release_probe().unwrap();
        let falsely_active = Lease::active(&root, "probe.lock").unwrap();
        release.write_all(&[1]).unwrap();
        let mut child = spawning.join().unwrap();
        crate::process::stop(&mut child);
        assert!(
            !falsely_active,
            "a completed observation must not create a native writer"
        );
    }
}
