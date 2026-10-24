use crate::model::{ArtifactFile, Entry};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub fn canonical(value: &Value) -> String {
    match value {
        Value::Array(values) => format!(
            "[{}]",
            values.iter().map(canonical).collect::<Vec<_>>().join(",")
        ),
        Value::Object(map) => {
            let sorted: BTreeMap<_, _> = map.iter().collect();
            format!(
                "{{{}}}",
                sorted
                    .into_iter()
                    .map(|(key, value)| format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap(),
                        canonical(value)
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        _ => value.to_string(),
    }
}
pub fn digest(value: &impl Serialize) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(canonical(&serde_json::to_value(value)?).as_bytes())
    ))
}
pub fn hash_string(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 48
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !value.starts_with('-')
        && !value.ends_with('-')
}
pub fn relative(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 1024
            && !value.contains(['\\', ':'])
            && !value.chars().any(char::is_control)
            && value
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."),
        "Unsafe relative file path"
    );
    Ok(())
}
pub fn executable(meta: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        false
    }
}
pub fn permissions(path: &Path, execute: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if execute { 0o700 } else { 0o600 }),
        )?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, execute);
    }
    Ok(())
}
pub fn hash_file(path: &Path, check: &dyn Fn() -> Result<()>) -> Result<(String, u64)> {
    check()?;
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink(),
        "Expected a regular file"
    );
    let mut file = open_regular(path)?;
    let mut hash = Sha256::new();
    let mut count = 0u64;
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        check()?;
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
        count = count
            .checked_add(n as u64)
            .context("File byte count overflow")?;
    }
    ensure!(count == meta.len(), "File changed while hashing");
    Ok((format!("{:x}", hash.finalize()), count))
}
pub fn inventory(
    root: &Path,
    skip: &[&str],
    check: &dyn Fn() -> Result<()>,
) -> Result<Vec<(String, fs::Metadata)>> {
    fn walk(
        root: &Path,
        at: &Path,
        skip: &[&str],
        out: &mut Vec<(String, fs::Metadata)>,
        check: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        check()?;
        for item in fs::read_dir(at)? {
            let item = item?;
            let path = item.path();
            let name = path
                .strip_prefix(root)?
                .to_str()
                .context("Non-UTF8 path")?
                .replace(std::path::MAIN_SEPARATOR, "/");
            relative(&name)?;
            if skip
                .iter()
                .any(|prefix| name == *prefix || name.starts_with(&format!("{prefix}/")))
            {
                continue;
            }
            let meta = fs::symlink_metadata(&path)?;
            ensure!(
                meta.is_dir() || meta.is_file() || meta.file_type().is_symlink(),
                "Special files are not build inputs/artifacts"
            );
            if meta.is_dir() {
                walk(root, &path, skip, out, check)?;
            } else {
                out.push((name, meta));
                ensure!(out.len() <= 100_000, "Source exceeds 100000 entries");
            }
        }
        Ok(())
    }
    let mut out = vec![];
    walk(root, root, skip, &mut out, check)?;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}
pub fn verify_source(root: &Path, entries: &[Entry], check: &dyn Fn() -> Result<()>) -> Result<()> {
    ensure!(entries.len() <= 100_000, "Source exceeds 100000 entries");
    let actual = inventory(root, &[], check)?;
    ensure!(
        actual.len() == entries.len(),
        "Source inventory does not match its manifest"
    );
    let mut portable = BTreeSet::new();
    let mut prior = "";
    for (entry, (name, meta)) in entries.iter().zip(actual.iter()) {
        relative(entry.path())?;
        ensure!(
            entry.path() == name
                && entry.path() > prior
                && portable.insert(entry.path().to_lowercase()),
            "Source entries must be unique, sorted and match disk"
        );
        prior = entry.path();
        let path = root.join(name);
        match entry {
            Entry::File {
                sha256,
                size,
                executable: expected,
                ..
            } => {
                ensure!(
                    hash_string(sha256)
                        && meta.is_file()
                        && *size == meta.len()
                        && executable(meta) == *expected,
                    "Source file metadata mismatch"
                );
                let (actual, bytes) = hash_file(&path, check)?;
                ensure!(
                    actual == *sha256 && bytes == *size,
                    "Source file digest mismatch"
                );
            }
            Entry::Symlink { target, .. } => {
                ensure!(
                    meta.file_type().is_symlink()
                        && !Path::new(target).is_absolute()
                        && !target.contains('\\')
                        && !target.chars().any(char::is_control),
                    "Unsafe source symlink"
                );
                ensure!(
                    fs::read_link(&path)?.to_str() == Some(target),
                    "Source link changed"
                );
                ensure!(
                    fs::canonicalize(&path)?.starts_with(root),
                    "Source symlink escapes its frozen tree"
                );
            }
        }
    }
    Ok(())
}
fn remove(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}
/// Reconcile only an owned scratch tree. Unchanged source files retain mtimes,
/// while the engine's generated cache directory remains in this private namespace.
pub fn materialize(
    root: &Path,
    source: &[Entry],
    scratch: &Path,
    caches: &[&str],
    check: &dyn Fn() -> Result<()>,
) -> Result<()> {
    ensure!(
        !source.iter().any(|e| caches
            .iter()
            .any(|c| e.path() == *c || e.path().starts_with(&format!("{c}/")))),
        "Source must not contain generated engine caches"
    );
    fs::create_dir_all(scratch)?;
    for cache in caches {
        if let Ok(meta) = fs::symlink_metadata(scratch.join(cache)) {
            ensure!(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "Generated cache is not an owned directory"
            );
        }
    }
    let desired: BTreeMap<_, _> = source.iter().map(|e| (e.path(), e)).collect();
    for (name, _) in inventory(scratch, caches, check)? {
        if !desired.contains_key(name.as_str()) {
            remove(&scratch.join(name))?;
        }
    }
    for entry in source {
        check()?;
        let path = scratch.join(entry.path());
        let parent = path.parent().unwrap();
        let mut partial = scratch.to_path_buf();
        for segment in parent.strip_prefix(scratch)?.components() {
            partial.push(segment);
            if let Ok(meta) = fs::symlink_metadata(&partial)
                && (!meta.is_dir() || meta.file_type().is_symlink())
            {
                remove(&partial)?;
            }
            fs::create_dir_all(&partial)?;
        }
        match entry {
            Entry::File {
                sha256,
                size,
                executable: execute,
                ..
            } => {
                if let Ok(meta) = fs::symlink_metadata(&path) {
                    if meta.is_file()
                        && !meta.file_type().is_symlink()
                        && meta.len() == *size
                        && hash_file(&path, check)?.0 == *sha256
                    {
                        permissions(&path, *execute)?;
                        continue;
                    }
                    remove(&path)?;
                }
                let mut from = open_regular(&root.join(entry.path()))?;
                let mut to = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)?;
                let mut buffer = vec![0; 1024 * 1024];
                let mut hash = Sha256::new();
                let mut count = 0u64;
                loop {
                    check()?;
                    let n = from.read(&mut buffer)?;
                    if n == 0 {
                        break;
                    }
                    to.write_all(&buffer[..n])?;
                    hash.update(&buffer[..n]);
                    count += n as u64;
                }
                ensure!(
                    count == *size && format!("{:x}", hash.finalize()) == *sha256,
                    "Source changed while materializing"
                );
                permissions(&path, *execute)?;
            }
            Entry::Symlink { target, .. } => {
                if fs::symlink_metadata(&path).is_ok() {
                    remove(&path)?;
                }
                #[cfg(unix)]
                std::os::unix::fs::symlink(target, &path)?;
                #[cfg(not(unix))]
                anyhow::bail!(
                    "Source symlinks require the qualified Linux/macOS build environment"
                );
            }
        }
    }
    Ok(())
}
pub fn artifact_files(root: &Path, check: &dyn Fn() -> Result<()>) -> Result<Vec<ArtifactFile>> {
    let mut files = vec![];
    for (path, meta) in inventory(root, &[], check)? {
        ensure!(
            meta.is_file() && !meta.file_type().is_symlink(),
            "Artifacts must contain regular files only"
        );
        let (sha256, bytes) = hash_file(&root.join(&path), check)?;
        files.push(ArtifactFile {
            path,
            sha256,
            bytes,
            executable: executable(&meta),
        });
        ensure!(
            files.len() <= 20_000,
            "Artifact exceeds 20000 files; use a protected packaging recipe"
        );
    }
    Ok(files)
}
pub fn absolute(path: &Path) -> Result<PathBuf> {
    ensure!(path.is_absolute(), "Use absolute native paths");
    Ok(fs::canonicalize(path)?)
}

pub fn open_regular(path: &Path) -> Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    ensure!(file.metadata()?.is_file(), "Expected regular file");
    Ok(file)
}
