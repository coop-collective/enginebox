#![cfg(unix)]
use anyhow::{Result, ensure};
use enginebox_storage_layerfs::{
    source,
    sync::{Outbox, Remote, Status},
};
use serde_json::Value;
use std::{
    cell::Cell,
    fs,
    path::{Path, PathBuf},
};
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn git(repo: &Path, args: &[&str]) -> String {
    String::from_utf8(source::git(repo, args).unwrap())
        .unwrap()
        .trim()
        .into()
}
struct LocalRemote {
    repo: PathBuf,
    pushes: Cell<u32>,
    lose_ack: Cell<bool>,
}
impl Remote for LocalRemote {
    fn head(&self) -> Result<String> {
        Ok(git(&self.repo, &["rev-parse", "main"]))
    }
    fn fetch(&self, repo: &Path, commit: &str, reference: &str) -> Result<()> {
        source::git(
            repo,
            &[
                "-c",
                "protocol.file.allow=always",
                "fetch",
                "--no-tags",
                self.repo.to_str().unwrap(),
                &format!("{commit}:{reference}"),
            ],
        )?;
        Ok(())
    }
    fn push(&self, repo: &Path, expected: &str, commit: &str) -> Result<()> {
        ensure!(self.head()? == expected, "Remote compare-and-swap failed");
        source::git(repo, &["merge-base", "--is-ancestor", expected, commit])?;
        self.fetch_into_remote(repo, commit)?;
        source::git(
            &self.repo,
            &["update-ref", "refs/heads/main", commit, expected],
        )?;
        self.pushes.set(self.pushes.get() + 1);
        if self.lose_ack.replace(false) {
            anyhow::bail!("Simulated response loss after accepted push");
        }
        Ok(())
    }
}
impl LocalRemote {
    fn fetch_into_remote(&self, repo: &Path, commit: &str) -> Result<()> {
        source::git(
            &self.repo,
            &[
                "-c",
                "protocol.file.allow=always",
                "fetch",
                repo.to_str().unwrap(),
                commit,
            ],
        )?;
        Ok(())
    }
}
struct Fixture {
    _root: tempfile::TempDir,
    remote: LocalRemote,
    source: PathBuf,
    outbox: Outbox,
    base: String,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("remote");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]);
        fs::write(repo.join("shared.txt"), "base\n").unwrap();
        fs::write(repo.join("other.txt"), "unchanged\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "base"]);
        let base = git(&repo, &["rev-parse", "HEAD"]);
        let source = root.path().join("snapshot");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("shared.txt"), "base\n").unwrap();
        fs::write(source.join("other.txt"), "unchanged\n").unwrap();
        let outbox = Outbox::open(&root.path().join("outbox"), &base).unwrap();
        Self {
            _root: root,
            remote: LocalRemote {
                repo,
                pushes: Cell::new(0),
                lose_ack: Cell::new(false),
            },
            source,
            outbox,
            base,
        }
    }
    fn enqueue(&self) -> String {
        self.outbox.configure(true).unwrap();
        let result = self.outbox.enqueue(1, "layerfs-r1", &self.source).unwrap();
        result["active"]["id"].as_str().unwrap().into()
    }
    fn upstream(&self, path: &str, text: &str) {
        fs::write(self.remote.repo.join(path), text).unwrap();
        git(&self.remote.repo, &["add", "--all"]);
        git(&self.remote.repo, &["commit", "-m", "upstream"]);
    }
    fn accept(&self, id: &str) -> Value {
        serde_json::to_value(
            self.outbox
                .accept(id, |attempt, files| {
                    assert_eq!(attempt.layerfs_revision, "layerfs-r1");
                    assert!(!files.join(".git").exists());
                    Ok(("layerfs-accepted-resolution".into(), 2))
                })
                .unwrap(),
        )
        .unwrap()
    }
}
#[test]
fn disabled_sync_has_no_environment_and_duplicate_promotions_are_idempotent() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let disabled = f.outbox.enqueue(1, "r1", &f.source).unwrap();
    assert_eq!(disabled["active"], Value::Null);
    assert_eq!(f.remote.pushes.get(), 0);
    f.outbox.configure(true).unwrap();
    let a = f.outbox.enqueue(2, "r2", &f.source).unwrap();
    assert_eq!(
        a,
        f.outbox
            .enqueue(2, "r2", Path::new("/does-not-exist"))
            .unwrap()
    );
    assert!(f.outbox.enqueue(2, "different", &f.source).is_err());
}
#[test]
fn exact_files_are_exported_and_nonconflicting_upstream_changes_survive() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    fs::write(f.source.join("local.bin"), [0, 255, 1, 2]).unwrap();
    fs::write(f.source.join("run.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(f.source.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("shared.txt", f.source.join("link")).unwrap();
    let id = f.enqueue();
    f.upstream("remote.txt", "remote change\n");
    assert_eq!(
        f.outbox.prepare(&id, &f.remote).unwrap().status,
        Status::AwaitingAcceptance
    );
    f.outbox
        .accept(&id, |_, files| {
            assert_eq!(fs::read(files.join("local.bin"))?, [0, 255, 1, 2]);
            assert_eq!(
                fs::read_to_string(files.join("remote.txt"))?,
                "remote change\n"
            );
            assert!(fs::metadata(files.join("run.sh"))?.permissions().mode() & 0o111 != 0);
            assert!(
                fs::symlink_metadata(files.join("link"))?
                    .file_type()
                    .is_symlink()
            );
            assert!(!files.join(".git").exists());
            Ok(("layerfs-r2".into(), 2))
        })
        .unwrap();
    let done = f.outbox.deliver(&id, &f.remote).unwrap();
    assert_eq!(done.status, Status::Delivered);
    assert_eq!(
        git(&f.remote.repo, &["show", "main:remote.txt"]),
        "remote change"
    );
    assert_eq!(
        f.outbox.deliver(&id, &f.remote).unwrap().delivery_commit,
        done.delivery_commit
    );
    assert_eq!(f.remote.pushes.get(), 1);
}
#[test]
fn conflicts_require_specialized_agent_resolution_and_acceptance_back_into_layerfs() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    fs::write(f.source.join("shared.txt"), "LayerFS edit\n").unwrap();
    let id = f.enqueue();
    f.upstream("shared.txt", "GitHub edit\n");
    let a = f.outbox.prepare(&id, &f.remote).unwrap();
    assert_eq!(a.status, Status::NeedsAgent);
    assert_eq!(a.conflicts, ["shared.txt"]);
    let env = f.outbox.worktree(&id).unwrap();
    assert_eq!(git(&env, &["show", ":1:shared.txt"]), "base");
    assert_eq!(git(&env, &["show", ":2:shared.txt"]), "LayerFS edit");
    assert_eq!(git(&env, &["show", ":3:shared.txt"]), "GitHub edit");
    assert!(
        f.outbox
            .accept(&id, |_, _| panic!(
                "Unresolved content must not reach LayerFS"
            ))
            .is_err()
    );
    git(&env, &["add", "shared.txt"]);
    assert!(
        f.outbox
            .accept(&id, |_, _| panic!("Markers must not reach LayerFS"))
            .is_err()
    );
    // Simulate the specialized agent's result, without inference or credentials.
    fs::write(env.join("shared.txt"), "LayerFS edit\nGitHub edit\n").unwrap();
    git(&env, &["add", "shared.txt"]);
    f.outbox
        .accept(&id, |_, files| {
            assert_eq!(
                fs::read_to_string(files.join("shared.txt"))?,
                "LayerFS edit\nGitHub edit\n"
            );
            Ok(("layerfs-resolved".into(), 2))
        })
        .unwrap();
    assert_eq!(f.remote.pushes.get(), 0);
    f.outbox.deliver(&id, &f.remote).unwrap();
    assert_eq!(
        git(&f.remote.repo, &["show", "main:shared.txt"]),
        "LayerFS edit\nGitHub edit"
    );
}
#[test]
fn delete_edit_conflicts_are_not_silently_resolved() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    fs::remove_file(f.source.join("shared.txt")).unwrap();
    let id = f.enqueue();
    f.upstream("shared.txt", "upstream edit");
    assert_eq!(
        f.outbox.prepare(&id, &f.remote).unwrap().status,
        Status::NeedsAgent
    );
    assert!(f.outbox.deliver(&id, &f.remote).is_err());
    assert_eq!(f.remote.pushes.get(), 0);
}
#[test]
fn layerfs_advancing_fences_old_agent_result_and_new_attempt_uses_latest_snapshot() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    fs::write(f.source.join("shared.txt"), "ours").unwrap();
    let id = f.enqueue();
    f.upstream("shared.txt", "theirs");
    f.outbox.prepare(&id, &f.remote).unwrap();
    fs::write(f.source.join("new.txt"), "new accepted main").unwrap();
    let next = f.outbox.enqueue(2, "layerfs-r2", &f.source).unwrap();
    assert_ne!(next["active"]["id"], id);
    assert_eq!(f.outbox.read(&id).unwrap().status, Status::Stale);
    assert!(
        f.outbox
            .accept(&id, |_, _| panic!(
                "Stale agent result must not be accepted"
            ))
            .is_err()
    );
    assert_eq!(f.remote.pushes.get(), 0);
}
#[test]
fn remote_advance_during_resolution_retains_work_and_requires_new_reconciliation() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let id = f.enqueue();
    f.outbox.prepare(&id, &f.remote).unwrap();
    f.accept(&id);
    f.upstream("late.txt", "remote changed again");
    assert!(f.outbox.deliver(&id, &f.remote).is_err());
    assert_eq!(f.outbox.read(&id).unwrap().status, Status::Stale);
    assert_eq!(f.remote.pushes.get(), 0);
    let retry = f.outbox.retry(&id).unwrap();
    let next = retry["active"]["id"].as_str().unwrap();
    assert_ne!(id, next);
    assert_eq!(
        f.outbox.prepare(next, &f.remote).unwrap().status,
        Status::AwaitingAcceptance
    );
    assert!(f.outbox.worktree(&id).unwrap().exists());
}
#[test]
fn lost_push_response_reuses_exact_commit_after_restart() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    fs::write(f.source.join("new.txt"), "accepted").unwrap();
    let id = f.enqueue();
    f.outbox.prepare(&id, &f.remote).unwrap();
    f.accept(&id);
    f.remote.lose_ack.set(true);
    assert!(f.outbox.deliver(&id, &f.remote).is_err());
    let pending = f.outbox.read(&id).unwrap();
    assert_eq!(pending.status, Status::Delivering);
    assert_eq!(
        f.remote.head().unwrap(),
        pending.delivery_commit.clone().unwrap()
    );
    let reopened = Outbox::open(&f._root.path().join("outbox"), &f.base).unwrap();
    let done = reopened.deliver(&id, &f.remote).unwrap();
    assert_eq!(done.delivery_commit, pending.delivery_commit);
    assert_eq!(done.status, Status::Delivered);
    assert_eq!(f.remote.pushes.get(), 1);
}
#[test]
fn acceptance_failure_or_disabling_sync_never_pushes() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let id = f.enqueue();
    f.outbox.prepare(&id, &f.remote).unwrap();
    assert!(
        f.outbox
            .accept(&id, |_, _| anyhow::bail!(
                "LayerFS compare-and-swap refused"
            ))
            .is_err()
    );
    assert_eq!(f.outbox.read(&id).unwrap().status, Status::Accepting);
    assert!(f.outbox.deliver(&id, &f.remote).is_err());
    assert_eq!(f.remote.pushes.get(), 0);
    let other = Fixture::new();
    let id = other.enqueue();
    other.outbox.prepare(&id, &other.remote).unwrap();
    other.accept(&id);
    other.outbox.configure(false).unwrap();
    assert!(other.outbox.deliver(&id, &other.remote).is_err());
    assert_eq!(other.remote.pushes.get(), 0);
}
#[test]
fn secret_paths_and_escaping_symlinks_cannot_enter_reconciliation_snapshot() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    f.outbox.configure(true).unwrap();
    fs::write(f.source.join(".env"), "synthetic credential").unwrap();
    assert!(f.outbox.enqueue(1, "r1", &f.source).is_err());
    fs::remove_file(f.source.join(".env")).unwrap();
    std::os::unix::fs::symlink("../../secret", f.source.join("escape")).unwrap();
    assert!(f.outbox.enqueue(1, "r1", &f.source).is_err());
}

#[test]
fn interrupted_connection_receipt_recovers_the_accepted_sequence_without_exporting_twice() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let id = f.enqueue();
    f.outbox.prepare(&id, &f.remote).unwrap();
    f.accept(&id);
    // Simulate a crash after the Ready attempt is durable, before updating its
    // connection. Delivery must repair both receipts before acknowledging sync.
    let connection = f._root.path().join("outbox/connection.json");
    let mut receipt = enginebox_core::state::read_json(&connection).unwrap();
    receipt["latest_sequence"] = serde_json::json!(1);
    receipt["latest_revision"] = serde_json::json!("layerfs-r1");
    enginebox_core::state::write_json(&connection, &receipt).unwrap();
    f.outbox.deliver(&id, &f.remote).unwrap();
    let repeated = f
        .outbox
        .enqueue(2, "layerfs-accepted-resolution", Path::new("/not-read"))
        .unwrap();
    assert_eq!(repeated["active"]["id"], id);
    assert_eq!(repeated["active"]["status"], "delivered");
    assert_eq!(f.remote.pushes.get(), 1);
}
#[test]
fn interrupted_acceptance_reuses_frozen_candidate_instead_of_agent_worktree() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    fs::write(f.source.join("accepted.txt"), "frozen result").unwrap();
    let id = f.enqueue();
    f.outbox.prepare(&id, &f.remote).unwrap();
    assert!(
        f.outbox
            .accept(&id, |_, files| {
                assert_eq!(
                    fs::read_to_string(files.join("accepted.txt"))?,
                    "frozen result"
                );
                anyhow::bail!("Lost LayerFS acknowledgement")
            })
            .is_err()
    );
    let candidate = f.outbox.read(&id).unwrap().candidate;
    let env = f.outbox.worktree(&id).unwrap();
    fs::write(env.join("accepted.txt"), "later agent edit").unwrap();
    f.outbox
        .accept(&id, |_, files| {
            assert_eq!(
                fs::read_to_string(files.join("accepted.txt"))?,
                "frozen result"
            );
            Ok(("reconciled".into(), 2))
        })
        .unwrap();
    assert_eq!(f.outbox.read(&id).unwrap().candidate, candidate);
    f.outbox.deliver(&id, &f.remote).unwrap();
    assert_eq!(
        git(&f.remote.repo, &["show", "main:accepted.txt"]),
        "frozen result"
    );
}
#[test]
fn failed_new_snapshot_does_not_stale_the_previous_pending_attempt() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let id = f.enqueue();
    fs::write(f.source.join(".env"), "synthetic secret").unwrap();
    assert!(f.outbox.enqueue(2, "new-main", &f.source).is_err());
    assert_eq!(f.outbox.read(&id).unwrap().status, Status::Queued);
    assert_eq!(f.outbox.status().unwrap()["active"]["id"], id);
}
#[test]
fn a_valid_resolution_can_delete_the_entire_source_tree() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    fs::remove_file(f.source.join("shared.txt")).unwrap();
    fs::remove_file(f.source.join("other.txt")).unwrap();
    let id = f.enqueue();
    f.outbox.prepare(&id, &f.remote).unwrap();
    f.outbox
        .accept(&id, |_, files| {
            assert!(fs::read_dir(files)?.next().is_none());
            Ok(("empty-main".into(), 2))
        })
        .unwrap();
    f.outbox.deliver(&id, &f.remote).unwrap();
    assert!(git(&f.remote.repo, &["ls-tree", "-r", "main"]).is_empty());
}
#[test]
fn agent_git_configuration_cannot_execute_on_the_sync_host() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let id = f.enqueue();
    f.outbox.prepare(&id, &f.remote).unwrap();
    let env = f.outbox.worktree(&id).unwrap();
    let canary = f._root.path().join("must-not-exist");
    fs::write(
        env.join(".git/config"),
        format!(
            "[core]\nrepositoryformatversion = 0\nfsmonitor = touch {}\n",
            canary.display()
        ),
    )
    .unwrap();
    f.accept(&id);
    assert!(!canary.exists());
    let other = Fixture::new();
    let id = other.enqueue();
    other.outbox.prepare(&id, &other.remote).unwrap();
    let metadata = other.outbox.worktree(&id).unwrap().join(".git");
    fs::write(
        metadata.join("objects/info/alternates"),
        "/private/foreign-objects",
    )
    .unwrap();
    assert!(
        other
            .outbox
            .accept(&id, |_, _| panic!("Foreign objects must be refused"))
            .is_err()
    );
}

#[test]
fn source_resolution_preserves_ignored_layerfs_dependencies_and_data() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    fs::write(f.source.join(".gitignore"), "cache/\n*.db\n").unwrap();
    fs::create_dir(f.source.join("cache")).unwrap();
    fs::write(f.source.join("cache/deps.bin"), "prepared dependencies").unwrap();
    fs::write(f.source.join("local.db"), "private app data fixture").unwrap();
    git(&f.remote.repo, &["rm", "other.txt"]);
    git(&f.remote.repo, &["commit", "-m", "Remote source deletion"]);
    let id = f.enqueue();
    f.outbox.prepare(&id, &f.remote).unwrap();
    f.outbox
        .accept(&id, |_, files| {
            assert!(!files.join("cache").exists());
            assert!(!files.join("local.db").exists());
            let removed =
                enginebox_core::state::read_json(&files.parent().unwrap().join("remove.json"))?;
            assert_eq!(removed, serde_json::json!(["other.txt"]));
            Ok(("source-only-resolution".into(), 2))
        })
        .unwrap();
    f.outbox.deliver(&id, &f.remote).unwrap();
    assert!(source::git(&f.remote.repo, &["cat-file", "-e", "main:cache/deps.bin"]).is_err());
}
#[test]
fn upstream_files_cannot_overwrite_ignored_layerfs_data() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    fs::write(f.source.join(".gitignore"), "local.db\n").unwrap();
    fs::write(f.source.join("local.db"), "retained local data").unwrap();
    let id = f.enqueue();
    f.upstream("local.db", "new remote source file");
    f.outbox.prepare(&id, &f.remote).unwrap();
    assert!(
        f.outbox
            .accept(&id, |_, _| panic!("Retained data must not be overwritten"))
            .unwrap_err()
            .to_string()
            .contains("collides")
    );
    assert_eq!(f.remote.pushes.get(), 0);
}
#[test]
fn damaged_source_or_frozen_resolution_is_never_published_or_retrusted_by_retry() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let id = f.enqueue();
    let dir = f
        .outbox
        .worktree(&id)
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    fs::write(dir.join("layerfs/shared.txt"), "damaged").unwrap();
    assert!(f.outbox.prepare(&id, &f.remote).is_err());
    assert!(f.outbox.retry(&id).is_err());
    let other = Fixture::new();
    let id = other.enqueue();
    other.outbox.prepare(&id, &other.remote).unwrap();
    assert!(
        other
            .outbox
            .accept(&id, |_, _| anyhow::bail!("Interrupted acceptance"))
            .is_err()
    );
    let dir = other
        .outbox
        .worktree(&id)
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    fs::write(dir.join("resolved/source/shared.txt"), "damaged").unwrap();
    assert!(
        other
            .outbox
            .accept(&id, |_, _| panic!("Damaged resolution must be refused"))
            .is_err()
    );
    assert_eq!(other.remote.pushes.get(), 0);
}

#[test]
fn retry_preserves_retained_data_collision_protection() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    fs::write(f.source.join(".gitignore"), "local.db\n").unwrap();
    fs::write(f.source.join("local.db"), "retained app data").unwrap();
    let id = f.enqueue();
    f.outbox.prepare(&id, &f.remote).unwrap();
    f.accept(&id);
    f.upstream("local.db", "remote source collision");
    assert!(f.outbox.deliver(&id, &f.remote).is_err());
    let next = f.outbox.retry(&id).unwrap();
    let next = next["active"]["id"].as_str().unwrap();
    f.outbox.prepare(next, &f.remote).unwrap();
    assert!(
        f.outbox
            .accept(next, |_, _| panic!("Retry must preserve retained data"))
            .unwrap_err()
            .to_string()
            .contains("collides")
    );
    assert_eq!(f.remote.pushes.get(), 0);
}

#[test]
fn agent_commits_cannot_redefine_original_layerfs_source() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    git(&f.remote.repo, &["rm", "other.txt"]);
    git(&f.remote.repo, &["commit", "-m", "Remote deletion"]);
    let id = f.enqueue();
    f.outbox.prepare(&id, &f.remote).unwrap();
    git(
        &f.outbox.worktree(&id).unwrap(),
        &["commit", "-m", "Agent recorded resolution"],
    );
    f.outbox
        .accept(&id, |_, files| {
            let removed =
                enginebox_core::state::read_json(&files.parent().unwrap().join("remove.json"))?;
            assert_eq!(removed, serde_json::json!(["other.txt"]));
            Ok(("accepted-agent-commit".into(), 2))
        })
        .unwrap();
}

#[test]
fn source_application_removes_only_reviewed_leaves_and_refuses_symlink_parents() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("old.txt"), "old source").unwrap();
    fs::write(root.path().join("app.db"), "retained").unwrap();
    fs::create_dir(root.path().join("cache")).unwrap();
    fs::write(root.path().join("cache/deps"), "retained dependencies").unwrap();
    std::os::unix::fs::symlink("cache", root.path().join("alias")).unwrap();
    let apply = |plan: Value| {
        let plan_file = root.path().join("apply.json");
        fs::write(&plan_file, plan.to_string()).unwrap();
        enginebox_core::transport::run_process(
            &[
                "node".into(),
                "-e".into(),
                include_str!("../src/prepare-source.cjs").into(),
                plan_file.to_str().unwrap().into(),
                root.path().to_str().unwrap().into(),
            ],
            None,
            std::time::Duration::from_secs(10),
        )
    };
    assert!(apply(serde_json::json!({"remove":[],"write":["alias/deps"]})).is_err());
    assert_eq!(
        fs::read_to_string(root.path().join("cache/deps")).unwrap(),
        "retained dependencies"
    );
    assert!(apply(serde_json::json!({"remove":[],"write":["cache"]})).is_err());
    apply(serde_json::json!({"remove":["old.txt"],"write":["new/file.txt"]})).unwrap();
    assert!(!root.path().join("old.txt").exists());
    assert_eq!(
        fs::read_to_string(root.path().join("app.db")).unwrap(),
        "retained"
    );
}
