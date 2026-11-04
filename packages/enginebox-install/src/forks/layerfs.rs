//! `LayerFsProvider`: forks on Linux engine nodes as LayerFS branches of the
//! project's Store (contract §2.3). **Not supported yet** (WS-10): every
//! operation returns [`ForkError::Unsupported`], and the node never selects
//! this provider. See docs/AGENT-FORKS.md "Linux (LayerFS)" for the mapping
//! onto `enginebox-storage-layerfs` and the gap to qualify (merging into a
//! branch whose workspace is open and mounted).
use super::{
    Capture, Change, Commit, DevTree, ForkError, ForkId, ForkProvider, ForkTree, Principal, Result,
    SlotAccount, Snapshot,
};

/// TODO(WS-10): `capture_live` = `checkpoint dev-<session>`; `create` =
/// `fork <project> fork-<id> --from dev-<session>` in a LayerFS sandbox
/// container; `snapshot` = `checkpoint` + `workspace-export`; `merge` =
/// native `branch.merge` with an `expected_main`-style guard (until LayerFS
/// can merge into an open branch: "dev = main" with `promote --merge
/// expected_main` and the `updateMode: "files"` follower as `apply`).
#[derive(Default)]
pub struct LayerFsProvider;

fn unsupported<T>() -> Result<T> {
    Err(ForkError::Unsupported(
        "LayerFS forks are not implemented yet (WS-10); this node uses git forks".into(),
    ))
}

impl ForkProvider for LayerFsProvider {
    fn kind(&self) -> &'static str {
        "layerfs"
    }
    fn capture_live(&self, _: &DevTree, _: &[Principal]) -> Result<Capture> {
        unsupported()
    }
    fn create(
        &self,
        _: &DevTree,
        _: &ForkId,
        _: Option<&Commit>,
        _: &SlotAccount,
    ) -> Result<ForkTree> {
        unsupported()
    }
    fn grant(&self, _: &ForkTree, _: &SlotAccount) -> Result<()> {
        unsupported()
    }
    fn revoke(&self, _: &ForkTree) -> Result<()> {
        unsupported()
    }
    fn snapshot(&self, _: &ForkTree, _: &str) -> Result<Snapshot> {
        unsupported()
    }
    fn diff(&self, _: &ForkTree, _: Option<&str>, _: usize) -> Result<String> {
        unsupported()
    }
    fn merge(&self, _: &DevTree, _: &Commit, _: &Commit, _: &str) -> Result<Commit> {
        unsupported()
    }
    fn apply(&self, _: &DevTree, _: &Commit, _: &Commit) -> Result<Vec<Change>> {
        unsupported()
    }
    fn rebase(&self, _: &DevTree, _: &ForkTree, _: &Commit) -> Result<Snapshot> {
        unsupported()
    }
    fn close(&self, _: &ForkTree, _: bool) -> Result<()> {
        unsupported()
    }
    fn revert(&self, _: &DevTree, _: &Commit, _: &Commit, _: &str) -> Result<Commit> {
        unsupported()
    }
    fn dev_head(&self, _: &DevTree) -> Result<Commit> {
        unsupported()
    }
    fn set_dev_head(&self, _: &DevTree, _: &Commit, _: &Commit) -> Result<()> {
        unsupported()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_operation_is_unsupported() {
        let p = LayerFsProvider;
        assert_eq!(p.kind(), "layerfs");
        let s = DevTree {
            session: "0".repeat(32),
            git_dir: "/x".into(),
            work_tree: "/x".into(),
            index: "/x".into(),
            project_path: String::new(),
            forks_root: "/x".into(),
        };
        assert!(matches!(p.dev_head(&s), Err(ForkError::Unsupported(_))));
        assert!(matches!(
            p.capture_live(&s, &[]),
            Err(ForkError::Unsupported(_))
        ));
    }
}
