use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};
pub fn digest(v: &Value) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(v).expect("JSON serializable"))
    )
}
/// v0.2 warm generations use Python's default ensure_ascii=True. Registry
/// manifests deliberately use UTF-8 instead, so preserve both on migration.
pub fn digest_ascii(v: &Value) -> String {
    let compact = serde_json::to_string(v).expect("JSON serializable");
    let mut ascii = String::with_capacity(compact.len());
    for c in compact.chars() {
        if c.is_ascii() {
            ascii.push(c);
        } else {
            for unit in c.encode_utf16(&mut [0; 2]) {
                ascii.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    format!("{:x}", Sha256::digest(ascii.as_bytes()))
}
pub fn private_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(path)?;
    }
    let m = fs::symlink_metadata(path)?;
    ensure!(
        m.is_dir() && !m.file_type().is_symlink(),
        "State directory must be a real directory"
    );
    check_private(&m)?;
    Ok(())
}
fn check_private(m: &fs::Metadata) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
            "State must be owned by service user and private"
        );
    }
    let _ = m;
    Ok(())
}
fn options() -> OpenOptions {
    #[allow(unused_mut)]
    let mut o = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .mode(0o600);
    }
    o
}
pub fn read_json(path: &Path) -> Result<Value> {
    let mut file = options().read(true).open(path)?;
    let m = file.metadata()?;
    ensure!(
        m.is_file() && m.len() <= 1024 * 1024,
        "Invalid or oversized state record"
    );
    check_private(&m)?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;
    Ok(serde_json::from_slice(&data)?)
}
pub fn write_json(path: &Path, value: &Value) -> Result<()> {
    let parent = path.parent().context("State record needs directory")?;
    private_dir(parent)?;
    let temp = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut f = options().write(true).create_new(true).open(&temp)?;
        serde_json::to_writer(&mut f, value)?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        fs::rename(&temp, path)?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(temp);
    result
}
pub struct StateLock {
    _file: File,
}
impl Drop for StateLock {
    fn drop(&mut self) {
        // A concurrent fork can briefly inherit this file description before
        // exec closes it. Release the scoped lock explicitly so that unrelated
        // children cannot keep a completed worker operation locked.
        let _ = FileExt::unlock(&self._file);
    }
}
pub fn lock(path: &Path, blocking: bool) -> Result<StateLock> {
    let f = options().read(true).write(true).create(true).open(path)?;
    let m = f.metadata()?;
    ensure!(m.is_file(), "Lock must be a regular file");
    check_private(&m)?;
    if blocking {
        FileExt::lock_exclusive(&f)?;
    } else {
        FileExt::try_lock_exclusive(&f).context("Operation is already in progress")?;
    }
    Ok(StateLock { _file: f })
}
pub fn json_files(path: &Path) -> Result<Vec<PathBuf>> {
    let mut files = fs::read_dir(path)?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    files.retain(|p| p.extension().is_some_and(|s| s == "json"));
    files.sort();
    Ok(files)
}
pub fn root_host_lock() -> Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        ensure!(unsafe { libc::geteuid() } == 0, "Host lock requires root");
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open("/run/lock/enginebox-host.lock")?;
        let m = f.metadata()?;
        ensure!(
            m.is_file() && m.uid() == 0 && m.mode() & 0o022 == 0,
            "Unsafe host lock"
        );
        FileExt::try_lock_exclusive(&f)
            .context("Another Engine Box operation holds the host lock")?;
        Ok(f)
    }
    #[cfg(not(unix))]
    {
        anyhow::bail!("Incus host lock requires Linux")
    }
}
