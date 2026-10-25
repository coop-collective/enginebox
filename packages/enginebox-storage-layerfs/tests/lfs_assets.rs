// Source connector commands use the same sanitized Unix environment as github_sync.
#![cfg(unix)]
use enginebox_storage_layerfs::{lfs, source};
use sha2::{Digest, Sha256};
use std::{fs, path::Path};

fn seed(repo: &Path, bytes: &[u8]) -> (String, std::path::PathBuf) {
    source::git(repo, &["init", "-b", "main"]).unwrap();
    let oid = format!("{:x}", Sha256::digest(bytes));
    let pointer = format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}\n",
        bytes.len()
    );
    fs::write(repo.join("art.png"), pointer).unwrap();
    source::git(repo, &["add", "."]).unwrap();
    source::git(repo, &["commit", "-m", "asset"]).unwrap();
    let cache = repo
        .join(".git/lfs/objects")
        .join(&oid[..2])
        .join(&oid[2..4])
        .join(&oid);
    fs::create_dir_all(cache.parent().unwrap()).unwrap();
    fs::write(&cache, bytes).unwrap();
    (oid, cache)
}

#[test]
fn hydration_preserves_asset_bytes_and_git_commit_without_exporting_git() {
    let repo = tempfile::tempdir().unwrap();
    let bytes = b"\x89PNG\0real binary source\xff";
    let (_, cache) = seed(repo.path(), bytes);
    let p = source::preview(repo.path(), "HEAD").unwrap();
    assert_eq!(p.bytes, bytes.len() as u64);
    let staging = tempfile::tempdir().unwrap();
    source::stage_files(repo.path(), &p, staging.path()).unwrap();
    assert_eq!(
        fs::read(staging.path().join("source/art.png")).unwrap(),
        bytes
    );
    assert!(!staging.path().join("source/.git").exists());
    fs::write(&cache, b"tampered").unwrap();
    assert!(source::preview(repo.path(), "HEAD").is_err());
    fs::write(&cache, vec![b'X'; bytes.len()]).unwrap();
    assert!(source::preview(repo.path(), "HEAD").is_err());
    fs::remove_file(cache).unwrap();
    assert!(source::preview(repo.path(), "HEAD").is_err());
}

#[test]
fn malformed_or_extended_pointers_are_refused() {
    assert!(lfs::parse(b"ordinary text").unwrap().is_none());
    for pointer in [
        "version https://git-lfs.github.com/spec/v1\noid sha256:../escape\nsize 1\n",
        "version https://git-lfs.github.com/spec/v1\next-0-custom something\noid sha256:abc\nsize 1\n",
        "version https://git-lfs.github.com/spec/v1\noid sha256:abc\nsize -1\n",
    ] {
        assert!(lfs::parse(pointer.as_bytes()).is_err());
    }
}

#[test]
fn source_changes_keep_assets_in_lfs_instead_of_expanding_git_blobs() {
    let repo = tempfile::tempdir().unwrap();
    source::git(repo.path(), &["init", "-b", "main"]).unwrap();
    fs::write(
        repo.path().join(".gitattributes"),
        "*.png filter=lfs diff=lfs merge=lfs -text\n",
    )
    .unwrap();
    let bytes = b"\x89PNG\0new artwork\xff";
    fs::write(repo.path().join("art.png"), bytes).unwrap();
    source::git(repo.path(), &["add", "."]).unwrap();
    source::git(repo.path(), &["commit", "-m", "artwork"]).unwrap();
    let blob = source::git(repo.path(), &["show", "HEAD:art.png"]).unwrap();
    let pointer = lfs::parse(&blob).unwrap().unwrap();
    assert_eq!(pointer.size, bytes.len() as u64);
    assert_eq!(pointer.oid, format!("{:x}", Sha256::digest(bytes)));
    let p = source::preview(repo.path(), "HEAD").unwrap();
    let staging = tempfile::tempdir().unwrap();
    source::stage_files(repo.path(), &p, staging.path()).unwrap();
    assert_eq!(
        fs::read(staging.path().join("source/art.png")).unwrap(),
        bytes
    );
}

#[cfg(unix)]
#[test]
fn cache_symlinks_are_refused_even_if_target_hash_matches() {
    let repo = tempfile::tempdir().unwrap();
    let data = tempfile::NamedTempFile::new().unwrap();
    let (_, cache) = seed(repo.path(), b"private asset");
    fs::write(data.path(), b"private asset").unwrap();
    fs::remove_file(&cache).unwrap();
    std::os::unix::fs::symlink(data.path(), &cache).unwrap();
    assert!(source::preview(repo.path(), "HEAD").is_err());
}

#[cfg(unix)]
#[test]
fn cache_copy_refuses_symlinked_destination_directories() {
    let repo = tempfile::tempdir().unwrap();
    seed(repo.path(), b"asset bytes");
    let destination = tempfile::tempdir().unwrap();
    source::git(destination.path(), &["init", "-b", "main"]).unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), destination.path().join(".git/lfs")).unwrap();
    assert!(
        enginebox_storage_layerfs::lfs::copy_cache(repo.path(), destination.path(), "HEAD")
            .is_err()
    );
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}
