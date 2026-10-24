use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::Semaphore;

pub fn private_dir(path: &Path) -> Result<()> {
    enginebox_core::state::private_dir(path)
}

pub fn write_private(path: &Path, value: &Value) -> Result<()> {
    if let Ok(m) = fs::symlink_metadata(path) {
        ensure!(
            m.is_file() && !m.file_type().is_symlink(),
            "Invalid state file"
        );
    }
    let mut f =
        tempfile::NamedTempFile::new_in(path.parent().context("State file parent missing")?)?;
    serde_json::to_writer(&mut f, value)?;
    f.write_all(b"\n")?;
    f.as_file().sync_all()?;
    f.persist(path).map_err(|e| e.error)?;
    #[cfg(unix)]
    {
        fs::File::open(path.parent().unwrap())?.sync_all()?;
    }
    Ok(())
}

pub fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

#[derive(Clone)]
pub struct Jobs {
    root: PathBuf,
    records: Arc<Mutex<BTreeMap<String, Value>>>,
    pending: Arc<AtomicUsize>,
    workers: Arc<Semaphore>,
}

impl Jobs {
    pub fn open(root: &Path) -> Result<Self> {
        private_dir(root)?;
        let mut records = BTreeMap::new();
        for entry in fs::read_dir(root)? {
            let path = entry?.path();
            if path.extension().and_then(|v| v.to_str()) != Some("json") {
                continue;
            }
            let id = path
                .file_stem()
                .and_then(|v| v.to_str())
                .context("Invalid job filename")?;
            ensure!(valid_id(id), "Invalid job filename");
            let meta = fs::symlink_metadata(&path)?;
            ensure!(
                meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= 4 * 1024 * 1024,
                "Invalid job record"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                ensure!(
                    meta.mode() & 0o077 == 0 && meta.nlink() == 1,
                    "Job records must be private regular files"
                );
            }
            let mut v = enginebox_core::state::read_json(&path)?;
            ensure!(
                v.is_object()
                    && v["id"] == id
                    && matches!(
                        v["status"].as_str(),
                        Some("queued" | "running" | "succeeded" | "failed")
                    ),
                "Invalid job record"
            );
            if matches!(v["status"].as_str(), Some("queued" | "running")) {
                v["status"] = json!("failed");
                v["error"] = json!("Controller restarted; inspect the workspace before retrying.");
                write_private(&path, &v)?;
            }
            records.insert(id.to_owned(), v);
        }
        Ok(Self {
            root: root.to_owned(),
            records: Arc::new(Mutex::new(records)),
            pending: Arc::new(AtomicUsize::new(0)),
            workers: Arc::new(Semaphore::new(4)),
        })
    }
    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::SeqCst)
    }
    pub fn get(&self, id: &str) -> Result<Value> {
        ensure!(valid_id(id), "Invalid job ID");
        self.records
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .context("Unknown job")
    }
    pub fn submit<F>(&self, host: &str, operation: &str, name: &str, work: F) -> Result<Value>
    where
        F: FnOnce() -> Result<Value> + Send + 'static,
    {
        let mut records = self.records.lock().unwrap();
        if operation == "install_template"
            && let Some(v) = records.values().find(|v| {
                v["operation"] == operation
                    && v["host_id"] == host
                    && v["name"] == name
                    && matches!(v["status"].as_str(), Some("queued" | "running"))
            })
        {
            return Ok(v.clone());
        }
        if self.pending() >= 32 {
            bail!("Fleet queue is full; wait for existing jobs");
        }
        let id = uuid::Uuid::new_v4().simple().to_string();
        let record = json!({"id":id,"host_id":host,"operation":operation,"name":name,"status":"queued","created_at":Utc::now().to_rfc3339()});
        write_private(&self.root.join(format!("{id}.json")), &record)?;
        records.insert(id.clone(), record.clone());
        self.pending.fetch_add(1, Ordering::SeqCst);
        drop(records);
        let this = self.clone();
        tokio::spawn(async move {
            let _permit = this
                .workers
                .clone()
                .acquire_owned()
                .await
                .expect("Job semaphore never closed");
            let started = std::time::Instant::now();
            let mut v = this.get(&id).expect("Admitted job exists");
            v["status"] = json!("running");
            let persisted = this.save(&id, v);
            let result = if persisted.is_ok() {
                tokio::task::spawn_blocking(work)
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|r| r)
            } else {
                Err(anyhow::anyhow!("Unable to journal job start"))
            };
            let mut v = this.get(&id).expect("Admitted job exists");
            match result {
                Ok(r) => {
                    v["status"] = json!("succeeded");
                    v["result"] = r;
                }
                Err(_) => {
                    v["status"] = json!("failed");
                    v["error"] = if v["operation"] == "github_sync" {
                        json!(
                            "Repository sync failed. Check GitHub access, the selected branch, uncommitted files and diverged history before retrying."
                        )
                    } else {
                        json!(
                            "Fleet operation failed; inspect the workspace and controller diagnostics before retrying."
                        )
                    };
                }
            }
            v["duration_seconds"] = json!(started.elapsed().as_secs_f64());
            // Keep a terminal result in memory if the disk becomes unavailable;
            // the durable running record recovers as uncertain after restart.
            if this.save(&id, v.clone()).is_err() {
                this.records.lock().unwrap().insert(id.clone(), v);
            }
            this.pending.fetch_sub(1, Ordering::SeqCst);
        });
        Ok(record)
    }
    fn save(&self, id: &str, value: Value) -> Result<()> {
        let mut records = self.records.lock().unwrap();
        write_private(&self.root.join(format!("{id}.json")), &value)?;
        records.insert(id.to_owned(), value);
        if records.len() > 200 {
            let mut done: Vec<_> = records
                .iter()
                .filter(|(_, v)| matches!(v["status"].as_str(), Some("succeeded" | "failed")))
                .map(|(id, v)| {
                    (
                        v["created_at"].as_str().unwrap_or("").to_owned(),
                        id.clone(),
                    )
                })
                .collect();
            done.sort();
            for (_, old) in done.into_iter().take(records.len() - 200) {
                fs::remove_file(self.root.join(format!("{old}.json")))?;
                records.remove(&old);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn completed_jobs_survive_restart_and_unfinished_jobs_are_not_replayed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("jobs");
        let jobs = Jobs::open(&root).unwrap();
        let job = jobs
            .submit("test", "create", "ebox-test", || {
                Ok(json!({"name":"ebox-test"}))
            })
            .unwrap();
        while jobs.pending() > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let recovered = Jobs::open(&root).unwrap();
        assert_eq!(
            recovered.get(job["id"].as_str().unwrap()).unwrap()["status"],
            "succeeded"
        );
        let id = "a".repeat(32);
        write_private(
            &root.join(format!("{id}.json")),
            &json!({"id":id,"status":"running"}),
        )
        .unwrap();
        let recovered = Jobs::open(&root).unwrap();
        assert_eq!(recovered.get(&id).unwrap()["status"], "failed");
        assert!(recovered.get("../secret").is_err());
    }
    #[cfg(unix)]
    #[test]
    fn state_cannot_be_a_symlink_or_public_file() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = tmp.path().join("link");
        symlink(&target, &link).unwrap();
        assert!(Jobs::open(&link).is_err());
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Jobs::open(&target).is_err());
    }
}
