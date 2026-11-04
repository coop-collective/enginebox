//! `GitTreeProvider`: forks as plain directories beside the dev tree, recorded
//! in the node-private repository `work/engine/repos/<key>.git` (contract
//! §2.2). No tree contains a `.git`; every git call names its git directory,
//! work tree and index explicitly and ignores system and global config, hooks
//! and fsmonitor, so nothing an agent or the Editor writes can make the node
//! run code.
use super::{
    Capture, Change, Commit, DevTree, ForkError, ForkId, ForkProvider, ForkTree, Principal, Result,
    SlotAccount, Snapshot,
    classify::{self, Class, Finding},
    prescan,
};
use anyhow::{Context, anyhow};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// The empty tree's id (SHA-1 repositories).
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const COMMITTER: (&str, &str) = ("Engine Box", "node@enginebox.invalid");

/// Fork metadata kept in `meta/state.json` (node only).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForkState {
    pub schema: u32,
    pub session: String,
    pub fork: ForkId,
    pub base: Commit,
    #[serde(default)]
    pub head: Option<Commit>,
    pub project_path: String,
    pub owner: SlotAccount,
}

pub struct GitTreeProvider {
    git: PathBuf,
    /// Node-private: `hooks/` (always empty) and `gitconfig` (empty).
    private: PathBuf,
    /// Smudge and clean Git LFS content (the engine checkouts use LFS).
    lfs: bool,
}

/// One git invocation's context.
struct Repo<'a> {
    p: &'a GitTreeProvider,
    git_dir: &'a Path,
    work_tree: Option<&'a Path>,
    index: Option<PathBuf>,
    attr_source: Option<&'a str>,
    identity: Option<(String, String)>,
}

pub struct Output {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl GitTreeProvider {
    /// `private` is a node-only directory (e.g. `work/engine/repos/.node`).
    pub fn new(git: PathBuf, private: &Path, lfs: bool) -> anyhow::Result<Self> {
        crate::release::private_dir(private)?;
        let hooks = private.join("hooks");
        std::fs::create_dir_all(&hooks)?;
        // The hooks directory stays empty: nothing in it ever runs.
        for entry in std::fs::read_dir(&hooks)? {
            let path = entry?.path();
            if path.is_dir() {
                std::fs::remove_dir_all(&path)?;
            } else {
                std::fs::remove_file(&path)?;
            }
        }
        std::fs::write(private.join("gitconfig"), b"")?;
        Ok(Self {
            git,
            private: private.to_owned(),
            lfs,
        })
    }
    pub fn git(&self) -> &Path {
        &self.git
    }
    pub fn lfs(&self) -> bool {
        self.lfs
    }
    /// Forks need Git 2.42+ (`merge-tree --write-tree --merge-base`,
    /// `GIT_ATTR_SOURCE`). Returns the version.
    pub fn check_version(&self) -> anyhow::Result<String> {
        let out = Command::new(&self.git).arg("--version").output()?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        let v =
            parse_version(&text).with_context(|| format!("Unrecognised git version: {text}"))?;
        anyhow::ensure!(
            v >= (2, 42),
            "Agent forks need Git 2.42 or newer; this machine has {text}"
        );
        Ok(text)
    }

    /// The hardened base command (contract §0): explicit directories, no
    /// system/global config, no hooks, no fsmonitor, literal pathspecs.
    pub fn command(
        &self,
        git_dir: &Path,
        work_tree: Option<&Path>,
        index: Option<&Path>,
    ) -> Command {
        let mut c = Command::new(&self.git);
        c.env_clear();
        for key in KEEP_ENV {
            if let Some(v) = std::env::var_os(key) {
                c.env(key, v);
            }
        }
        c.env("GIT_DIR", git_dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.private.join("gitconfig"))
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_LITERAL_PATHSPECS", "1")
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_LFS_SKIP_SMUDGE", "0")
            .env("GCM_INTERACTIVE", "never")
            .env("LC_ALL", "C");
        if let Some(w) = work_tree {
            c.env("GIT_WORK_TREE", w);
        }
        if let Some(i) = index {
            c.env("GIT_INDEX_FILE", i);
        }
        for (k, v) in self.config() {
            c.arg("-c").arg(format!("{k}={}", v.to_string_lossy()));
        }
        c.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c
    }
    fn config(&self) -> Vec<(&'static str, OsString)> {
        let mut v: Vec<(&'static str, OsString)> = vec![
            ("core.hooksPath", self.private.join("hooks").into()),
            ("core.fsmonitor", "false".into()),
            ("core.symlinks", "true".into()),
            ("protocol.file.allow", "never".into()),
            ("core.autocrlf", "false".into()),
            ("core.longpaths", "true".into()),
            ("core.untrackedCache", "false".into()),
            ("core.splitIndex", "false".into()),
            ("core.sparseCheckout", "false".into()),
            ("core.quotePath", "false".into()),
            ("gc.auto", "0".into()),
            ("maintenance.auto", "false".into()),
            ("diff.external", "".into()),
            ("merge.renormalize", "false".into()),
        ];
        if self.lfs {
            v.extend([
                ("filter.lfs.process", "git-lfs filter-process".into()),
                ("filter.lfs.clean", "git-lfs clean -- %f".into()),
                ("filter.lfs.smudge", "git-lfs smudge -- %f".into()),
                ("filter.lfs.required", "true".into()),
            ]);
        }
        v
    }
    fn repo<'a>(&'a self, git_dir: &'a Path) -> Repo<'a> {
        Repo {
            p: self,
            git_dir,
            work_tree: None,
            index: None,
            attr_source: None,
            identity: None,
        }
    }

    /// Point the dev head at `commit` (session start).
    pub fn start_dev(&self, s: &DevTree, commit: &Commit) -> Result<()> {
        self.repo(&s.git_dir)
            .ok(&["update-ref", &s.dev_ref(), commit.as_str()])?;
        Ok(())
    }

    /// Keep the node's own edits to tracked files (the bridge entries it adds
    /// to `Packages/manifest.json`) out of live capture: `skip-worktree` in
    /// the dev index. Paths not tracked are ignored.
    pub fn hide_node_edits(&self, s: &DevTree, paths: &[String]) -> Result<()> {
        let mut r = self.repo(&s.git_dir);
        r.work_tree = Some(&s.work_tree);
        r.index = Some(s.index.clone());
        for p in paths {
            let tracked = r.ok(&["ls-files", "-z", "--", p])?;
            if !tracked.is_empty() {
                r.ok(&["update-index", "--skip-worktree", "--", p])?;
            }
        }
        Ok(())
    }

    /// Reopen a fork created earlier (node restart, later ops).
    pub fn open(&self, s: &DevTree, fork: &ForkId) -> Result<(ForkTree, ForkState)> {
        let root = s.forks_root.join(fork.as_str());
        let f = fork_tree(&root, fork, s);
        let bytes = std::fs::read(f.meta.join("state.json"))
            .map_err(|_| ForkError::Invalid("unknown fork".into()))?;
        let state: ForkState =
            serde_json::from_slice(&bytes).map_err(|e| ForkError::Other(e.into()))?;
        if state.session != s.session || &state.fork != fork {
            return Err(ForkError::Invalid("fork belongs to another session".into()));
        }
        Ok((f, state))
    }
    pub fn state(&self, f: &ForkTree) -> Result<ForkState> {
        let bytes = std::fs::read(f.meta.join("state.json"))?;
        serde_json::from_slice(&bytes).map_err(|e| ForkError::Other(e.into()))
    }
    fn save_state(&self, f: &ForkTree, state: &ForkState) -> Result<()> {
        crate::release::atomic_json(&f.meta.join("state.json"), state)?;
        Ok(())
    }
    /// The node-private repository a fork belongs to: `meta/repo` records it.
    fn fork_git_dir(&self, f: &ForkTree) -> Result<PathBuf> {
        let text = std::fs::read_to_string(f.meta.join("repo"))?;
        Ok(PathBuf::from(text.trim()))
    }

    /// `diff-tree` between two tree-ish ids as `Change`s (sizes from the
    /// object store; LFS pointers report the object's size).
    fn changes(&self, git_dir: &Path, from: &str, to: &str) -> Result<Vec<Change>> {
        let out = self.repo(git_dir).ok(&[
            "diff-tree",
            "-r",
            "-z",
            "--raw",
            "--no-renames",
            "--no-ext-diff",
            from,
            to,
        ])?;
        let mut changes = Vec::new();
        let mut blobs = Vec::new();
        let mut fields = out.split(|b| *b == 0).filter(|f| !f.is_empty());
        while let Some(meta) = fields.next() {
            let meta = String::from_utf8_lossy(meta);
            let path = fields
                .next()
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .context("diff-tree output without a path")?;
            // :<old mode> <new mode> <old sha> <new sha> <status>
            let parts: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
            if parts.len() < 5 {
                return Err(anyhow!("unexpected diff-tree output").into());
            }
            let status = parts[4].chars().next().unwrap_or('M');
            let (mode, sha) = if status == 'D' {
                (parts[0], parts[2])
            } else {
                (parts[1], parts[3])
            };
            let mode = u32::from_str_radix(mode, 8).unwrap_or(0);
            blobs.push((status != 'D').then(|| sha.to_owned()));
            changes.push(Change {
                path,
                status,
                mode,
                lfs: false,
                bytes: 0,
            });
        }
        let wanted: Vec<&str> = blobs.iter().flatten().map(String::as_str).collect();
        if !wanted.is_empty() {
            let sizes = self.blob_info(git_dir, &wanted)?;
            for (change, sha) in changes.iter_mut().zip(&blobs) {
                if let Some(sha) = sha
                    && let Some((size, lfs)) = sizes.get(sha.as_str())
                {
                    change.bytes = *size;
                    change.lfs = *lfs;
                }
            }
        }
        Ok(changes)
    }
    /// Size and LFS-ness per blob id.
    fn blob_info(
        &self,
        git_dir: &Path,
        shas: &[&str],
    ) -> Result<std::collections::BTreeMap<String, (u64, bool)>> {
        let mut input = Vec::new();
        for s in shas {
            input.extend_from_slice(s.as_bytes());
            input.push(b'\n');
        }
        let out = self
            .repo(git_dir)
            .ok_input(&["cat-file", "--batch-check"], &input)?;
        let mut info = std::collections::BTreeMap::new();
        let mut small = Vec::new();
        for line in String::from_utf8_lossy(&out).lines() {
            let p: Vec<&str> = line.split(' ').collect();
            if p.len() == 3 && p[1] == "blob" {
                let size: u64 = p[2].parse().unwrap_or(0);
                if size < 1024 {
                    small.push(p[0].to_owned());
                }
                info.insert(p[0].to_owned(), (size, false));
            }
        }
        if !small.is_empty() {
            // Read small blobs to recognise LFS pointers.
            let mut input = Vec::new();
            for s in &small {
                input.extend_from_slice(s.as_bytes());
                input.push(b'\n');
            }
            let out = self
                .repo(git_dir)
                .ok_input(&["cat-file", "--batch"], &input)?;
            let mut rest = &out[..];
            while let Some(nl) = rest.iter().position(|b| *b == b'\n') {
                let header = String::from_utf8_lossy(&rest[..nl]).into_owned();
                let p: Vec<&str> = header.split(' ').collect();
                let size: usize = p.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
                let body = &rest[nl + 1..(nl + 1 + size).min(rest.len())];
                if let Some(object_size) = lfs_pointer_size(body) {
                    info.insert(p[0].to_owned(), (object_size, true));
                }
                rest = &rest[(nl + 2 + size).min(rest.len())..];
            }
        }
        Ok(info)
    }

    /// Paths git would record in `work_tree` against `index`.
    fn candidates(&self, git_dir: &Path, work_tree: &Path, index: &Path) -> Result<Vec<String>> {
        let mut r = self.repo(git_dir);
        r.work_tree = Some(work_tree);
        r.index = Some(index.to_owned());
        let out = r.ok(&[
            "ls-files",
            "-z",
            "--modified",
            "--others",
            "--exclude-standard",
        ])?;
        let set: BTreeSet<String> = out
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect();
        Ok(set.into_iter().collect())
    }

    /// Add `paths` (all candidates when None) to a copy of `index`, write the
    /// tree and run the size, mode and case checks against `base`. Returns
    /// (temporary index, tree, changes).
    fn stage(
        &self,
        git_dir: &Path,
        work_tree: &Path,
        index: &Path,
        paths: Option<&[String]>,
        base: &Commit,
    ) -> Result<(tempfile::TempPath, String, Vec<Change>)> {
        let dir = index.parent().context("index has no directory")?;
        let tmp = tempfile::Builder::new()
            .prefix(".stage-")
            .tempfile_in(dir)?
            .into_temp_path();
        if index.exists() {
            std::fs::copy(index, &tmp)?;
        } else {
            std::fs::remove_file(&tmp)?;
        }
        let mut r = self.repo(git_dir);
        r.work_tree = Some(work_tree);
        r.index = Some(tmp.to_path_buf());
        match paths {
            None => {
                r.ok(&["add", "-A"])?;
            }
            Some([]) => {}
            Some(paths) => {
                let mut input = Vec::new();
                for p in paths {
                    input.extend_from_slice(p.as_bytes());
                    input.push(0);
                }
                r.ok_input(
                    &["add", "-A", "--pathspec-from-file=-", "--pathspec-file-nul"],
                    &input,
                )?;
            }
        }
        let tree = String::from_utf8_lossy(&r.ok(&["write-tree"])?)
            .trim()
            .to_owned();
        let changes = self.changes(git_dir, base.as_str(), &tree)?;
        let mut findings = Vec::new();
        if changes.len() > prescan::MAX_CHANGED_FILES {
            findings.push(Finding::new("too-many-files", ""));
        }
        let added: u64 = changes
            .iter()
            .filter(|c| c.status != 'D')
            .map(|c| c.bytes)
            .sum();
        if added > prescan::MAX_ADDED_BYTES {
            findings.push(Finding::new("too-much-content", ""));
        }
        for c in &changes {
            if c.status != 'D' && c.mode == 0o120000 {
                findings.push(Finding::new("symlink", &c.path));
            }
            if c.status != 'D' && c.mode == 0o160000 {
                findings.push(Finding::new("nested-repository", &c.path));
            }
            if c.status != 'D' && !c.lfs && c.bytes > prescan::MAX_FILE_BYTES {
                findings.push(Finding::new("large-file-not-lfs", &c.path));
            }
        }
        if changes.iter().any(|c| c.status == 'A') {
            let all = r.ok(&["ls-files", "-z"])?;
            let names: Vec<String> = all
                .split(|b| *b == 0)
                .filter(|p| !p.is_empty())
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .collect();
            findings.extend(prescan::case_collisions(names.iter().map(String::as_str)));
        }
        if !findings.is_empty() {
            return Err(ForkError::Refused { findings });
        }
        Ok((tmp, tree, changes))
    }

    fn commit_tree(
        &self,
        git_dir: &Path,
        tree: &str,
        parents: &[&Commit],
        msg: &str,
        author: Option<&Principal>,
    ) -> Result<Commit> {
        let mut r = self.repo(git_dir);
        r.identity = author.map(|a| {
            (
                clean_ident(if a.name.is_empty() { &a.id } else { &a.name }),
                format!("{}@principals.enginebox.invalid", clean_email(&a.id)),
            )
        });
        let mut args: Vec<&str> = vec!["commit-tree", tree];
        for p in parents {
            args.extend(["-p", p.as_str()]);
        }
        let out = r.ok_input(&args, msg.as_bytes())?;
        Commit::parse(&String::from_utf8_lossy(&out))
    }

    /// Compare-and-swap of a ref.
    fn swap(&self, git_dir: &Path, name: &str, new: &Commit, old: &Commit) -> Result<()> {
        let out = self
            .repo(git_dir)
            .run(&["update-ref", name, new.as_str(), old.as_str()], None)?;
        if out.code != 0 {
            let current = self.rev(git_dir, name).ok();
            return Err(ForkError::Stale { current });
        }
        Ok(())
    }
    fn rev(&self, git_dir: &Path, name: &str) -> Result<Commit> {
        let out = self.repo(git_dir).ok(&[
            "rev-parse",
            "--verify",
            "-q",
            &format!("{name}^{{commit}}"),
        ])?;
        Commit::parse(&String::from_utf8_lossy(&out))
    }
    fn tree_of(&self, git_dir: &Path, commit: &Commit) -> Result<String> {
        let out = self
            .repo(git_dir)
            .ok(&["rev-parse", &format!("{}^{{tree}}", commit.as_str())])?;
        Ok(String::from_utf8_lossy(&out).trim().to_owned())
    }
    /// Make sure the empty tree object exists (for `GIT_ATTR_SOURCE`).
    fn empty_tree(&self, git_dir: &Path) -> Result<()> {
        let out = self
            .repo(git_dir)
            .ok_input(&["hash-object", "-t", "tree", "-w", "--stdin"], b"")?;
        if String::from_utf8_lossy(&out).trim() != EMPTY_TREE {
            return Err(anyhow!("repository does not use SHA-1").into());
        }
        Ok(())
    }
    /// A three-way merge without a work tree; attributes come from the empty
    /// tree, so no `.gitattributes` can choose a merge driver (`union`).
    fn merge_trees(&self, git_dir: &Path, base: &str, ours: &str, theirs: &str) -> Result<String> {
        self.empty_tree(git_dir)?;
        let mut r = self.repo(git_dir);
        r.attr_source = Some(EMPTY_TREE);
        let out = r.run(
            &[
                "merge-tree",
                "--write-tree",
                "-z",
                "--name-only",
                "--no-messages",
                &format!("--merge-base={base}"),
                ours,
                theirs,
            ],
            None,
        )?;
        let mut fields = out.stdout.split(|b| *b == 0);
        let tree = fields
            .next()
            .map(|t| String::from_utf8_lossy(t).trim().to_owned())
            .unwrap_or_default();
        match out.code {
            0 if tree.len() == 40 => Ok(tree),
            1 => {
                let paths: BTreeSet<String> = fields
                    .take_while(|f| !f.is_empty())
                    .map(|f| String::from_utf8_lossy(f).into_owned())
                    .collect();
                Err(ForkError::Conflict {
                    paths: paths.into_iter().collect(),
                })
            }
            _ => Err(anyhow!("git merge-tree failed: {}", last_line(&out.stderr)).into()),
        }
    }
    fn parent(&self, git_dir: &Path, commit: &Commit) -> Result<Commit> {
        self.rev(git_dir, &format!("{}^1", commit.as_str()))
    }
}

/// Inherited environment a git child needs (no `GIT_*`, no credentials).
const KEEP_ENV: &[&str] = &[
    "PATH",
    "SystemRoot",
    "SYSTEMROOT",
    "windir",
    "WINDIR",
    "ComSpec",
    "PATHEXT",
    "TEMP",
    "TMP",
    "TMPDIR",
    "HOME",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "LOCALAPPDATA",
    "APPDATA",
    "ProgramData",
    "ProgramFiles",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
];

fn last_line(s: &str) -> String {
    s.lines()
        .rfind(|l| !l.trim().is_empty())
        .unwrap_or("")
        .chars()
        .take(300)
        .collect()
}
fn clean_ident(s: &str) -> String {
    let s: String = s
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '<' | '>' | '\n'))
        .take(80)
        .collect();
    if s.trim().is_empty() {
        "Engine Box member".into()
    } else {
        s
    }
}
fn clean_email(s: &str) -> String {
    let s: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .take(80)
        .collect();
    if s.is_empty() { "member".into() } else { s }
}
/// `git version 2.45.1.windows.1` → (2, 45).
pub fn parse_version(text: &str) -> Option<(u32, u32)> {
    let v = text.strip_prefix("git version ")?;
    let mut parts = v.split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// The object size declared by a Git LFS pointer.
pub fn lfs_pointer_size(body: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(body).ok()?;
    if !text.starts_with("version https://git-lfs.github.com/spec/") {
        return None;
    }
    text.lines()
        .find_map(|l| l.strip_prefix("size "))
        .and_then(|s| s.trim().parse().ok())
}

impl Repo<'_> {
    fn command(&self, args: &[&str]) -> Command {
        let mut c = self
            .p
            .command(self.git_dir, self.work_tree, self.index.as_deref());
        if let Some(src) = self.attr_source {
            c.env("GIT_ATTR_SOURCE", src);
        }
        let (name, email) = self
            .identity
            .clone()
            .unwrap_or((COMMITTER.0.into(), COMMITTER.1.into()));
        c.env("GIT_AUTHOR_NAME", name)
            .env("GIT_AUTHOR_EMAIL", email)
            .env("GIT_COMMITTER_NAME", COMMITTER.0)
            .env("GIT_COMMITTER_EMAIL", COMMITTER.1);
        c.args(args.iter().map(OsStr::new));
        c
    }
    fn run(&self, args: &[&str], input: Option<&[u8]>) -> Result<Output> {
        let mut c = self.command(args);
        if input.is_some() {
            c.stdin(Stdio::piped());
        }
        let mut child = c.spawn().context("Cannot start git")?;
        if let Some(input) = input {
            let mut stdin = child.stdin.take().context("git stdin")?;
            let data = input.to_vec();
            // Write on a thread so a large output cannot deadlock the pipe.
            let writer = std::thread::spawn(move || stdin.write_all(&data));
            let out = child.wait_with_output()?;
            let _ = writer.join();
            return Ok(Output {
                code: out.status.code().unwrap_or(-1),
                stdout: out.stdout,
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        let out = child.wait_with_output()?;
        Ok(Output {
            code: out.status.code().unwrap_or(-1),
            stdout: out.stdout,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
    fn ok(&self, args: &[&str]) -> Result<Vec<u8>> {
        self.checked(args, None)
    }
    fn ok_input(&self, args: &[&str], input: &[u8]) -> Result<Vec<u8>> {
        self.checked(args, Some(input))
    }
    fn checked(&self, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
        let out = self.run(args, input)?;
        if out.code != 0 {
            return Err(anyhow!(
                "git {} failed: {}",
                args.first().copied().unwrap_or(""),
                last_line(&out.stderr)
            )
            .into());
        }
        Ok(out.stdout)
    }
}

fn fork_tree(root: &Path, fork: &ForkId, s: &DevTree) -> ForkTree {
    ForkTree {
        id: fork.clone(),
        session: s.session.clone(),
        root: root.to_owned(),
        tree: root.join("tree"),
        home: root.join("home"),
        meta: root.join("meta"),
        project_path: s.project_path.clone(),
    }
}

/// Remove a directory tree, clearing read-only attributes that stop Windows
/// from deleting files an agent or Unity marked read-only.
pub fn remove_tree(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => {
            clear_readonly(path);
            std::fs::remove_dir_all(path)
        }
    }
}
fn clear_readonly(path: &Path) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if meta.file_type().is_symlink() {
        return;
    }
    let mut perms = meta.permissions();
    if perms.readonly() {
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        let _ = std::fs::set_permissions(path, perms);
    }
    if meta.is_dir()
        && let Ok(entries) = std::fs::read_dir(path)
    {
        for e in entries.flatten() {
            clear_readonly(&e.path());
        }
    }
}

impl ForkProvider for GitTreeProvider {
    fn kind(&self) -> &'static str {
        "git"
    }

    fn dev_head(&self, s: &DevTree) -> Result<Commit> {
        self.rev(&s.git_dir, &s.dev_ref())
    }

    fn set_dev_head(&self, s: &DevTree, expected: &Commit, to: &Commit) -> Result<()> {
        self.rev(&s.git_dir, to.as_str())?;
        self.swap(&s.git_dir, &s.dev_ref(), to, expected)
    }

    fn capture_live(&self, s: &DevTree, attribution: &[Principal]) -> Result<Capture> {
        let head = self.dev_head(s)?;
        let candidates = self.candidates(&s.git_dir, &s.work_tree, &s.index)?;
        let mut refused = prescan::scan(&s.work_tree, &candidates);
        let refused_paths: BTreeSet<String> = refused.iter().map(|f| f.path.clone()).collect();
        let mut live_code_pending = Vec::new();
        let mut allowed = Vec::new();
        for path in candidates.iter().filter(|p| !refused_paths.contains(*p)) {
            match classify::classify(path, 0o100644, &s.project_path) {
                (Class::Refused, rule) => refused.push(Finding::new(rule, path)),
                (Class::Code | Class::Protected, _) => live_code_pending.push(path.clone()),
                _ => allowed.push(path.clone()),
            }
        }
        if allowed.is_empty() {
            return Ok(Capture {
                head,
                changed: Vec::new(),
                live_code_pending,
                refused,
            });
        }
        let (tmp, tree, changed) =
            self.stage(&s.git_dir, &s.work_tree, &s.index, Some(&allowed), &head)?;
        if changed.is_empty() {
            // Only stat information changed: keep the refreshed index.
            tmp.persist(&s.index)
                .map_err(|e| ForkError::Other(e.error.into()))?;
            return Ok(Capture {
                head,
                changed,
                live_code_pending,
                refused,
            });
        }
        let names: Vec<&str> = attribution
            .iter()
            .map(|p| {
                if p.name.is_empty() {
                    p.id.as_str()
                } else {
                    p.name.as_str()
                }
            })
            .collect();
        let msg = if names.is_empty() {
            "Live edits\n".to_owned()
        } else {
            format!("Live edits by {}\n", names.join(", "))
        };
        let commit = self.commit_tree(&s.git_dir, &tree, &[&head], &msg, attribution.first())?;
        self.swap(&s.git_dir, &s.dev_ref(), &commit, &head)?;
        tmp.persist(&s.index)
            .map_err(|e| ForkError::Other(e.error.into()))?;
        Ok(Capture {
            head: commit,
            changed,
            live_code_pending,
            refused,
        })
    }

    fn create(
        &self,
        s: &DevTree,
        fork: &ForkId,
        base: Option<&Commit>,
        owner: &SlotAccount,
    ) -> Result<ForkTree> {
        let base = match base {
            Some(b) => {
                self.rev(&s.git_dir, b.as_str())?;
                b.clone()
            }
            None => self.dev_head(s)?,
        };
        crate::release::private_dir(&s.forks_root)?;
        let root = s.forks_root.join(fork.as_str());
        if root.exists() {
            return Err(ForkError::Invalid("fork already exists".into()));
        }
        let f = fork_tree(&root, fork, s);
        crate::release::private_dir(&root)?;
        for dir in [&f.tree, &f.home, &f.meta] {
            crate::release::private_dir(dir)?;
        }
        std::fs::write(f.meta.join("repo"), s.git_dir.to_string_lossy().as_bytes())?;
        let mut r = self.repo(&s.git_dir);
        r.index = Some(f.index());
        r.ok(&["read-tree", base.as_str()])?;
        r.work_tree = Some(&f.tree);
        r.ok(&["checkout-index", "-a", "-f", "-u", "-q"])?;
        self.save_state(
            &f,
            &ForkState {
                schema: 1,
                session: s.session.clone(),
                fork: fork.clone(),
                base,
                head: None,
                project_path: s.project_path.clone(),
                owner: owner.clone(),
            },
        )?;
        if !owner.is_node() {
            self.grant(&f, owner)?;
        }
        Ok(f)
    }

    fn grant(&self, f: &ForkTree, slot: &SlotAccount) -> Result<()> {
        super::slots::grant_dirs(&[&f.tree, &f.home], slot)?;
        Ok(())
    }
    fn revoke(&self, f: &ForkTree) -> Result<()> {
        super::slots::revoke_dirs(&[&f.tree, &f.home])?;
        Ok(())
    }

    fn snapshot(&self, f: &ForkTree, message: &str) -> Result<Snapshot> {
        let git_dir = self.fork_git_dir(f)?;
        let mut state = self.state(f)?;
        let candidates = self.candidates(&git_dir, &f.tree, &f.index())?;
        let findings = prescan::scan(&f.tree, &candidates);
        if !findings.is_empty() {
            return Err(ForkError::Refused { findings });
        }
        let (tmp, tree, changed) = self.stage(&git_dir, &f.tree, &f.index(), None, &state.base)?;
        let head = match &state.head {
            Some(h) if self.tree_of(&git_dir, h)? == tree => h.clone(),
            _ => {
                let msg = format!("{}\n", message.trim());
                let c = self.commit_tree(&git_dir, &tree, &[&state.base], &msg, None)?;
                self.repo(&git_dir)
                    .ok(&["update-ref", &f.fork_ref(), c.as_str()])?;
                c
            }
        };
        tmp.persist(f.index())
            .map_err(|e| ForkError::Other(e.error.into()))?;
        state.head = Some(head.clone());
        self.save_state(f, &state)?;
        let added_bytes = changed
            .iter()
            .filter(|c| c.status != 'D')
            .map(|c| c.bytes)
            .sum();
        Ok(Snapshot {
            files: changed.len(),
            head,
            base: state.base,
            changed,
            added_bytes,
        })
    }

    fn diff(&self, f: &ForkTree, path: Option<&str>, limit: usize) -> Result<String> {
        let git_dir = self.fork_git_dir(f)?;
        let state = self.state(f)?;
        let head = state
            .head
            .ok_or_else(|| ForkError::Invalid("the fork has no snapshot yet".into()))?;
        let mut args = vec![
            "diff-tree",
            "-p",
            "-r",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--stat",
            state.base.as_str(),
            head.as_str(),
        ];
        if let Some(p) = path {
            args.extend(["--", p]);
        }
        let mut r = self.repo(&git_dir);
        r.attr_source = Some(EMPTY_TREE);
        self.empty_tree(&git_dir)?;
        let out = r.ok(&args)?;
        let mut text = String::from_utf8_lossy(&out).into_owned();
        if text.len() > limit {
            let cut = text[..limit].rfind('\n').unwrap_or(0);
            text.truncate(cut);
            text.push_str("\n[diff truncated]\n");
        }
        Ok(text)
    }

    fn merge(
        &self,
        s: &DevTree,
        expected_dev_head: &Commit,
        fork_head: &Commit,
        msg: &str,
    ) -> Result<Commit> {
        let current = self.dev_head(s)?;
        if &current != expected_dev_head {
            return Err(ForkError::Stale {
                current: Some(current),
            });
        }
        let base = self.parent(&s.git_dir, fork_head)?;
        let tree = self.merge_trees(
            &s.git_dir,
            base.as_str(),
            expected_dev_head.as_str(),
            fork_head.as_str(),
        )?;
        let commit = self.commit_tree(
            &s.git_dir,
            &tree,
            &[expected_dev_head, fork_head],
            &format!("{}\n", msg.trim()),
            None,
        )?;
        self.swap(&s.git_dir, &s.dev_ref(), &commit, expected_dev_head)?;
        Ok(commit)
    }

    fn apply(&self, s: &DevTree, from: &Commit, to: &Commit) -> Result<Vec<Change>> {
        let mut r = self.repo(&s.git_dir);
        r.work_tree = Some(&s.work_tree);
        r.index = Some(s.index.clone());
        let out = r.run(&["read-tree", "-m", "-u", from.as_str(), to.as_str()], None)?;
        if out.code != 0 {
            return Err(ForkError::Held(format!(
                "live edits in progress ({})",
                last_line(&out.stderr)
            )));
        }
        self.changes(&s.git_dir, from.as_str(), to.as_str())
    }

    fn rebase(&self, s: &DevTree, f: &ForkTree, onto: &Commit) -> Result<Snapshot> {
        let snap = self.snapshot(f, "Checkpoint before rebase")?;
        if &snap.base == onto {
            return Ok(snap);
        }
        let git_dir = self.fork_git_dir(f)?;
        if git_dir != s.git_dir {
            return Err(ForkError::Invalid(
                "fork belongs to another repository".into(),
            ));
        }
        let tree = self.merge_trees(
            &git_dir,
            snap.base.as_str(),
            onto.as_str(),
            snap.head.as_str(),
        )?;
        let mut r = self.repo(&git_dir);
        r.work_tree = Some(&f.tree);
        r.index = Some(f.index());
        let out = r.run(&["read-tree", "-m", "-u", snap.head.as_str(), &tree], None)?;
        if out.code != 0 {
            return Err(ForkError::Held(format!(
                "the fork changed during the rebase ({})",
                last_line(&out.stderr)
            )));
        }
        let head = self.commit_tree(
            &git_dir,
            &tree,
            &[onto],
            &format!("Rebased onto {}\n", onto.short()),
            None,
        )?;
        self.repo(&git_dir)
            .ok(&["update-ref", &f.fork_ref(), head.as_str()])?;
        let mut state = self.state(f)?;
        state.base = onto.clone();
        state.head = Some(head.clone());
        self.save_state(f, &state)?;
        let changed = self.changes(&git_dir, onto.as_str(), head.as_str())?;
        let added_bytes = changed
            .iter()
            .filter(|c| c.status != 'D')
            .map(|c| c.bytes)
            .sum();
        Ok(Snapshot {
            files: changed.len(),
            head,
            base: onto.clone(),
            changed,
            added_bytes,
        })
    }

    fn close(&self, f: &ForkTree, keep_snapshot: bool) -> Result<()> {
        let git_dir = self.fork_git_dir(f).ok();
        let _ = self.revoke(f);
        if !keep_snapshot && let Some(git_dir) = &git_dir {
            let _ = self
                .repo(git_dir)
                .run(&["update-ref", "-d", &f.fork_ref()], None);
        }
        remove_tree(&f.root)?;
        Ok(())
    }

    fn revert(
        &self,
        s: &DevTree,
        expected_dev_head: &Commit,
        bad: &Commit,
        msg: &str,
    ) -> Result<Commit> {
        let current = self.dev_head(s)?;
        if &current != expected_dev_head {
            return Err(ForkError::Stale {
                current: Some(current),
            });
        }
        let before = self.parent(&s.git_dir, bad)?;
        // Undo bad's changes on top of whatever came after it.
        let tree = self.merge_trees(
            &s.git_dir,
            bad.as_str(),
            expected_dev_head.as_str(),
            before.as_str(),
        )?;
        let commit = self.commit_tree(
            &s.git_dir,
            &tree,
            &[expected_dev_head],
            &format!("{}\n", msg.trim()),
            None,
        )?;
        self.swap(&s.git_dir, &s.dev_ref(), &commit, expected_dev_head)?;
        Ok(commit)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub struct Fixture {
        pub _dir: tempfile::TempDir,
        pub p: GitTreeProvider,
        pub s: DevTree,
    }
    fn write(root: &Path, path: &str, text: &str) {
        let file = prescan::join(root, path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, text).unwrap();
    }
    fn read(root: &Path, path: &str) -> String {
        std::fs::read_to_string(prescan::join(root, path)).unwrap()
    }
    const SCENE: &str = "%YAML 1.1\n--- !u!1 &1\nGameObject:\n  m_Name: Card\n  m_Layer: 0\n  m_IsActive: 1\n--- !u!4 &2\nTransform:\n  m_LocalPosition: {x: 0, y: 0, z: 0}\n";

    /// A dev tree like an engine session's: a node-private repository and a
    /// work tree with a Unity project under `unity-project/`.
    pub fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        // macOS's /var is a symlink; Windows' canonical form (\\?\) is not
        // a path git accepts.
        let base = if cfg!(windows) {
            dir.path().to_path_buf()
        } else {
            dir.path().canonicalize().unwrap()
        };
        let engine = base.join("engine");
        let p =
            GitTreeProvider::new("git".into(), &engine.join("repos").join(".node"), false).unwrap();
        let s = DevTree {
            session: "0123456789abcdef0123456789abcdef".into(),
            git_dir: engine.join("repos").join("k.git"),
            work_tree: engine.join("checkouts").join("k"),
            index: engine.join("repos").join("k.index"),
            project_path: "unity-project".into(),
            forks_root: engine.join("forks"),
        };
        std::fs::create_dir_all(&s.work_tree).unwrap();
        let mut c = p.command(&s.git_dir, Some(&s.work_tree), Some(&s.index));
        c.args(["init", "-q"]);
        assert!(c.status().unwrap().success());
        let w = &s.work_tree;
        write(w, ".gitignore", "unity-project/Library/\n");
        write(
            w,
            "unity-project/ProjectSettings/ProjectVersion.txt",
            "m_EditorVersion: 6000.6.5f1\n",
        );
        write(w, "unity-project/Assets/Scenes/Main.unity", SCENE);
        write(w, "unity-project/Assets/Scripts/Game.cs", "class Game {}\n");
        write(
            w,
            "unity-project/Assets/Data/levels.json",
            "{\"levels\": 1}\n",
        );
        write(w, "unity-project/Library/cache.bin", "ignored");
        let mut r = p.repo(&s.git_dir);
        r.work_tree = Some(&s.work_tree);
        r.index = Some(s.index.clone());
        r.ok(&["add", "-A"]).unwrap();
        let tree = String::from_utf8(r.ok(&["write-tree"]).unwrap()).unwrap();
        let c = p
            .commit_tree(&s.git_dir, tree.trim(), &[], "Initial\n", None)
            .unwrap();
        p.start_dev(&s, &c).unwrap();
        Fixture { _dir: dir, p, s }
    }
    fn fork(f: &Fixture, n: u8) -> ForkTree {
        let id = ForkId::parse(&format!("{:032x}", u128::from(n) + 0xabc)).unwrap();
        f.p.create(&f.s, &id, None, &SlotAccount::node()).unwrap()
    }

    #[test]
    fn git_versions_are_checked() {
        assert_eq!(parse_version("git version 2.45.1.windows.1"), Some((2, 45)));
        assert_eq!(parse_version("git version 2.53.0"), Some((2, 53)));
        assert_eq!(parse_version("nonsense"), None);
        fixture().p.check_version().unwrap();
    }

    #[test]
    fn fork_snapshot_merge_and_apply() {
        let f = fixture();
        let head0 = f.p.dev_head(&f.s).unwrap();
        let k = fork(&f, 1);
        // The fork tree has the dev tree's tracked files and no .git or Library.
        assert_eq!(
            read(&k.tree, "unity-project/Assets/Scripts/Game.cs"),
            "class Game {}\n"
        );
        assert!(!k.tree.join(".git").exists());
        assert!(!prescan::join(&k.tree, "unity-project/Library").exists());
        assert_eq!(k.cwd(), k.tree.join("unity-project"));
        write(&k.tree, "unity-project/Assets/Art/new.txt", "hello\n");
        write(
            &k.tree,
            "unity-project/Assets/Data/levels.json",
            "{\"levels\": 2}\n",
        );
        let snap = f.p.snapshot(&k, "Agent run").unwrap();
        assert_eq!(snap.base, head0);
        let changed: Vec<(&str, char)> = snap
            .changed
            .iter()
            .map(|c| (c.path.as_str(), c.status))
            .collect();
        assert_eq!(
            changed,
            [
                ("unity-project/Assets/Art/new.txt", 'A'),
                ("unity-project/Assets/Data/levels.json", 'M')
            ]
        );
        assert_eq!(snap.changed[0].bytes, 6);
        // Snapshot is idempotent while nothing changes.
        assert_eq!(f.p.snapshot(&k, "again").unwrap().head, snap.head);
        let diff = f.p.diff(&k, None, 1 << 20).unwrap();
        assert!(diff.contains("+{\"levels\": 2}"));
        assert!(
            f.p.diff(&k, None, 40)
                .unwrap()
                .ends_with("[diff truncated]\n")
        );
        // A live edit lands on the dev head meanwhile; the merge is clean.
        write(
            &f.s.work_tree,
            "unity-project/Assets/Scenes/Main.unity",
            &SCENE.replace("m_Layer: 0", "m_Layer: 5"),
        );
        let cap =
            f.p.capture_live(
                &f.s,
                &[Principal {
                    id: "gj:room:7".into(),
                    name: "Ada".into(),
                }],
            )
            .unwrap();
        assert_eq!(cap.changed.len(), 1);
        let head1 = cap.head;
        let merged = f.p.merge(&f.s, &head1, &snap.head, "Merge fork").unwrap();
        assert_eq!(f.p.dev_head(&f.s).unwrap(), merged);
        // Stale expected head: compare-and-swap refuses.
        assert!(matches!(
            f.p.merge(&f.s, &head1, &snap.head, "again"),
            Err(ForkError::Stale { .. })
        ));
        let applied = f.p.apply(&f.s, &head1, &merged).unwrap();
        assert_eq!(applied.len(), 2);
        assert_eq!(
            read(&f.s.work_tree, "unity-project/Assets/Art/new.txt"),
            "hello\n"
        );
        assert!(
            read(&f.s.work_tree, "unity-project/Assets/Scenes/Main.unity").contains("m_Layer: 5")
        );
        // Ignored Library content survives.
        assert!(prescan::join(&f.s.work_tree, "unity-project/Library/cache.bin").exists());
        // Nothing left to capture.
        assert!(f.p.capture_live(&f.s, &[]).unwrap().changed.is_empty());
        f.p.close(&k, true).unwrap();
        assert!(!k.root.exists());
        assert_eq!(f.p.rev(&f.s.git_dir, &k.fork_ref()).unwrap(), snap.head);
    }

    #[test]
    fn overlapping_scene_edits_conflict_and_leave_the_dev_head() {
        let f = fixture();
        let k = fork(&f, 2);
        write(
            &k.tree,
            "unity-project/Assets/Scenes/Main.unity",
            &SCENE.replace("m_Name: Card", "m_Name: Agent"),
        );
        let snap = f.p.snapshot(&k, "Agent").unwrap();
        write(
            &f.s.work_tree,
            "unity-project/Assets/Scenes/Main.unity",
            &SCENE.replace("m_Name: Card", "m_Name: Live"),
        );
        let head = f.p.capture_live(&f.s, &[]).unwrap().head;
        match f.p.merge(&f.s, &head, &snap.head, "Merge") {
            Err(ForkError::Conflict { paths }) => {
                assert_eq!(paths, ["unity-project/Assets/Scenes/Main.unity"])
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
        assert_eq!(f.p.dev_head(&f.s).unwrap(), head);
        // A .gitattributes `merge=union` in the dev head cannot hide it.
        write(&f.s.work_tree, ".gitattributes", "*.unity merge=union\n");
        let head = f.p.capture_live(&f.s, &[]);
        // .gitattributes is protected: left for the owner, not committed.
        let head = head.unwrap();
        assert_eq!(head.live_code_pending, [".gitattributes"]);
    }

    #[test]
    fn hostile_trees_are_refused() {
        let f = fixture();
        let k = fork(&f, 3);
        // A nested repository.
        let nested = prescan::join(&k.tree, "unity-project/Assets/sub");
        std::fs::create_dir_all(&nested).unwrap();
        let mut c = f.p.command(&nested.join(".git"), Some(&nested), None);
        c.args(["init", "-q"]);
        assert!(c.status().unwrap().success());
        write(&nested, "x.txt", "x");
        let nested_git = nested.join(".git");
        let mut r = f.p.repo(&nested_git);
        r.work_tree = Some(&nested);
        r.index = Some(nested.join(".git").join("index"));
        r.ok(&["add", "-A"]).unwrap();
        let t = String::from_utf8(r.ok(&["write-tree"]).unwrap()).unwrap();
        let c =
            f.p.commit_tree(&nested.join(".git"), t.trim(), &[], "x\n", None)
                .unwrap();
        r.ok(&["update-ref", "HEAD", c.as_str()]).unwrap();
        match f.p.snapshot(&k, "x") {
            Err(ForkError::Refused { findings }) => assert!(
                findings.iter().any(|x| x.rule == "nested-repository"),
                "{findings:?}"
            ),
            other => panic!("expected a refusal, got {other:?}"),
        }
        remove_tree(&nested).unwrap();
        // Case-only collisions.
        write(&k.tree, "unity-project/Assets/Data/LEVELS.json", "{}");
        match f.p.snapshot(&k, "x") {
            Err(ForkError::Refused { findings }) => {
                assert_eq!(findings[0].rule, "case-collision")
            }
            other => {
                // Case-insensitive filesystems (macOS default) merge the names.
                assert!(other.is_ok() || cfg!(not(target_os = "linux")), "{other:?}")
            }
        }
        let _ = std::fs::remove_file(prescan::join(
            &k.tree,
            "unity-project/Assets/Data/LEVELS.json",
        ));
        write(
            &k.tree,
            "unity-project/Assets/Data/levels.json",
            "{\"levels\": 1}\n",
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                &f.s.git_dir,
                prescan::join(&k.tree, "unity-project/Assets/repo"),
            )
            .unwrap();
            match f.p.snapshot(&k, "x") {
                Err(ForkError::Refused { findings }) => {
                    assert_eq!(findings[0].rule, "symlink-or-reparse-point")
                }
                other => panic!("expected a refusal, got {other:?}"),
            }
            std::fs::remove_file(prescan::join(&k.tree, "unity-project/Assets/repo")).unwrap();
            std::fs::write(f.s.forks_root.join("private.txt"), "node secret").unwrap();
            std::fs::hard_link(
                f.s.forks_root.join("private.txt"),
                prescan::join(&k.tree, "unity-project/Assets/copy.txt"),
            )
            .unwrap();
            match f.p.snapshot(&k, "x") {
                Err(ForkError::Refused { findings }) => assert_eq!(findings[0].rule, "hard-link"),
                other => panic!("expected a refusal, got {other:?}"),
            }
            std::fs::remove_file(prescan::join(&k.tree, "unity-project/Assets/copy.txt")).unwrap();
        }
        // A clean tree snapshots again.
        assert!(f.p.snapshot(&k, "x").unwrap().changed.is_empty());
    }

    #[test]
    fn repository_hooks_and_fsmonitor_never_run() {
        let f = fixture();
        let marker = f._dir.path().join("ran");
        let script = f._dir.path().join("hook.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let hooks = f.s.git_dir.join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        for hook in [
            "post-index-change",
            "pre-commit",
            "post-commit",
            "reference-transaction",
        ] {
            std::fs::copy(&script, hooks.join(hook)).unwrap();
        }
        // A config an attacker might plant in the repository.
        let config = f.s.git_dir.join("config");
        let mut text = std::fs::read_to_string(&config).unwrap();
        text.push_str(&format!(
            "[core]\n\tfsmonitor = {0}\n\thooksPath = {1}\n",
            script.display().to_string().replace('\\', "/"),
            hooks.display().to_string().replace('\\', "/")
        ));
        std::fs::write(&config, text).unwrap();
        let k = fork(&f, 4);
        write(&k.tree, "unity-project/Assets/Art/a.txt", "a");
        let snap = f.p.snapshot(&k, "x").unwrap();
        write(&f.s.work_tree, "unity-project/Assets/Art/b.txt", "b");
        let head = f.p.capture_live(&f.s, &[]).unwrap().head;
        let merged = f.p.merge(&f.s, &head, &snap.head, "m").unwrap();
        f.p.apply(&f.s, &head, &merged).unwrap();
        assert!(!marker.exists(), "a repository hook or fsmonitor ran");
    }

    #[test]
    fn live_capture_commits_content_and_holds_code() {
        let f = fixture();
        let head0 = f.p.dev_head(&f.s).unwrap();
        write(
            &f.s.work_tree,
            "unity-project/Assets/Scenes/Main.unity",
            &SCENE.replace("x: 0", "x: 3"),
        );
        write(
            &f.s.work_tree,
            "unity-project/Assets/Scripts/Game.cs",
            "class Game { int x; }\n",
        );
        write(&f.s.work_tree, "unity-project/Assets/.env", "TOKEN=1");
        let cap =
            f.p.capture_live(
                &f.s,
                &[Principal {
                    id: "usr_1".into(),
                    name: String::new(),
                }],
            )
            .unwrap();
        assert_ne!(cap.head, head0);
        assert_eq!(cap.changed.len(), 1);
        assert_eq!(
            cap.changed[0].path,
            "unity-project/Assets/Scenes/Main.unity"
        );
        assert_eq!(
            cap.live_code_pending,
            ["unity-project/Assets/Scripts/Game.cs"]
        );
        assert_eq!(cap.refused[0].rule, "credential-path");
        // A fork that changes the pending code file cannot be applied over it.
        let k = fork(&f, 5);
        write(
            &k.tree,
            "unity-project/Assets/Scripts/Game.cs",
            "class Game { int y; }\n",
        );
        let snap = f.p.snapshot(&k, "x").unwrap();
        let merged = f.p.merge(&f.s, &cap.head, &snap.head, "m").unwrap();
        assert!(matches!(
            f.p.apply(&f.s, &cap.head, &merged),
            Err(ForkError::Held(_))
        ));
        assert_eq!(
            read(&f.s.work_tree, "unity-project/Assets/Scripts/Game.cs"),
            "class Game { int x; }\n"
        );
    }

    #[test]
    fn rebase_brings_the_dev_head_into_the_fork() {
        let f = fixture();
        let a = fork(&f, 6);
        let b = fork(&f, 7);
        write(&a.tree, "unity-project/Assets/Art/a.txt", "a\n");
        write(&b.tree, "unity-project/Assets/Art/b.txt", "b\n");
        let sa = f.p.snapshot(&a, "a").unwrap();
        let head0 = f.p.dev_head(&f.s).unwrap();
        let head1 = f.p.merge(&f.s, &head0, &sa.head, "merge a").unwrap();
        f.p.apply(&f.s, &head0, &head1).unwrap();
        let rb = f.p.rebase(&f.s, &b, &head1).unwrap();
        assert_eq!(rb.base, head1);
        assert_eq!(rb.changed.len(), 1);
        assert_eq!(read(&b.tree, "unity-project/Assets/Art/a.txt"), "a\n");
        assert_eq!(read(&b.tree, "unity-project/Assets/Art/b.txt"), "b\n");
        // Rebasing again is a no-op; the fork keeps working afterwards.
        assert_eq!(f.p.rebase(&f.s, &b, &head1).unwrap().head, rb.head);
        write(&b.tree, "unity-project/Assets/Art/b.txt", "b2\n");
        let sb = f.p.snapshot(&b, "b2").unwrap();
        let head2 = f.p.merge(&f.s, &head1, &sb.head, "merge b").unwrap();
        f.p.apply(&f.s, &head1, &head2).unwrap();
        assert_eq!(
            read(&f.s.work_tree, "unity-project/Assets/Art/b.txt"),
            "b2\n"
        );
        // A conflicting rebase leaves the fork tree as it was.
        let c = fork(&f, 8);
        let d = fork(&f, 9);
        write(
            &c.tree,
            "unity-project/Assets/Data/levels.json",
            "{\"levels\": 3}\n",
        );
        write(
            &d.tree,
            "unity-project/Assets/Data/levels.json",
            "{\"levels\": 4}\n",
        );
        let sc = f.p.snapshot(&c, "c").unwrap();
        let head3 = f.p.merge(&f.s, &head2, &sc.head, "merge c").unwrap();
        assert!(matches!(
            f.p.rebase(&f.s, &d, &head3),
            Err(ForkError::Conflict { .. })
        ));
        assert_eq!(
            read(&d.tree, "unity-project/Assets/Data/levels.json"),
            "{\"levels\": 4}\n"
        );
    }

    #[test]
    fn revert_undoes_a_merge_after_later_live_edits() {
        let f = fixture();
        let k = fork(&f, 10);
        write(
            &k.tree,
            "unity-project/Assets/Scripts/Game.cs",
            "class Game { broken }\n",
        );
        let snap = f.p.snapshot(&k, "x").unwrap();
        let head0 = f.p.dev_head(&f.s).unwrap();
        let bad = f.p.merge(&f.s, &head0, &snap.head, "m").unwrap();
        f.p.apply(&f.s, &head0, &bad).unwrap();
        write(
            &f.s.work_tree,
            "unity-project/Assets/Art/later.txt",
            "later\n",
        );
        let later = f.p.capture_live(&f.s, &[]).unwrap().head;
        let reverted = f.p.revert(&f.s, &later, &bad, "Revert").unwrap();
        let changes = f.p.apply(&f.s, &later, &reverted).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(
            read(&f.s.work_tree, "unity-project/Assets/Scripts/Game.cs"),
            "class Game {}\n"
        );
        assert_eq!(
            read(&f.s.work_tree, "unity-project/Assets/Art/later.txt"),
            "later\n"
        );
    }
}
