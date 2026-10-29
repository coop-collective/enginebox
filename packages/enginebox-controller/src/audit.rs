//! Append-only audit log for catalog operations (docs/CONNECTIONS.md, "Audit").
//!
//! One JSON line per operation in `audit/events.jsonl` (0600, owner-only
//! directory): when, which org, who, what, on which record, and whether it was
//! applied. Entries carry names only (connection, profile, secret *names*),
//! never a value. The file rotates once to `events.1.jsonl` past 4 MiB.
use anyhow::Result;
use enginebox_core::state;
use serde_json::{Value, json};
use std::{
    io::{BufRead, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

const ROTATE_BYTES: u64 = 4 * 1024 * 1024;
/// Most entries one read returns.
pub const MAX_READ: usize = 500;

#[derive(Clone)]
pub struct Audit {
    dir: PathBuf,
    lock: Arc<Mutex<()>>,
}

/// Who performed an operation: `owner` (the owner token: fleet MCP, CLI) or
/// the member the dashboard names in `X-EngineBox-Actor`.
pub fn valid_actor(actor: &str) -> bool {
    actor == "owner"
        || (actor.len() == 28
            && actor.starts_with("usr_")
            && actor[4..]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

impl Audit {
    pub fn open(root: &Path) -> Result<Self> {
        let dir = root.join("audit");
        state::private_dir(&dir)?;
        Ok(Self {
            dir,
            lock: Arc::new(Mutex::new(())),
        })
    }
    fn file(&self) -> PathBuf {
        self.dir.join("events.jsonl")
    }
    /// Record one operation. `detail` must hold names and counts only.
    pub fn record(
        &self,
        org: &str,
        actor: &str,
        action: &str,
        target: &str,
        ok: bool,
        detail: Value,
    ) {
        let entry = json!({
            "at": chrono::Utc::now().to_rfc3339(),
            "org_id": org,
            "actor": actor,
            "action": action,
            "target": target,
            "outcome": if ok { "applied" } else { "refused" },
            "detail": detail,
        });
        let _guard = self.lock.lock().unwrap();
        // Auditing never fails the operation it records; a write failure is
        // reported on stderr without the entry's detail.
        if let Err(error) = self.append(&entry) {
            eprintln!("audit: could not record {action}: {error}");
        }
    }
    fn append(&self, entry: &Value) -> Result<()> {
        let path = self.file();
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > ROTATE_BYTES) {
            std::fs::rename(&path, self.dir.join("events.1.jsonl"))?;
        }
        // Never append through a planted symlink or to anything but a file.
        if let Ok(meta) = std::fs::symlink_metadata(&path) {
            anyhow::ensure!(
                meta.is_file() && !meta.file_type().is_symlink(),
                "Invalid audit log"
            );
        }
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        let mut line = serde_json::to_vec(entry)?;
        line.push(b'\n');
        file.write_all(&line)?;
        Ok(())
    }
    /// The newest entries of `org` (newest first), at most `limit`.
    pub fn list(&self, org: &str, limit: usize) -> Result<Value> {
        let _guard = self.lock.lock().unwrap();
        let mut rows = Vec::new();
        for name in ["events.1.jsonl", "events.jsonl"] {
            let Ok(file) = std::fs::File::open(self.dir.join(name)) else {
                continue;
            };
            for line in std::io::BufReader::new(file).lines() {
                let Ok(entry) = serde_json::from_str::<Value>(&line?) else {
                    continue;
                };
                if entry["org_id"] == org {
                    rows.push(entry);
                }
            }
        }
        rows.reverse();
        rows.truncate(limit.min(MAX_READ));
        Ok(json!({ "events": rows }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn entries_are_org_scoped_newest_first_and_private() {
        let dir = tempfile::tempdir().unwrap();
        let audit = Audit::open(dir.path()).unwrap();
        audit.record(
            "default",
            "owner",
            "connection.create",
            "a",
            true,
            json!({}),
        );
        audit.record("org_x", "owner", "connection.create", "b", true, json!({}));
        audit.record(
            "default",
            "owner",
            "connection.delete",
            "a",
            false,
            json!({}),
        );
        let listed = audit.list("default", 10).unwrap();
        let events = listed["events"].as_array().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["action"], "connection.delete");
        assert_eq!(events[0]["outcome"], "refused");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("audit/events.jsonl"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0);
        }
        assert!(valid_actor("owner"));
        assert!(valid_actor("usr_0123456789abcdef01234567"));
        assert!(!valid_actor("usr_x"));
    }
}
