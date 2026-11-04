//! Agent forks of a live engine session's dev tree (docs/AGENT-FORKS.md).
//!
//! The dev tree is the engine session's working tree (the Editor runs on it).
//! A *fork* is a separate writable copy of that tree at a dev-head commit,
//! owned by one principal, in which agent runs (Claude Code, Codex) work as a
//! low-privilege slot account. Submissions merge a fork's snapshot into the
//! dev head with a three-way merge and apply the result to the dev tree,
//! which the Editor then hot-refreshes.
//!
//! - [`ForkProvider`]: the OS-agnostic storage interface; [`git::GitTreeProvider`]
//!   (Windows, macOS, Linux without LayerFS) and [`layerfs::LayerFsProvider`]
//!   (Linux, not yet supported).
//! - [`classify`]: path classes and the secret scan for submissions.
//! - [`relay`]: the loopback gateway relay to Xava Inference.
//! - [`harness`]: per-run homes and harness configuration.
//! - [`slots`]: slot accounts and the run spawner (Windows: `eb-agent-NN`).
//! - [`runner`]: one agent run end to end.
//! - [`submit`]: the submission pipeline (checks, merge, apply, refresh,
//!   compile check, automatic revert).
//! - [`executor`]: the node side of the fork protocol (`engine-poll` `forks`,
//!   `fork-report`, `fork-events`, `fork-artifact`).
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub mod classify;
pub mod executor;
pub mod git;
pub mod harness;
pub mod layerfs;
pub mod prescan;
pub mod relay;
pub mod runner;
pub mod slots;
pub mod submit;
#[cfg(windows)]
pub mod windows;

/// A 40-hex git commit id.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(transparent)]
pub struct Commit(pub String);
impl Commit {
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.len() == 40
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            Ok(Self(s.to_owned()))
        } else {
            Err(ForkError::Invalid("not a commit id".into()))
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn short(&self) -> &str {
        &self.0[..12]
    }
}
impl std::fmt::Display for Commit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A fork id: 32 lowercase hex characters (the controller's id).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ForkId(String);
impl ForkId {
    pub fn parse(s: &str) -> Result<Self> {
        if hex32(s) {
            Ok(Self(s.to_owned()))
        } else {
            Err(ForkError::Invalid(
                "fork id must be 32 lowercase hex".into(),
            ))
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
pub fn hex32(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Who an edit or capture is attributed to (`gj:<room>:<user>` or an
/// EngineBox member id). Never a credential.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    pub id: String,
    #[serde(default)]
    pub name: String,
}

/// The account an agent run executes as.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotAccount {
    /// `eb-agent-01`…; on a node without slot accounts, `node` (the node's
    /// own account; only for the session starter's forks).
    pub name: String,
    /// Windows: the account's SID string, once resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sid: Option<String>,
    /// Unix: the slot's uid, when the node has per-slot uids.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
}
impl SlotAccount {
    /// The node's own account (isolation `none`).
    pub fn node() -> Self {
        Self {
            name: "node".into(),
            sid: None,
            uid: None,
        }
    }
    pub fn is_node(&self) -> bool {
        self.name == "node"
    }
}

/// The engine session's dev tree as a git work tree of a node-private
/// repository (no `.git` inside the tree).
#[derive(Clone, Debug)]
pub struct DevTree {
    /// The engine session id (32 hex).
    pub session: String,
    /// `work\engine\repos\<key>.git` (node-only, bare).
    pub git_dir: PathBuf,
    /// `work\engine\checkouts\<key>` (the Editor's tree; the repository root).
    pub work_tree: PathBuf,
    /// `work\engine\repos\<key>.index`.
    pub index: PathBuf,
    /// The Unity project inside the repository (`P`, `/`-separated, may be
    /// empty for a project at the root).
    pub project_path: String,
    /// `work\engine\forks`: where fork trees, homes and metadata live.
    pub forks_root: PathBuf,
}
impl DevTree {
    /// `refs/eb/dev/<session>`: the dev head.
    pub fn dev_ref(&self) -> String {
        format!("refs/eb/dev/{}", self.session)
    }
}

/// A fork's tree and private metadata.
#[derive(Clone, Debug)]
pub struct ForkTree {
    pub id: ForkId,
    pub session: String,
    /// `forks\<fork>`.
    pub root: PathBuf,
    /// `forks\<fork>\tree`: the only directory the agent may write besides
    /// its home.
    pub tree: PathBuf,
    /// `forks\<fork>\home`.
    pub home: PathBuf,
    /// `forks\<fork>\meta` (node only): `index`, `state.json`.
    pub meta: PathBuf,
    pub project_path: String,
}
impl ForkTree {
    /// The fork `id` of dev tree `s` (its directories need not exist).
    pub fn at(s: &DevTree, id: &ForkId) -> Self {
        let root = s.forks_root.join(id.as_str());
        Self {
            id: id.clone(),
            session: s.session.clone(),
            tree: root.join("tree"),
            home: root.join("home"),
            meta: root.join("meta"),
            root,
            project_path: s.project_path.clone(),
        }
    }
    /// Created earlier (its metadata exists).
    pub fn exists(&self) -> bool {
        self.meta.join("state.json").is_file()
    }
    pub fn fork_ref(&self) -> String {
        format!("refs/eb/forks/{}", self.id.as_str())
    }
    pub fn index(&self) -> PathBuf {
        self.meta.join("index")
    }
    /// The agent's working directory: the Unity project inside the tree.
    pub fn cwd(&self) -> PathBuf {
        let p = self.project_path.trim_matches('/');
        if p.is_empty() {
            self.tree.clone()
        } else {
            p.split('/').fold(self.tree.clone(), |d, c| d.join(c))
        }
    }
}

/// One changed path between two commits (`diff-tree --raw`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub path: String,
    /// `A`, `M`, `D` or `T` (type change).
    pub status: char,
    /// The new mode (the old one for a deletion), e.g. `0o100644`.
    pub mode: u32,
    /// The new content is a Git LFS pointer.
    pub lfs: bool,
    /// The new content's size (the LFS object's size for a pointer).
    pub bytes: u64,
}

/// A live capture: the dev head after committing the Editor's saves.
#[derive(Clone, Debug, Serialize)]
pub struct Capture {
    pub head: Commit,
    pub changed: Vec<Change>,
    /// Saved code-class paths left uncommitted for the owner.
    pub live_code_pending: Vec<String>,
    /// Saved paths that may never be committed (symlinks, credential files…).
    pub refused: Vec<classify::Finding>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub head: Commit,
    pub base: Commit,
    pub changed: Vec<Change>,
    pub files: usize,
    pub added_bytes: u64,
}

/// Fork operation errors. `Conflict` and `Refused` are expected outcomes the
/// controller shows to people; the rest are failures.
#[derive(Debug)]
pub enum ForkError {
    /// The three-way merge has conflicts in these paths.
    Conflict {
        paths: Vec<String>,
    },
    /// The dev head moved since the caller looked (compare-and-swap lost).
    Stale {
        current: Option<Commit>,
    },
    /// The tree breaks a rule (pre-scan, classification or secret scan).
    Refused {
        findings: Vec<classify::Finding>,
    },
    /// The dev tree has live edits in the way; try again later.
    Held(String),
    /// This provider or platform cannot do this yet.
    Unsupported(String),
    Invalid(String),
    Other(anyhow::Error),
}
impl std::fmt::Display for ForkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict { paths } => write!(f, "merge conflict in {}", paths.join(", ")),
            Self::Stale { .. } => f.write_str("the dev head moved; retry against the new head"),
            Self::Refused { findings } => write!(
                f,
                "refused: {}",
                findings
                    .iter()
                    .take(8)
                    .map(|x| format!("{} ({})", x.path, x.rule))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Held(reason) => write!(f, "held: {reason}"),
            Self::Unsupported(what) => write!(f, "unsupported: {what}"),
            Self::Invalid(what) => write!(f, "invalid: {what}"),
            Self::Other(e) => write!(f, "{e:#}"),
        }
    }
}
impl std::error::Error for ForkError {}
impl From<anyhow::Error> for ForkError {
    fn from(e: anyhow::Error) -> Self {
        Self::Other(e)
    }
}
impl From<std::io::Error> for ForkError {
    fn from(e: std::io::Error) -> Self {
        Self::Other(e.into())
    }
}
impl ForkError {
    /// The fork-report `state`/`error` code for this outcome.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Conflict { .. } => "conflict",
            Self::Stale { .. } => "stale",
            Self::Refused { .. } => "refused",
            Self::Held(_) => "held",
            Self::Unsupported(_) => "unsupported",
            Self::Invalid(_) => "invalid",
            Self::Other(_) => "failed",
        }
    }
}
pub type Result<T, E = ForkError> = std::result::Result<T, E>;

/// Storage for forks of a dev tree (contract §2.1). `apply` is always called
/// inside the Editor bridge's source-lock handshake.
pub trait ForkProvider: Send + Sync {
    /// `git` or `layerfs`, for the heartbeat.
    fn kind(&self) -> &'static str;
    /// Commit the dev tree's saved changes (only those passing live-capture
    /// rules) onto the dev head.
    fn capture_live(&self, s: &DevTree, attribution: &[Principal]) -> Result<Capture>;
    /// A new fork at `base` (default: the current dev head).
    fn create(
        &self,
        s: &DevTree,
        fork: &ForkId,
        base: Option<&Commit>,
        owner: &SlotAccount,
    ) -> Result<ForkTree>;
    /// Hand the tree (and home) to a slot account, or take it back.
    fn grant(&self, f: &ForkTree, slot: &SlotAccount) -> Result<()>;
    fn revoke(&self, f: &ForkTree) -> Result<()>;
    /// Pre-scan, then record the tree as a commit on `refs/eb/forks/<id>`.
    fn snapshot(&self, f: &ForkTree, message: &str) -> Result<Snapshot>;
    fn diff(&self, f: &ForkTree, path: Option<&str>, limit: usize) -> Result<String>;
    /// Three-way merge of `fork_head` into `expected_dev_head` (no work-tree
    /// change); moves the dev head with compare-and-swap.
    fn merge(
        &self,
        s: &DevTree,
        expected_dev_head: &Commit,
        fork_head: &Commit,
        msg: &str,
    ) -> Result<Commit>;
    /// Move the dev tree's files from `from` to `to`; refuses if the tree
    /// differs from `from` on a path that changes.
    fn apply(&self, s: &DevTree, from: &Commit, to: &Commit) -> Result<Vec<Change>>;
    /// Three-way of `onto` into the fork tree, or `Conflict`.
    fn rebase(&self, s: &DevTree, f: &ForkTree, onto: &Commit) -> Result<Snapshot>;
    fn close(&self, f: &ForkTree, keep_snapshot: bool) -> Result<()>;
    /// A commit on the dev head that undoes `bad` (a merge commit: back to its
    /// first parent's tree for the paths it changed). Used for automatic
    /// reverts after a compile failure.
    fn revert(
        &self,
        s: &DevTree,
        expected_dev_head: &Commit,
        bad: &Commit,
        msg: &str,
    ) -> Result<Commit>;
    /// The current dev head.
    fn dev_head(&self, s: &DevTree) -> Result<Commit>;
    /// Move the dev head from `expected` to `to` (compare-and-swap); used to
    /// rewind a merge whose files could not be applied yet (`held`).
    fn set_dev_head(&self, s: &DevTree, expected: &Commit, to: &Commit) -> Result<()>;
}
