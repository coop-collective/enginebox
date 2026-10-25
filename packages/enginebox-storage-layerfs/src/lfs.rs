//! Git LFS assets stay in the private connector cache, never in agent Git state.
use crate::source;
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone)]
pub struct Pointer {
    pub oid: String,
    pub size: u64,
}

pub fn parse(bytes: &[u8]) -> Result<Option<Pointer>> {
    if !bytes.starts_with(b"version https://git-lfs.github.com/spec/v1") {
        return Ok(None);
    }
    let text = std::str::from_utf8(bytes).context("Invalid LFS pointer encoding")?;
    let lines: Vec<_> = text.lines().collect();
    ensure!(
        lines.len() == 3 && lines[0] == "version https://git-lfs.github.com/spec/v1",
        "Unsupported or malformed LFS pointer"
    );
    let oid = lines[1]
        .strip_prefix("oid sha256:")
        .context("LFS requires SHA-256")?;
    ensure!(
        oid.len() == 64
            && oid
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
        "Invalid LFS object identity"
    );
    let size = lines[2]
        .strip_prefix("size ")
        .context("LFS size missing")?
        .parse()?;
    Ok(Some(Pointer {
        oid: oid.into(),
        size,
    }))
}

pub fn objects(repo: &Path, revision: &str) -> Result<Vec<(String, Pointer)>> {
    let tree = source::git(repo, &["ls-tree", "-rlz", "--full-tree", revision])?;
    let mut result = Vec::new();
    for record in tree.split(|b| *b == 0).filter(|b| !b.is_empty()) {
        let (header, path) = std::str::from_utf8(record)?
            .split_once('\t')
            .context("Invalid tree")?;
        let fields: Vec<_> = header.split_whitespace().collect();
        ensure!(fields.len() == 4, "Invalid tree entry");
        if fields[1] == "blob"
            && fields[0] != "120000"
            && fields[3].parse::<u64>()? <= 1024
            && let Some(pointer) = parse(&source::git(repo, &["cat-file", "blob", fields[2]])?)?
        {
            source::allowed_path(path)?;
            result.push((path.into(), pointer));
        }
    }
    Ok(result)
}

fn git_dir(repo: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(
        String::from_utf8(source::git(repo, &["rev-parse", "--absolute-git-dir"])?)?.trim(),
    ))
}

pub fn verified_object(repo: &Path, pointer: &Pointer) -> Result<PathBuf> {
    let mut path = git_dir(repo)?;
    for component in [
        "lfs",
        "objects",
        &pointer.oid[..2],
        &pointer.oid[2..4],
        &pointer.oid,
    ] {
        path.push(component);
        let metadata = fs::symlink_metadata(&path)
            .context("Git LFS asset is not hydrated in the connector cache")?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "Symlink in LFS object cache"
        );
    }
    let metadata = fs::metadata(&path)?;
    ensure!(
        metadata.is_file() && metadata.len() == pointer.size,
        "LFS object size does not match its pointer"
    );
    let mut file = fs::File::open(&path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    ensure!(
        format!("{:x}", hash.finalize()) == pointer.oid,
        "LFS object checksum mismatch"
    );
    Ok(path)
}

pub fn materialize(repo: &Path, revision: &str, destination: &Path) -> Result<()> {
    for (name, pointer) in objects(repo, revision)? {
        let source = verified_object(repo, &pointer)?;
        let target = destination.join(name);
        let metadata = fs::symlink_metadata(&target)?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "LFS target is not a regular source file"
        );
        let permissions = metadata.permissions();
        fs::copy(source, &target)?;
        fs::set_permissions(target, permissions)?;
    }
    Ok(())
}

pub fn copy_cache(from: &Path, to: &Path, revision: &str) -> Result<()> {
    for (_, pointer) in objects(from, revision)? {
        let source = verified_object(from, &pointer)?;
        let mut target = git_dir(to)?;
        crate::config::real_path(&target)?;
        for component in ["lfs", "objects", &pointer.oid[..2], &pointer.oid[2..4]] {
            target.push(component);
            if let Err(error) = fs::create_dir(&target) {
                ensure!(
                    error.kind() == std::io::ErrorKind::AlreadyExists,
                    "Cannot create LFS cache directory: {error}"
                );
            }
            let metadata = fs::symlink_metadata(&target)?;
            ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "Symlink or special directory in destination LFS cache"
            );
        }
        target.push(&pointer.oid);
        match fs::symlink_metadata(&target) {
            Ok(metadata) => ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "LFS cache destination is not a regular file"
            ),
            Err(error) => ensure!(
                error.kind() == std::io::ErrorKind::NotFound,
                "Cannot inspect LFS cache destination"
            ),
        }
        fs::copy(source, target)?;
    }
    Ok(())
}
