//! Import a reviewed Git commit, never local Git configuration or working-tree state.
use anyhow::{Context, Result, ensure};
use enginebox_core::transport::run_process;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    path::{Component, Path},
    time::Duration,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preview {
    pub commit: String,
    pub approval: String,
    pub files: Vec<String>,
    pub bytes: u64,
}
pub fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    git_input(repo, args, None)
}
fn git_input(repo: &Path, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
    let repository = fs::canonicalize(repo)?;
    let mut argv = vec![
        "env".into(),
        "-i".into(),
        "PATH=/usr/bin:/bin:/usr/local/bin".into(),
        "HOME=/nonexistent".into(),
        "LC_ALL=C".into(),
        "GIT_CONFIG_NOSYSTEM=1".into(),
        "GIT_CONFIG_GLOBAL=/dev/null".into(),
        "GIT_ATTR_NOSYSTEM=1".into(),
        "GIT_NO_REPLACE_OBJECTS=1".into(),
        "GIT_NO_LAZY_FETCH=1".into(),
        "GIT_TERMINAL_PROMPT=0".into(),
        "git".into(),
        "-c".into(),
        "protocol.allow=never".into(),
        "-c".into(),
        format!(
            "safe.directory={}",
            repository.to_str().context("Repository path")?
        ),
        "-c".into(),
        "core.hooksPath=/dev/null".into(),
        "-c".into(),
        "init.templateDir=".into(),
        "-c".into(),
        "commit.gpgsign=false".into(),
        "-c".into(),
        "user.name=Engine Box".into(),
        "-c".into(),
        "user.email=workspace@example.invalid".into(),
        "-C".into(),
        repo.to_str().context("Repository path")?.into(),
    ];
    argv.extend(args.iter().map(|s| (*s).into()));
    run_process(&argv, input, Duration::from_secs(120))
}
pub fn allowed_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && !path.contains(['\0', '\n', '\r', '\\'])
            && Path::new(path)
                .components()
                .all(|c| matches!(c, Component::Normal(_))),
        "Unsafe Git path"
    );
    for component in path.split('/') {
        let name = component.to_ascii_lowercase();
        ensure!(
            ![
                ".git",
                ".ssh",
                ".aws",
                ".azure",
                ".claude",
                ".codex",
                ".cursor",
                ".npmrc",
                ".pypirc",
                ".netrc",
                "credentials.json",
                "auth.json",
                "id_rsa",
                "id_ed25519"
            ]
            .contains(&name.as_str()),
            "Private/configuration path refused: {path}"
        );
        ensure!(
            !(name == ".env" || name.starts_with(".env."))
                || [".env.example", ".env.sample", ".env.template"].contains(&name.as_str()),
            "Environment secrets path refused: {path}"
        );
    }
    Ok(())
}
pub fn safe_link(path: &str, target: &str) -> Result<()> {
    ensure!(
        !target.is_empty()
            && !target.contains(['\0', '\n', '\r', '\\'])
            && !Path::new(target).is_absolute(),
        "Unsafe symlink target: {path}"
    );
    let mut parts: Vec<&str> = path.split('/').collect();
    parts.pop();
    for p in target.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                ensure!(parts.pop().is_some(), "Symlink escapes checkout: {path}");
            }
            _ => parts.push(p),
        }
    }
    allowed_path(&parts.join("/"))
}
pub fn preview(repo: &Path, revision: &str) -> Result<Preview> {
    ensure!(
        !revision.is_empty() && revision.len() < 512 && !revision.contains(['\0', '\n', '\r']),
        "Invalid revision"
    );
    let commit = String::from_utf8(git(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{revision}^{{commit}}"),
        ],
    )?)?
    .trim()
    .to_owned();
    ensure!(
        [40, 64].contains(&commit.len()) && commit.bytes().all(|b| b.is_ascii_hexdigit()),
        "Invalid commit ID"
    );
    let tree = git(repo, &["ls-tree", "-rlz", "--full-tree", &commit])?;
    let mut files = vec![];
    let mut links = BTreeMap::new();
    let mut bytes = 0u64;
    for record in tree.split(|b| *b == 0).filter(|b| !b.is_empty()) {
        let record = std::str::from_utf8(record)?;
        let (header, path) = record.split_once('\t').context("Invalid Git tree record")?;
        allowed_path(path)?;
        let fields: Vec<_> = header.split_whitespace().collect();
        ensure!(
            fields.len() == 4
                && fields[1] == "blob"
                && ["100644", "100755", "120000"].contains(&fields[0]),
            "Submodules and special files require a separate import: {path}"
        );
        bytes = bytes
            .checked_add(fields[3].parse::<u64>()?)
            .context("Tree size overflow")?;
        if fields[0] == "120000" {
            ensure!(fields[3].parse::<usize>()? <= 4096, "Oversized symlink");
            let target = String::from_utf8(git(repo, &["cat-file", "blob", fields[2]])?)?;
            safe_link(path, &target)?;
            links.insert(path.to_owned(), target);
        }
        files.push(path.to_owned());
    }
    ensure!(!files.is_empty(), "Empty source tree");
    for (path, target) in &links {
        let mut resolved: Vec<String> = path.split('/').map(str::to_owned).collect();
        resolved.pop();
        let mut pending: VecDeque<String> = target.split('/').map(str::to_owned).collect();
        let mut expansions = 0;
        while let Some(part) = pending.pop_front() {
            match part.as_str() {
                "" | "." => {}
                ".." => {
                    ensure!(
                        resolved.pop().is_some(),
                        "Symlink chain escapes checkout: {path}"
                    );
                }
                _ => {
                    resolved.push(part);
                    if let Some(link) = links.get(&resolved.join("/")) {
                        expansions += 1;
                        ensure!(expansions <= 40, "Symlink cycle or excessive chain: {path}");
                        resolved.pop();
                        for part in link.split('/').rev() {
                            pending.push_front(part.into());
                        }
                    }
                }
            }
        }
        if !resolved.is_empty() {
            allowed_path(&resolved.join("/"))?;
        }
    }
    let mut hash = Sha256::new();
    hash.update(b"enginebox-git-import-v1\0");
    hash.update(commit.as_bytes());
    hash.update(&tree);
    Ok(Preview {
        commit,
        approval: format!("{:x}", hash.finalize()),
        files,
        bytes,
    })
}
pub fn stage(repo: &Path, preview: &Preview, directory: &Path) -> Result<()> {
    // Transfer only the approved commit and its tree objects. Preserve its
    // identity in a clean shallow repository without local config, hooks,
    // untracked files, old history or delta bases outside this approved tree.
    ensure!(
        !directory.join("source").exists(),
        "Import staging already exists"
    );
    let source = directory.join("source");
    fs::create_dir(&source)?;
    git(
        &source,
        &[
            "init",
            "-b",
            "main",
            if preview.commit.len() == 64 {
                "--object-format=sha256"
            } else {
                "--object-format=sha1"
            },
        ],
    )?;
    let mut objects = format!("{}\n", preview.commit).into_bytes();
    objects.extend(git(
        repo,
        &[
            "rev-list",
            "--objects",
            "--no-object-names",
            &format!("{}^{{tree}}", preview.commit),
        ],
    )?);
    let pack = source.join(".git/objects/pack/checkout");
    git_input(
        repo,
        &[
            "pack-objects",
            "--no-reuse-delta",
            "--no-reuse-object",
            "--window=0",
            pack.to_str().context("Pack path")?,
        ],
        Some(&objects),
    )?;
    fs::write(source.join(".git/shallow"), format!("{}\n", preview.commit))?;
    git(&source, &["update-ref", "refs/heads/main", &preview.commit])?;
    git(&source, &["reset", "--hard", &preview.commit])?;
    Ok(())
}
