//! GitHub is an optional source adapter. Initial import creates one LayerFS
//! project. Subsequent main revisions stay in its native branch/history system.
use crate::{config::identifier, source};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use enginebox_core::{state, transport::run_isolated_process};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

pub fn repository(value: &str) -> Result<String> {
    let value = value.strip_prefix("https://github.com/").unwrap_or(value);
    let value = value.strip_suffix(".git").unwrap_or(value);
    let (owner, name) = value
        .split_once('/')
        .context("Use owner/repository or its GitHub HTTPS URL")?;
    ensure!(
        !owner.is_empty()
            && owner.len() <= 39
            && !owner.starts_with('-')
            && !owner.ends_with('-')
            && owner
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !name.is_empty()
            && name.len() <= 100
            && ![".", ".."].contains(&name)
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
        "Invalid GitHub repository; credentials, query strings and non-GitHub hosts are refused"
    );
    Ok(format!("{owner}/{name}").to_ascii_lowercase())
}
pub fn branch(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 200
            && !value.starts_with('-')
            && !value.contains("..")
            && !value.contains("@{")
            && value.split('/').all(|p| !p.is_empty()
                && !p.starts_with('.')
                && !p.ends_with('.')
                && !p.ends_with(".lock"))
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/_-.".contains(&b)),
        "Use a branch name with letters, digits, slashes, dots, underscores or hyphens"
    );
    Ok(())
}
fn commit(value: &str) -> bool {
    [40, 64].contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Generation {
    pub project: String,
    pub commit: String,
    pub approval: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct Candidate {
    commit: String,
    approval: String,
    source_approval: String,
    previous: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Repository {
    schema: u32,
    project: String,
    repository: String,
    branch: String,
    current: Option<Generation>,
    generations: Vec<Generation>,
    candidate: Option<Candidate>,
}
/// This directory contains only controller-owned fetch caches and small receipts.
/// No token is ever written here or passed into the LayerFS importer.
pub struct Repositories {
    root: PathBuf,
}
impl Repositories {
    pub fn new(storage_root: &Path) -> Result<Self> {
        let root = storage_root.join("github");
        state::private_dir(&root)?;
        Ok(Self { root })
    }
    fn directory(&self, project: &str) -> Result<PathBuf> {
        identifier(project)?;
        Ok(self.root.join(project))
    }
    fn read(&self, project: &str) -> Result<Repository> {
        let r: Repository = serde_json::from_value(state::read_json(
            &self.directory(project)?.join("repository.json"),
        )?)?;
        ensure!(
            r.schema == 1 && r.project == project && repository(&r.repository)? == r.repository,
            "Invalid GitHub receipt"
        );
        branch(&r.branch)?;
        for g in &r.generations {
            identifier(&g.project)?;
            ensure!(
                commit(&g.commit) && crate::config::digest(&g.approval),
                "Invalid GitHub generation receipt"
            );
        }
        ensure!(
            r.current
                .as_ref()
                .is_none_or(|c| r.generations.iter().any(|g| g.project == c.project
                    && g.commit == c.commit
                    && g.approval == c.approval)),
            "Invalid published generation"
        );
        Ok(r)
    }
    fn save(&self, r: &Repository) -> Result<()> {
        state::write_json(
            &self.directory(&r.project)?.join("repository.json"),
            &json!(r),
        )
    }
    pub fn list(&self) -> Result<Vec<Value>> {
        let mut result = vec![];
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() && entry.path().join("repository.json").exists() {
                let r = self.read(
                    entry
                        .file_name()
                        .to_str()
                        .context("Invalid project filename")?,
                )?;
                result.push(json!({"id":r.project,"repository":r.repository,"branch":r.branch,"current":r.current,"generations":r.generations}));
            }
        }
        result.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        Ok(result)
    }
    pub fn current(&self, project: &str) -> Result<Generation> {
        self.read(project)?
            .current
            .context("Import a reviewed GitHub revision first")
    }
    pub fn preview(
        &self,
        project: &str,
        repo: &str,
        requested_branch: Option<&str>,
        token_file: Option<&Path>,
        max_bytes: u64,
    ) -> Result<Value> {
        self.preview_with(
            project,
            repo,
            requested_branch,
            max_bytes,
            |cache, repo, branch| {
                let selected = fetch(cache, repo, branch, token_file)?;
                hydrate_lfs(cache, repo, "refs/heads/source", token_file, max_bytes)?;
                Ok(selected)
            },
        )
    }
    fn preview_with(
        &self,
        project: &str,
        repo: &str,
        requested_branch: Option<&str>,
        max_bytes: u64,
        fetcher: impl FnOnce(&Path, &str, Option<&str>) -> Result<String>,
    ) -> Result<Value> {
        let repo = repository(repo)?;
        if let Some(b) = requested_branch {
            branch(b)?;
        }
        let directory = self.directory(project)?;
        // Serialize directory enrollment, then release before potentially slow network IO.
        let registry_lock = state::lock(&self.root.join(".lock"), false)?;
        if !directory.exists() {
            ensure!(
                fs::read_dir(&self.root)?
                    .filter_map(Result::ok)
                    .filter(|e| e.path().is_dir())
                    .count()
                    < 1024,
                "GitHub project limit reached"
            );
            state::private_dir(&directory)?;
        }
        let _lease = state::lock(&directory.join(".lock"), false)?;
        drop(registry_lock);
        let mut existing = if directory.join("repository.json").exists() {
            Some(self.read(project)?)
        } else {
            None
        };
        if let Some(r) = &existing {
            ensure!(
                r.repository == repo && requested_branch.is_none_or(|b| b == r.branch),
                "This project is bound to another repository or branch; use a new project ID"
            );
        }
        let cache = directory.join("repository.git");
        if !cache.exists() {
            state::private_dir(&cache)?;
            source::git(&cache, &["init", "--bare"])?;
        }
        let selected = requested_branch.or_else(|| existing.as_ref().map(|r| r.branch.as_str()));
        let branch_name = fetcher(&cache, &repo, selected)?;
        branch(&branch_name)?;
        ensure!(
            selected.is_none_or(|b| b == branch_name),
            "Fetched branch does not match request"
        );
        let preview = source::preview(&cache, "refs/heads/source")?;
        ensure!(preview.bytes <= max_bytes, "Source exceeds import limit");
        let mut r = existing.take().unwrap_or(Repository {
            schema: 1,
            project: project.into(),
            repository: repo,
            branch: branch_name,
            current: None,
            generations: vec![],
            candidate: None,
        });
        let previous = r.current.as_ref().map(|g| g.project.clone());
        let approval = state::digest(
            &json!({"kind":"enginebox-github-review-v1","project":project,"repository":r.repository,"branch":r.branch,"commit":preview.commit,"source_approval":preview.approval,"previous":previous}),
        );
        r.candidate = Some(Candidate {
            commit: preview.commit.clone(),
            approval: approval.clone(),
            source_approval: preview.approval,
            previous,
        });
        self.save(&r)?;
        Ok(
            json!({"project":project,"repository":r.repository,"branch":r.branch,"commit":preview.commit,"approval":approval,"files":preview.files,"bytes":preview.bytes,"current":r.current,"up_to_date":r.current.as_ref().is_some_and(|g| g.commit == preview.commit)}),
        )
    }
    /// Publish only after the storage adapter proves a complete import. A failed
    /// write retains the old pointer; retry can adopt a proven ready generation.
    pub fn import(
        &self,
        project: &str,
        revision: &str,
        approval: &str,
        publish: impl FnOnce(&str, &Path, &str, &str) -> Result<()>,
    ) -> Result<Value> {
        ensure!(
            commit(revision) && crate::config::digest(approval),
            "Exact commit and approval digest required"
        );
        let directory = self.directory(project)?;
        let _lease = state::lock(&directory.join(".lock"), false)?;
        let mut r = self.read(project)?;
        let candidate = r
            .candidate
            .clone()
            .context("Check GitHub for a revision to review first")?;
        ensure!(
            candidate.commit == revision && candidate.approval == approval,
            "Review changed; check for updates and approve again"
        );
        if r.current.as_ref().is_some_and(|g| g.commit == revision) {
            return Ok(json!({"ok":true,"unchanged":true,"project":project,"current":r.current}));
        }
        ensure!(
            r.current.is_none(),
            "LayerFS owns this project now; reconcile upstream changes through the outbound sync workflow instead of replacing its history"
        );
        ensure!(
            r.current.as_ref().map(|g| &g.project) == candidate.previous.as_ref(),
            "Published generation changed; review again"
        );
        ensure!(
            r.generations.len() < 1024,
            "Retained generation limit reached; administrator inspection required"
        );
        let cache = directory.join("repository.git");
        let preview = source::preview(&cache, revision)?;
        ensure!(
            preview.approval == candidate.source_approval,
            "Fetched content no longer matches review"
        );
        let hash = state::digest(&json!([
            "enginebox-github-generation-v1",
            project,
            r.repository,
            r.branch,
            revision,
            candidate.source_approval
        ]));
        let generation = Generation {
            project: format!("gh-{}", &hash[..40]),
            commit: revision.into(),
            approval: candidate.source_approval,
        };
        publish(&generation.project, &cache, revision, &generation.approval)?;
        if !r
            .generations
            .iter()
            .any(|g| g.project == generation.project)
        {
            r.generations.push(generation.clone());
        }
        r.current = Some(generation);
        self.save(&r)?;
        Ok(json!({"ok":true,"unchanged":false,"project":project,"current":r.current}))
    }
}
fn network_git(cache: &Path, token_file: Option<&Path>, args: &[&str]) -> Result<Vec<u8>> {
    let token = token_file
        .map(|path| -> Result<String> {
            let value =
                state::read_json(path).context("Cannot read private GitHub credential file")?;
            let token = value["token"]
                .as_str()
                .context("GitHub credential file needs a token string")?;
            ensure!(
                !token.is_empty()
                    && token.len() <= 16384
                    && token
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)),
                "Invalid GitHub token format"
            );
            Ok(token.to_owned())
        })
        .transpose()?;
    let header = token.map(|token| {
        format!(
            "Authorization: Basic {}",
            STANDARD.encode(format!("x-access-token:{token}"))
        )
    });

    let mut env = vec![
        ("PATH", "/usr/bin:/bin:/usr/local/bin"),
        ("HOME", "/nonexistent"),
        ("LC_ALL", "C"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_NO_REPLACE_OBJECTS", "1"),
    ];
    if let Some(header) = &header {
        env.extend([
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "http.https://github.com/.extraHeader"),
            ("GIT_CONFIG_VALUE_0", header),
        ]);
    }
    let mut argv: Vec<String> = vec![
        "git".into(),
        "-C".into(),
        cache
            .to_str()
            .context("GitHub cache path must be UTF-8")?
            .into(),
    ];
    for c in [
        "protocol.allow=never",
        "protocol.https.allow=always",
        "credential.helper=",
        "core.hooksPath=/dev/null",
        "http.followRedirects=false",
        "http.sslVerify=true",
        "http.lowSpeedLimit=1024",
        "http.lowSpeedTime=30",
        "fetch.fsckObjects=true",
        "fetch.recurseSubmodules=false",
        "gc.auto=0",
        "maintenance.auto=false",
    ] {
        argv.extend(["-c".into(), c.into()]);
    }
    argv.extend(args.iter().map(|s| (*s).into()));
    // Remote stderr is deliberately discarded: it can contain credentials or
    // server-controlled text. Never chain it into durable jobs or receipts.
    run_isolated_process(&argv,None,Duration::from_secs(300),&env)
            .map_err(|_| anyhow::anyhow!("GitHub transfer failed or timed out; check the repository, branch, node network and configured repository credential"))
}
fn fetch(
    cache: &Path,
    repo: &str,
    selected: Option<&str>,
    token_file: Option<&Path>,
) -> Result<String> {
    let url = format!("https://github.com/{repo}.git");
    let remote = |args: &[&str]| network_git(cache, token_file, args);
    let selected = match selected {
        Some(b) => b.to_owned(),
        None => {
            let output = String::from_utf8(remote(&["ls-remote", "--symref", &url, "HEAD"])?)?;
            output
                .lines()
                .find_map(|line| {
                    line.strip_prefix("ref: refs/heads/")
                        .and_then(|s| s.strip_suffix("\tHEAD"))
                })
                .context("GitHub default branch is missing; empty repositories cannot be imported")?
                .to_owned()
        }
    };
    branch(&selected)?;
    remote(&[
        "fetch",
        "--depth=1",
        "--no-tags",
        "--no-recurse-submodules",
        "--no-write-fetch-head",
        &url,
        &format!("+refs/heads/{selected}:refs/heads/source"),
    ])?;
    Ok(selected)
}

fn lfs_transfer(
    cache: &Path,
    repository: &str,
    token_file: Option<&Path>,
    arguments: &[&str],
) -> Result<()> {
    // Override repository-supplied endpoints and transfer adapters. The ordinary
    // GitHub authorization header remains scoped to github.com, not object URLs.
    let endpoint = format!("https://github.com/{repository}.git/info/lfs");
    let settings = [
        format!("lfs.url={endpoint}"),
        format!("lfs.pushurl={endpoint}"),
    ];
    let mut args = vec!["-c", settings[0].as_str(), "-c", settings[1].as_str()];
    for setting in [
        "lfs.storage=lfs",
        "lfs.basictransfersonly=true",
        "lfs.fetchrecentalways=false",
        "lfs.fetchrecentrefsdays=0",
        "lfs.fetchrecentcommitsdays=0",
        "lfs.remote.autodetect=false",
        "lfs.remote.searchall=false",
        "lfs.concurrenttransfers=4",
    ] {
        args.extend(["-c", setting]);
    }
    args.push("lfs");
    args.extend_from_slice(arguments);
    network_git(cache, token_file, &args)?;
    Ok(())
}

fn hydrate_lfs(
    cache: &Path,
    repository: &str,
    revision: &str,
    token_file: Option<&Path>,
    max_bytes: u64,
) -> Result<()> {
    let preview = source::preview_before_hydration(cache, revision)?;
    ensure!(
        preview.bytes <= max_bytes,
        "Hydrated source exceeds import limit"
    );
    let objects = crate::lfs::objects(cache, revision)?;
    if objects.is_empty() {
        return Ok(());
    }
    if objects
        .iter()
        .any(|(_, p)| crate::lfs::verified_object(cache, p).is_err())
    {
        let url = format!("https://github.com/{repository}.git");
        lfs_transfer(
            cache,
            repository,
            token_file,
            &["fetch", "--include=", "--exclude=", &url, &preview.commit],
        )?;
    }
    for (_, pointer) in objects {
        crate::lfs::verified_object(cache, &pointer)?;
    }
    Ok(())
}

/// Repository and branch are normalized at construction; callers cannot choose
/// transport schemes, credentials, arbitrary URLs or refspecs.
pub struct GitHubRemote {
    repository: String,
    branch: String,
    credential: Option<PathBuf>,
    control: PathBuf,
}
impl GitHubRemote {
    pub fn new(
        repository_name: &str,
        branch_name: &str,
        credential: Option<&Path>,
        control: &Path,
    ) -> Result<Self> {
        branch(branch_name)?;
        Ok(Self {
            repository: repository(repository_name)?,
            branch: branch_name.into(),
            credential: credential.map(Path::to_path_buf),
            control: control.into(),
        })
    }
    fn url(&self) -> String {
        format!("https://github.com/{}.git", self.repository)
    }
}
impl crate::sync::Remote for GitHubRemote {
    fn head(&self) -> Result<String> {
        let bytes = network_git(
            &self.control,
            self.credential.as_deref(),
            &[
                "ls-remote",
                "--exit-code",
                &self.url(),
                &format!("refs/heads/{}", self.branch),
            ],
        )?;
        let text = String::from_utf8(bytes)?;
        let (sha, reference) = text
            .trim()
            .split_once('\t')
            .context("GitHub branch not found")?;
        ensure!(
            commit(sha) && reference == format!("refs/heads/{}", self.branch),
            "Invalid GitHub branch head"
        );
        Ok(sha.into())
    }
    fn fetch(&self, repo: &Path, sha: &str, reference: &str) -> Result<()> {
        ensure!(
            commit(sha) && ["refs/heads/base", "refs/heads/upstream"].contains(&reference),
            "Invalid sync fetch"
        );
        if source::git(
            &self.control,
            &["cat-file", "-e", &format!("{sha}^{{commit}}")],
        )
        .is_ok()
        {
            source::git(
                repo,
                &[
                    "-c",
                    "protocol.file.allow=always",
                    "fetch",
                    "--depth=1",
                    "--update-shallow",
                    "--no-tags",
                    self.control.to_str().context("Cache path")?,
                    &format!("{sha}:{reference}"),
                ],
            )?;
            crate::lfs::copy_cache(&self.control, repo, sha)?;
            return Ok(());
        }
        network_git(
            repo,
            self.credential.as_deref(),
            &[
                "fetch",
                "--depth=1",
                "--no-tags",
                "--no-recurse-submodules",
                &self.url(),
                &format!("{sha}:{reference}"),
            ],
        )?;
        hydrate_lfs(
            repo,
            &self.repository,
            sha,
            self.credential.as_deref(),
            1024 * 1024 * 1024,
        )?;
        Ok(())
    }
    fn remember(&self, repo: &Path, sha: &str) -> Result<()> {
        ensure!(commit(sha), "Invalid remembered commit");
        source::git(
            &self.control,
            &[
                "-c",
                "protocol.file.allow=always",
                "fetch",
                "--depth=1",
                "--update-shallow",
                "--no-tags",
                repo.to_str().context("Environment path")?,
                &format!("+{sha}:refs/enginebox/synced"),
            ],
        )?;
        crate::lfs::copy_cache(repo, &self.control, sha)?;
        Ok(())
    }
    fn push(&self, repo: &Path, expected: &str, sha: &str) -> Result<()> {
        ensure!(commit(expected) && commit(sha), "Invalid sync push");
        // The only published object is a commit directly parented by the exact
        // observed remote head. The lease adds an atomic remote race check.
        ensure!(
            String::from_utf8(source::git(repo, &["rev-parse", &format!("{sha}^")])?)?.trim()
                == expected,
            "Sync commit must extend the observed GitHub head"
        );
        let objects = crate::lfs::objects(repo, sha)?;
        for (_, pointer) in &objects {
            crate::lfs::verified_object(repo, pointer)?;
        }
        for chunk in objects.chunks(64) {
            let url = self.url();
            let mut args = vec!["push", "--object-id", url.as_str()];
            args.extend(chunk.iter().map(|(_, p)| p.oid.as_str()));
            lfs_transfer(repo, &self.repository, self.credential.as_deref(), &args)?;
        }
        network_git(
            repo,
            self.credential.as_deref(),
            &[
                "push",
                "--porcelain",
                &format!("--force-with-lease=refs/heads/{}:{expected}", self.branch),
                &self.url(),
                &format!("{sha}:refs/heads/{}", self.branch),
            ],
        )?;
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    fn seed(repo: &Path) {
        source::git(repo, &["init", "-b", "main"]).unwrap();
        fs::write(repo.join("hello.txt"), "hello\n").unwrap();
        source::git(repo, &["add", "."]).unwrap();
        source::git(repo, &["commit", "-m", "initial"]).unwrap();
    }
    fn preview(repos: &Repositories, source: &Path) -> Value {
        repos
            .preview_with("app", "Owner/App", None, 1_000_000, |cache, _, branch| {
                assert!(branch.is_none_or(|b| b == "main"));
                source::git(
                    cache,
                    &[
                        "-c",
                        "protocol.file.allow=always",
                        "fetch",
                        source.to_str().unwrap(),
                        "+refs/heads/main:refs/heads/source",
                    ],
                )?;
                Ok("main".into())
            })
            .unwrap()
    }
    #[test]
    fn import_is_review_bound_atomic_idempotent_and_cannot_replace_layerfs_main() {
        let root = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        seed(source.path());
        let repos = Repositories::new(root.path()).unwrap();
        let p = preview(&repos, source.path());
        let sha = p["commit"].as_str().unwrap();
        let approval = p["approval"].as_str().unwrap();
        assert!(
            repos
                .import("app", sha, &"0".repeat(64), |_, _, _, _| panic!(
                    "No import without exact review"
                ))
                .is_err()
        );
        assert!(
            repos
                .import("app", sha, approval, |_, _, _, _| anyhow::bail!(
                    "Storage failure"
                ))
                .is_err()
        );
        assert!(repos.current("app").is_err());
        let first = repos
            .import("app", sha, approval, |_, _, _, _| Ok(()))
            .unwrap();
        assert_eq!(first["unchanged"], false);
        let again = repos
            .import("app", sha, approval, |_, _, _, _| {
                panic!("Import must not repeat")
            })
            .unwrap();
        assert_eq!(again["unchanged"], true);
        fs::write(source.path().join("hello.txt"), "next").unwrap();
        source::git(source.path(), &["add", "."]).unwrap();
        source::git(source.path(), &["commit", "-m", "next"]).unwrap();
        let next = preview(&repos, source.path());
        assert!(
            repos
                .import(
                    "app",
                    next["commit"].as_str().unwrap(),
                    next["approval"].as_str().unwrap(),
                    |_, _, _, _| panic!("LayerFS main cannot be replaced")
                )
                .is_err()
        );
        assert_eq!(repos.current("app").unwrap().commit, sha);
    }
    #[test]
    #[ignore = "read-only GitHub network smoke test using a public fixture"]
    fn public_github_fetch_review_and_cached_reconciliation() {
        let root = tempfile::tempdir().unwrap();
        let repos = Repositories::new(root.path()).unwrap();
        let review = repos
            .preview(
                "public-fixture",
                "octocat/Hello-World",
                None,
                None,
                1024 * 1024,
            )
            .unwrap();
        assert!(
            review["files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p == "README")
        );
        let sha = review["commit"].as_str().unwrap();
        let cache = root.path().join("github/public-fixture/repository.git");
        let staging = tempfile::tempdir().unwrap();
        source::stage_files(
            &cache,
            &source::preview(&cache, sha).unwrap(),
            staging.path(),
        )
        .unwrap();
        assert!(!staging.path().join("source/.git").exists());
        let remote = GitHubRemote::new(
            "octocat/Hello-World",
            review["branch"].as_str().unwrap(),
            None,
            &cache,
        )
        .unwrap();
        let outbox = crate::sync::Outbox::open(&root.path().join("smoke-outbox"), sha).unwrap();
        outbox.configure(true).unwrap();
        let status = outbox
            .enqueue(
                1,
                "synthetic-readonly-smoke",
                &staging.path().join("source"),
            )
            .unwrap();
        let attempt = outbox
            .prepare(status["active"]["id"].as_str().unwrap(), &remote)
            .unwrap();
        assert_eq!(attempt.status, crate::sync::Status::AwaitingAcceptance);
        // No acceptance or delivery call: this test cannot mutate GitHub.
    }
    #[test]
    fn repository_and_branch_validation_prevent_credentials_and_transport_escape() {
        assert_eq!(
            repository("https://github.com/Owner/App.git").unwrap(),
            "owner/app"
        );
        for value in [
            "file:///tmp/repo",
            "https://github.com.evil/x/y",
            "https://token@github.com/x/y",
            "x/y?token=secret",
            "../x",
            "x/y/z",
            "--upload-pack=x",
            "x/y\n",
        ] {
            assert!(repository(value).is_err(), "{value}");
        }
        for value in [
            "-main",
            "main:other",
            "../main",
            "refs//main",
            "main.lock",
            "a@{1}",
            "a b",
            "main\n",
        ] {
            assert!(branch(value).is_err(), "{value}");
        }
        branch("feature/my-app_2").unwrap();
    }
}
