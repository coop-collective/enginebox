//! Durable GitHub delivery outbox. LayerFS acceptance and Git ref delivery are
//! separate transactions. Git and merge conflicts live only in this adapter.
use crate::{config::identifier, source};
use anyhow::{Context, Result, ensure};
use enginebox_core::state;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::{
    fs,
    path::{Path, PathBuf},
};

pub trait Remote {
    fn head(&self) -> Result<String>;
    fn fetch(&self, repo: &Path, commit: &str, reference: &str) -> Result<()>;
    /// Must compare-and-swap expected -> commit and refuse any changed head.
    fn push(&self, repo: &Path, expected: &str, commit: &str) -> Result<()>;
    fn remember(&self, _repo: &Path, _commit: &str) -> Result<()> {
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Queued,
    Preparing,
    NeedsAgent,
    AwaitingAcceptance,
    Accepting,
    Ready,
    Delivering,
    Delivered,
    Stale,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Attempt {
    pub id: String,
    pub sequence: u64,
    pub layerfs_revision: String,
    pub base: String,
    pub snapshot_digest: String,
    pub layerfs_commit: Option<String>,
    pub github_head: Option<String>,
    pub status: Status,
    pub conflicts: Vec<String>,
    pub accepted_revision: Option<String>,
    pub accepted_sequence: Option<u64>,
    pub candidate: Option<String>,
    pub candidate_digest: Option<String>,
    pub delivery_commit: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct Connection {
    schema: u32,
    enabled: bool,
    last_commit: String,
    latest_sequence: u64,
    latest_revision: String,
    active: Option<String>,
}
pub struct Outbox {
    root: PathBuf,
}
fn git(repo: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(source::git(repo, args)?)?
        .trim()
        .to_owned())
}
fn sha(value: &str) -> bool {
    [40, 64].contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn revision(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control),
        "Invalid LayerFS revision"
    );
    Ok(())
}
/// Copy an immutable accepted source snapshot, never a live agent mount. Fail
/// closed on secrets, escaping symlinks, special files and Git metadata.
pub fn copy_files(from: &Path, to: &Path) -> Result<()> {
    ensure!(
        fs::symlink_metadata(from)?.is_dir(),
        "Snapshot must be a real directory"
    );
    fn walk(root: &Path, relative: &Path, target: &Path) -> Result<()> {
        for entry in fs::read_dir(root.join(relative))? {
            let entry = entry?;
            let path = relative.join(entry.file_name());
            let name = path.to_str().context("Source path must be UTF-8")?;
            source::allowed_path(name)?;
            let ty = entry.file_type()?;
            let output = target.join(&path);
            if ty.is_dir() {
                fs::create_dir(&output)?;
                walk(root, &path, target)?;
            } else if ty.is_symlink() {
                let link = fs::read_link(entry.path())?;
                source::safe_link(name, link.to_str().context("Symlink must be UTF-8")?)?;
                #[cfg(unix)]
                std::os::unix::fs::symlink(link, output)?;
                #[cfg(not(unix))]
                anyhow::bail!("Symlink materialization requires a Unix sync worker");
            } else {
                ensure!(ty.is_file(), "Special file in source snapshot");
                fs::copy(entry.path(), output)?;
            }
        }
        Ok(())
    }
    walk(from, Path::new(""), to)
}

/// Integrity identity for a Git-free source tree, including modes and symlinks.
pub fn snapshot_digest(root: &Path) -> Result<String> {
    fn walk(root: &Path, relative: &Path, hash: &mut Sha256) -> Result<()> {
        let mut entries =
            fs::read_dir(root.join(relative))?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let relative = relative.join(entry.file_name());
            let name = relative.to_str().context("Source filename must be UTF-8")?;
            source::allowed_path(name)?;
            hash.update(name.as_bytes());
            hash.update([0]);
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.is_dir() {
                hash.update(b"dir");
                walk(root, &relative, hash)?;
            } else if metadata.file_type().is_symlink() {
                let link = fs::read_link(entry.path())?;
                let link = link.to_str().context("Symlink must be UTF-8")?;
                source::safe_link(name, link)?;
                hash.update(b"link");
                hash.update(link.as_bytes());
            } else {
                ensure!(metadata.is_file(), "Special file in accepted source");
                hash.update(b"file");
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    hash.update((metadata.permissions().mode() & 0o111).to_le_bytes());
                }
                hash.update(metadata.len().to_le_bytes());
                let mut file = fs::File::open(entry.path())?;
                let mut content = Sha256::new();
                let mut buffer = [0; 65536];
                loop {
                    let n = file.read(&mut buffer)?;
                    if n == 0 {
                        break;
                    }
                    content.update(&buffer[..n]);
                }
                hash.update(content.finalize());
            }
            hash.update([0]);
        }
        Ok(())
    }
    ensure!(
        fs::symlink_metadata(root)?.is_dir(),
        "Snapshot root must be a directory"
    );
    let mut hash = Sha256::new();
    hash.update(b"enginebox-source-snapshot-v1\0");
    walk(root, Path::new(""), &mut hash)?;
    Ok(format!("{:x}", hash.finalize()))
}

fn resolution_digest(staging: &Path) -> Result<String> {
    Ok(state::digest(
        &json!({"files":snapshot_digest(&staging.join("source"))?,"remove":state::read_json(&staging.join("remove.json"))?,"write":state::read_json(&staging.join("write.json"))?}),
    ))
}

// Overlay reviewed source onto a private immutable snapshot. Never follow an
// existing symlink, and never replace a directory containing retained data.
fn overlay_files(from: &Path, to: &Path) -> Result<()> {
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let output = to.join(entry.file_name());
        let incoming = entry.file_type()?;
        if let Ok(existing) = fs::symlink_metadata(&output) {
            if incoming.is_dir() && existing.is_dir() {
                overlay_files(&entry.path(), &output)?;
                continue;
            }
            if existing.is_dir() {
                fs::remove_dir(&output)?;
            } else {
                fs::remove_file(&output)?;
            }
        }
        if incoming.is_dir() {
            fs::create_dir(&output)?;
            overlay_files(&entry.path(), &output)?;
        } else if incoming.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(fs::read_link(entry.path())?, &output)?;
            #[cfg(not(unix))]
            anyhow::bail!("Symlink materialization requires Unix");
        } else {
            ensure!(incoming.is_file(), "Special resolution file");
            fs::copy(entry.path(), &output)?;
        }
    }
    Ok(())
}

fn sanitize_agent_git(repo: &Path) -> Result<()> {
    fn walk(path: &Path) -> Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "Symlink in reconciliation Git metadata"
        );
        if metadata.is_dir() {
            for entry in fs::read_dir(path)? {
                walk(&entry?.path())?;
            }
        } else {
            ensure!(metadata.is_file(), "Special Git metadata file");
        }
        Ok(())
    }
    let metadata = repo.join(".git");
    ensure!(
        fs::symlink_metadata(&metadata)?.is_dir(),
        "Git metadata must remain a directory"
    );
    walk(&metadata)?;
    for relative in [
        "commondir",
        "gitdir",
        "objects/info/alternates",
        "objects/info/http-alternates",
        "info/grafts",
    ] {
        ensure!(
            !metadata.join(relative).exists(),
            "External Git object or metadata indirection is refused"
        );
    }
    // Agent edits cannot install credential helpers, filters, include files,
    // external merge drivers or filesystem-monitor commands on the sync host.
    fs::write(
        metadata.join("config"),
        "[core]\nrepositoryformatversion = 0\nbare = false\nfilemode = true\n",
    )?;
    Ok(())
}
impl Outbox {
    pub fn open(root: &Path, initial_commit: &str) -> Result<Self> {
        ensure!(sha(initial_commit), "Exact initial GitHub commit required");
        state::private_dir(root)?;
        let this = Self { root: root.into() };
        let _lock = state::lock(&root.join(".lock"), false)?;
        if !root.join("connection.json").exists() {
            this.save_connection(&Connection {
                schema: 1,
                enabled: false,
                last_commit: initial_commit.into(),
                latest_sequence: 0,
                latest_revision: String::new(),
                active: None,
            })?;
        }
        Ok(this)
    }
    fn connection(&self) -> Result<Connection> {
        let c: Connection =
            serde_json::from_value(state::read_json(&self.root.join("connection.json"))?)?;
        ensure!(
            c.schema == 1 && sha(&c.last_commit),
            "Invalid GitHub outbox binding"
        );
        Ok(c)
    }
    fn save_connection(&self, c: &Connection) -> Result<()> {
        state::write_json(&self.root.join("connection.json"), &json!(c))
    }
    fn dir(&self, id: &str) -> Result<PathBuf> {
        identifier(id)?;
        Ok(self.root.join(id))
    }
    pub fn read(&self, id: &str) -> Result<Attempt> {
        let a: Attempt =
            serde_json::from_value(state::read_json(&self.dir(id)?.join("attempt.json"))?)?;
        ensure!(a.id == id && sha(&a.base), "Invalid sync attempt");
        Ok(a)
    }
    fn save(&self, a: &Attempt) -> Result<()> {
        state::write_json(&self.dir(&a.id)?.join("attempt.json"), &json!(a))
    }
    fn current(&self, c: &Connection, a: &Attempt) -> Result<()> {
        ensure!(c.enabled, "GitHub outbound sync is disabled");
        ensure!(
            c.active.as_deref() == Some(&a.id)
                && (c.latest_sequence == a.sequence
                    || a.accepted_sequence == Some(c.latest_sequence))
                && (c.latest_revision == a.layerfs_revision
                    || a.accepted_revision.as_deref() == Some(&c.latest_revision)),
            "LayerFS main advanced; this reconciliation attempt is stale"
        );
        Ok(())
    }
    pub fn configure(&self, enabled: bool) -> Result<Value> {
        let _lock = state::lock(&self.root.join(".lock"), false)?;
        let mut c = self.connection()?;
        c.enabled = enabled;
        self.save_connection(&c)?;
        self.status()
    }
    pub fn status(&self) -> Result<Value> {
        let c = self.connection()?;
        let active = c.active.as_ref().map(|id| self.read(id)).transpose()?;
        Ok(
            json!({"enabled":c.enabled,"last_synced_commit":c.last_commit,"latest_revision":c.latest_revision,"active":active}),
        )
    }
    /// Called only for an accepted main promotion, with an immutable export.
    /// Duplicate event delivery is idempotent; an event cannot reuse a sequence.
    pub fn enqueue(&self, sequence: u64, layerfs_revision: &str, files: &Path) -> Result<Value> {
        revision(layerfs_revision)?;
        let _lock = state::lock(&self.root.join(".lock"), false)?;
        let mut c = self.connection()?;
        if sequence == c.latest_sequence && c.latest_revision == layerfs_revision {
            return self.status();
        }
        ensure!(
            sequence > c.latest_sequence,
            "Stale or conflicting main promotion sequence"
        );
        c.latest_sequence = sequence;
        c.latest_revision = layerfs_revision.into();
        let prior = c.active.take();
        if c.enabled {
            let id = format!("sync-{}", uuid::Uuid::new_v4().simple());
            let directory = self.dir(&id)?;
            state::private_dir(&directory)?;
            let snapshot = directory.join("layerfs");
            state::private_dir(&snapshot)?;
            copy_files(files, &snapshot)?;
            let a = Attempt {
                id: id.clone(),
                sequence,
                layerfs_revision: layerfs_revision.into(),
                base: c.last_commit.clone(),
                snapshot_digest: snapshot_digest(&snapshot)?,
                layerfs_commit: None,
                github_head: None,
                status: Status::Queued,
                conflicts: vec![],
                accepted_revision: None,
                accepted_sequence: None,
                candidate: None,
                candidate_digest: None,
                delivery_commit: None,
            };
            self.save(&a)?;
            c.active = Some(id);
        }
        self.save_connection(&c)?;
        if let Some(id) = prior {
            let mut previous = self.read(&id)?;
            if previous.status != Status::Delivered {
                previous.status = Status::Stale;
                self.save(&previous)?;
            }
        }
        self.status()
    }
    pub fn worktree(&self, id: &str) -> Result<PathBuf> {
        Ok(self.dir(id)?.join("environment"))
    }
    pub fn prepare(&self, id: &str, remote: &dyn Remote) -> Result<Attempt> {
        let _lock = state::lock(&self.root.join(".lock"), false)?;
        let c = self.connection()?;
        let mut a = self.read(id)?;
        self.current(&c, &a)?;
        if matches!(a.status, Status::NeedsAgent | Status::AwaitingAcceptance) {
            return Ok(a);
        }
        ensure!(
            a.status == Status::Queued,
            "Interrupted preparation requires a new attempt, not automatic replay"
        );
        a.status = Status::Preparing;
        self.save(&a)?;
        ensure!(
            snapshot_digest(&self.dir(id)?.join("layerfs"))? == a.snapshot_digest,
            "Accepted snapshot changed or was damaged; reconciliation is blocked"
        );
        let repo = self.worktree(id)?;
        state::private_dir(&repo)?;
        git(&repo, &["init", "-b", "layerfs"])?;
        let head = remote.head()?;
        ensure!(sha(&head), "Invalid GitHub head");
        remote.fetch(&repo, &a.base, "refs/heads/base")?;
        remote.fetch(&repo, &head, "refs/heads/upstream")?;
        // Index starts at the base so Git-ignore rules retain previously tracked
        // files. The working tree is replaced by the exact LayerFS source export.
        git(&repo, &["read-tree", &a.base])?;
        copy_files(&self.dir(id)?.join("layerfs"), &repo)?;
        git(&repo, &["add", "--all"])?;
        let tree = git(&repo, &["write-tree"])?;
        let ours = git(
            &repo,
            &[
                "commit-tree",
                &tree,
                "-p",
                &a.base,
                "-m",
                &format!("LayerFS accepted revision {}", a.layerfs_revision),
            ],
        )?;
        git(&repo, &["update-ref", "refs/heads/layerfs", &ours])?;
        a.layerfs_commit = Some(ours.clone());
        git(&repo, &["reset", "--hard", &ours])?;
        let theirs_tree = git(&repo, &["rev-parse", &format!("{head}^{{tree}}")])?;
        // A synthetic sibling allows an exact three-way merge with shallow
        // external objects, independent of GitHub history rewrites or depth.
        let theirs = git(
            &repo,
            &[
                "commit-tree",
                &theirs_tree,
                "-p",
                &a.base,
                "-m",
                "GitHub observed tree",
            ],
        )?;
        git(&repo, &["update-ref", "refs/heads/github", &theirs])?;
        let merged = git(&repo, &["merge", "--no-commit", "--no-ff", "github"]);
        a.github_head = Some(head);
        let conflicts = String::from_utf8(source::git(
            &repo,
            &["diff", "--name-only", "--diff-filter=U", "-z"],
        )?)?;
        a.conflicts = conflicts
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        if a.conflicts.is_empty() {
            merged?;
            a.status = Status::AwaitingAcceptance;
        } else {
            a.status = Status::NeedsAgent;
        }
        self.save(&a)?;
        state::write_json(
            &self.dir(id)?.join("agent-task.json"),
            &json!({"schema":1,"attempt":a.id,"kind":"github-reconciliation","layerfs_revision":a.layerfs_revision,"sequence":a.sequence,"layerfs_commit":a.layerfs_commit,"base_commit":a.base,"github_head":a.github_head,"conflicts":a.conflicts,"worktree":repo,"instructions":"Resolve the Git index conflicts using LayerFS (ours) and GitHub (theirs), preserving intended changes on both sides. Run relevant project tests. Stage all resolved source files with git add. Do not push, change remotes, or add credentials. Return this attempt ID and test evidence for controller acceptance."}),
        )?;
        Ok(a)
    }
    /// The callback must CAS the LayerFS accepted main revision and be
    /// idempotent for this attempt ID. It consumes files, never Git metadata.
    pub fn accept(
        &self,
        id: &str,
        accept: impl FnOnce(&Attempt, &Path) -> Result<(String, u64)>,
    ) -> Result<Attempt> {
        let _lock = state::lock(&self.root.join(".lock"), false)?;
        let mut c = self.connection()?;
        let mut a = self.read(id)?;
        self.current(&c, &a)?;
        if a.status == Status::Ready {
            return Ok(a);
        }
        let staging = self.dir(id)?.join("resolved");
        if a.status != Status::Accepting {
            ensure!(
                matches!(a.status, Status::NeedsAgent | Status::AwaitingAcceptance),
                "Attempt is not awaiting resolution/acceptance"
            );
            let repo = self.worktree(id)?;
            sanitize_agent_git(&repo)?;
            ensure!(
                source::git(&repo, &["ls-files", "--unmerged"])?.is_empty(),
                "Unresolved Git index conflicts remain"
            );
            git(&repo, &["diff", "--exit-code"])?;
            ensure!(
                source::git(&repo, &["ls-files", "--others", "--exclude-standard"])?.is_empty(),
                "Stage all intended resolution files first"
            );
            for path in &a.conflicts {
                source::allowed_path(path)?;
                let file = repo.join(path);
                if file.is_file() && !fs::symlink_metadata(&file)?.file_type().is_symlink() {
                    let bytes = fs::read(&file)?;
                    ensure!(
                        !bytes
                            .split(|b| *b == b'\n')
                            .any(|line| line.starts_with(b"<<<<<<< ")
                                || line.starts_with(b">>>>>>> ")),
                        "Conflict markers remain in {path}"
                    );
                }
            }
            let tree = git(&repo, &["write-tree"])?;
            let parent = a
                .github_head
                .as_deref()
                .context("Missing observed GitHub head")?;
            let candidate = git(
                &repo,
                &[
                    "commit-tree",
                    &tree,
                    "-p",
                    parent,
                    "-m",
                    &format!(
                        "Sync LayerFS revision {}\n\nEnginebox-Sync: {}",
                        a.layerfs_revision, a.id
                    ),
                ],
            )?;
            let preview = source::preview_for_export(&repo, &candidate)?;
            let original = String::from_utf8(source::git(
                &repo,
                &[
                    "ls-tree",
                    "-rz",
                    "--name-only",
                    a.layerfs_commit
                        .as_deref()
                        .context("Original source commit missing")?,
                ],
            )?)?;
            let selected: std::collections::BTreeSet<&str> =
                original.split('\0').filter(|p| !p.is_empty()).collect();
            let frozen = self.dir(id)?.join("layerfs");
            ensure!(
                snapshot_digest(&frozen)? == a.snapshot_digest,
                "Accepted snapshot integrity check failed"
            );
            for path in &preview.files {
                let mut parent = Path::new(path).parent();
                while let Some(path) = parent.filter(|p| !p.as_os_str().is_empty()) {
                    if let Ok(metadata) = fs::symlink_metadata(frozen.join(path)) {
                        ensure!(
                            metadata.is_dir()
                                || selected.contains(path.to_str().context("Source path")?),
                            "GitHub source collides with retained LayerFS data: {}",
                            path.display()
                        );
                    }
                    parent = path.parent();
                }
                if !selected.contains(path.as_str()) {
                    ensure!(
                        fs::symlink_metadata(frozen.join(path))
                            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
                        "GitHub source collides with retained LayerFS data: {path}"
                    );
                }
            }
            let removed: Vec<_> = selected
                .iter()
                .filter(|path| !preview.files.iter().any(|file| file == **path))
                .copied()
                .collect();
            for path in &removed {
                source::allowed_path(path)?;
            }

            state::private_dir(&staging)?;
            source::stage_files(&repo, &preview, &staging)?;
            state::write_json(&staging.join("remove.json"), &json!(removed))?;
            state::write_json(&staging.join("write.json"), &json!(preview.files))?;
            a.candidate = Some(candidate);
            a.candidate_digest = Some(resolution_digest(&staging)?);
            a.status = Status::Accepting;
            self.save(&a)?;
        }
        // No GitHub mutation can precede this acceptance, including after an
        // uncertain callback. Only the same frozen candidate can be reconciled.
        ensure!(
            a.candidate_digest.as_deref() == Some(&resolution_digest(&staging)?),
            "Frozen resolution files changed or were damaged"
        );
        let (accepted, sequence) = accept(&a, &staging.join("source"))?;
        ensure!(
            sequence > a.sequence,
            "Reconciliation acceptance must advance the LayerFS sequence"
        );
        revision(&accepted)?;
        c.latest_revision = accepted.clone();
        c.latest_sequence = sequence;
        a.accepted_revision = Some(accepted);
        a.accepted_sequence = Some(sequence);
        a.status = Status::Ready;
        self.save(&a)?;
        self.save_connection(&c)?;
        Ok(a)
    }
    pub fn deliver(&self, id: &str, remote: &dyn Remote) -> Result<Attempt> {
        let _lock = state::lock(&self.root.join(".lock"), false)?;
        let mut c = self.connection()?;
        let mut a = self.read(id)?;
        if a.status == Status::Delivered {
            return Ok(a);
        }
        self.current(&c, &a)?;
        ensure!(
            matches!(a.status, Status::Ready | Status::Delivering),
            "LayerFS acceptance required before GitHub delivery"
        );
        let target = a
            .candidate
            .clone()
            .context("Missing accepted Git candidate")?;
        let expected = a.github_head.as_deref().context("Missing GitHub head")?;
        let observed = remote.head()?;
        if observed != target {
            if observed != expected {
                a.status = Status::Stale;
                self.save(&a)?;
                anyhow::bail!("GitHub advanced during reconciliation; prepare a fresh attempt");
            }
            a.delivery_commit = Some(target.clone());
            a.status = Status::Delivering;
            self.save(&a)?;
            // Even if the response is lost, the exact target is durable. The next
            // call first recognizes it at the remote and never duplicates it.
            remote.push(&self.worktree(id)?, expected, &target)?;
        }
        ensure!(
            remote.head()? == target,
            "GitHub changed after delivery; inspect before acknowledging sync"
        );
        a.delivery_commit = Some(target.clone());
        a.status = Status::Delivered;
        // Commit connection first. On restart, Delivering + remote==target is
        // safe to acknowledge again without changing the exported commit.
        remote.remember(&self.worktree(id)?, &target)?;
        c.last_commit = target;
        c.latest_sequence = a.accepted_sequence.context("Accepted sequence missing")?;
        c.latest_revision = a
            .accepted_revision
            .clone()
            .context("Accepted revision missing")?;
        self.save_connection(&c)?;
        self.save(&a)?;
        Ok(a)
    }
    /// Retry against a fresh observed remote after staleness/preparation failure.
    /// It retains the old attempt and reuses the latest accepted source export.
    pub fn retry(&self, id: &str) -> Result<Value> {
        let _lock = state::lock(&self.root.join(".lock"), false)?;
        let mut c = self.connection()?;
        let old = self.read(id)?;
        self.current(&c, &old)?;
        ensure!(
            matches!(old.status, Status::Stale | Status::Preparing),
            "Only stale or interrupted preparation can start a new attempt"
        );
        let new_id = format!("sync-{}", uuid::Uuid::new_v4().simple());
        let dir = self.dir(&new_id)?;
        state::private_dir(&dir)?;
        let snapshot = dir.join("layerfs");
        state::private_dir(&snapshot)?;
        let resolved = self.dir(id)?.join("resolved/source");
        let original = self.dir(id)?.join("layerfs");
        ensure!(
            snapshot_digest(&original)? == old.snapshot_digest,
            "Accepted snapshot integrity check failed"
        );
        if old.accepted_revision.is_some() {
            ensure!(
                old.candidate_digest.as_deref()
                    == Some(&resolution_digest(&self.dir(id)?.join("resolved"))?),
                "Accepted resolution integrity check failed"
            );
        }
        copy_files(&original, &snapshot)?;
        if old.accepted_revision.is_some() {
            let removed = state::read_json(&self.dir(id)?.join("resolved/remove.json"))?;
            for path in removed.as_array().context("Invalid removal plan")? {
                let path = path.as_str().context("Invalid source path")?;
                source::allowed_path(path)?;
                fs::remove_file(snapshot.join(path))?;
            }
            overlay_files(&resolved, &snapshot)?;
        }
        let latest_revision = old
            .accepted_revision
            .clone()
            .unwrap_or_else(|| old.layerfs_revision.clone());
        c.latest_revision = latest_revision.clone();
        let a = Attempt {
            id: new_id.clone(),
            layerfs_revision: latest_revision,
            sequence: old.accepted_sequence.unwrap_or(old.sequence),
            base: c.last_commit.clone(),
            snapshot_digest: snapshot_digest(&snapshot)?,
            layerfs_commit: None,
            github_head: None,
            status: Status::Queued,
            conflicts: vec![],
            accepted_revision: None,
            accepted_sequence: None,
            candidate: None,
            candidate_digest: None,
            delivery_commit: None,
        };
        self.save(&a)?;
        c.active = Some(new_id);
        self.save_connection(&c)?;
        self.status()
    }
}
