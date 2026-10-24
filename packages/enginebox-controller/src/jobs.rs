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

struct PendingGuard(Arc<AtomicUsize>);
impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

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
    ci_pending: Arc<AtomicUsize>,
    ci_workers: Arc<Semaphore>,
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
            ci_pending: Arc::new(AtomicUsize::new(0)),
            ci_workers: Arc::new(Semaphore::new(4)),
        })
    }
    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::SeqCst) + self.ci_pending.load(Ordering::SeqCst)
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
    pub fn existing_request(&self, key: &str, digest: &str) -> Result<Option<Value>> {
        let id =
            enginebox_core::state::digest(&json!({"namespace":"ci-v1","key":key}))[..32].to_owned();
        let records = self.records.lock().unwrap();
        let record = records.get(&id);
        ensure!(
            record.is_none_or(|r| r["request_digest"] == digest),
            "Request ID was already used for different CI content"
        );
        Ok(record.cloned())
    }
    pub fn submit<F>(&self, host: &str, operation: &str, name: &str, work: F) -> Result<Value>
    where
        F: FnOnce() -> Result<Value> + Send + 'static,
    {
        self.submit_inner(host, operation, name, None, work)
    }
    /// A durable caller identity prevents a merge delivery or lost HTTP reply
    /// from replaying a build/deploy trigger. Keyed records are never pruned.
    pub fn submit_once<F>(
        &self,
        host: &str,
        operation: &str,
        name: &str,
        identity: (&str, &str),
        work: F,
    ) -> Result<Value>
    where
        F: FnOnce() -> Result<Value> + Send + 'static,
    {
        self.submit_inner(host, operation, name, Some(identity), work)
    }
    fn submit_inner<F>(
        &self,
        host: &str,
        operation: &str,
        name: &str,
        identity: Option<(&str, &str)>,
        work: F,
    ) -> Result<Value>
    where
        F: FnOnce() -> Result<Value> + Send + 'static,
    {
        let mut records = self.records.lock().unwrap();
        let id = match identity {
            Some((key, digest)) => {
                let id = enginebox_core::state::digest(&json!({"namespace":"ci-v1","key":key}))
                    [..32]
                    .to_owned();
                if let Some(existing) = records.get(&id) {
                    ensure!(
                        existing["request_digest"] == digest,
                        "Request ID was already used for different CI content"
                    );
                    return Ok(existing.clone());
                }
                ensure!(
                    records
                        .values()
                        .filter(|r| r.get("request_digest").is_some())
                        .count()
                        < 10000,
                    "CI request journal is full; archive it during controller maintenance"
                );
                id
            }
            None => uuid::Uuid::new_v4().simple().to_string(),
        };
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
        let is_ci = operation.starts_with("ci_");
        let pool_pending = if is_ci {
            self.ci_pending.load(Ordering::SeqCst)
        } else {
            self.pending.load(Ordering::SeqCst)
        };
        if pool_pending >= 32 {
            bail!("Fleet queue is full; wait for existing jobs");
        }
        let mut record = json!({"id":id,"host_id":host,"operation":operation,"name":name,"status":"queued","created_at":Utc::now().to_rfc3339()});
        if let Some((key, digest)) = identity {
            record["request_id"] = json!(key);
            record["request_digest"] = json!(digest);
        }
        write_private(&self.root.join(format!("{id}.json")), &record)?;
        records.insert(id.clone(), record.clone());
        let counter = if is_ci {
            &self.ci_pending
        } else {
            &self.pending
        };
        counter.fetch_add(1, Ordering::SeqCst);
        drop(records);
        let this = self.clone();
        // Released on every exit, including a panic, so drain() cannot see a
        // phantom active job.
        let pending = PendingGuard(counter.clone());
        tokio::spawn(async move {
            let _pending = pending;
            let Ok(_permit) = if is_ci {
                &this.ci_workers
            } else {
                &this.workers
            }
            .clone()
            .acquire_owned()
            .await
            else {
                eprintln!("Fleet job {id} abandoned: worker semaphore closed");
                return;
            };
            let started = std::time::Instant::now();
            let Ok(mut v) = this.get(&id) else {
                eprintln!("Fleet job {id} abandoned: admitted record is missing");
                return;
            };
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
            let Ok(mut v) = this.get(&id) else {
                eprintln!("Fleet job {id} finished but its record is missing");
                return;
            };
            match result {
                Ok(r) => {
                    v["status"] = json!("succeeded");
                    v["result"] = r;
                }
                Err(_) => {
                    v["status"] = json!("failed");
                    v["error"] = json!(
                        "Fleet operation failed; inspect the workspace and controller diagnostics before retrying."
                    );
                }
            }
            v["duration_seconds"] = json!(started.elapsed().as_secs_f64());
            // Keep a terminal result in memory if the disk becomes unavailable;
            // the durable running record recovers as uncertain after restart.
            if this.save(&id, v.clone()).is_err() {
                this.records.lock().unwrap().insert(id.clone(), v);
            }
        });
        Ok(record)
    }
    fn save(&self, id: &str, value: Value) -> Result<()> {
        let mut records = self.records.lock().unwrap();
        write_private(&self.root.join(format!("{id}.json")), &value)?;
        records.insert(id.to_owned(), value);
        let ordinary_records = records
            .values()
            .filter(|v| v.get("request_digest").is_none())
            .count();
        if ordinary_records > 200 {
            let mut done: Vec<_> = records
                .iter()
                .filter(|(_, v)| {
                    v.get("request_digest").is_none()
                        && matches!(v["status"].as_str(), Some("succeeded" | "failed"))
                })
                .map(|(id, v)| {
                    (
                        v["created_at"].as_str().unwrap_or("").to_owned(),
                        id.clone(),
                    )
                })
                .collect();
            done.sort();
            for (_, old) in done.into_iter().take(ordinary_records - 200) {
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
    async fn ci_history_does_not_evict_workspace_operation_results() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("jobs");
        private_dir(&root).unwrap();
        for n in 0..201 {
            let id = format!("{n:032x}");
            write_private(
                &root.join(format!("{id}.json")),
                &json!({"id":id,"status":"succeeded","request_digest":"fixture"}),
            )
            .unwrap();
        }
        let jobs = Jobs::open(&root).unwrap();
        let job = jobs
            .submit("agents", "resume", "ebox-interactive", || {
                Ok(json!({"resumed":true}))
            })
            .unwrap();
        while jobs.pending() > 0 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            jobs.get(job["id"].as_str().unwrap()).unwrap()["status"],
            "succeeded"
        );
    }
    #[tokio::test]
    async fn saturated_ci_pool_leaves_agent_workspace_operations_admissible_and_runnable() {
        use std::sync::atomic::AtomicBool;
        let tmp = tempfile::tempdir().unwrap();
        let jobs = Jobs::open(&tmp.path().join("jobs")).unwrap();
        let release = Arc::new(AtomicBool::new(false));
        for _ in 0..32 {
            let release = release.clone();
            jobs.submit("ci", "ci_automation", "heavy-build", move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while !release.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Ok(json!({"built":true}))
            })
            .unwrap();
        }
        assert!(
            jobs.submit("ci", "ci_automation", "overflow", || Ok(json!({})))
                .is_err()
        );
        let (sent, received) = tokio::sync::oneshot::channel();
        jobs.submit("agents", "create", "ebox-interactive", move || {
            let _ = sent.send(());
            Ok(json!({"created":true}))
        })
        .unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), received).await;
        release.store(true, Ordering::SeqCst);
        assert!(
            result.is_ok(),
            "Agent work waited for the saturated CI pool"
        );
        while jobs.pending() > 0 {
            tokio::task::yield_now().await;
        }
    }
    #[tokio::test]
    async fn keyed_triggers_execute_once_and_keep_identity_after_restart() {
        use std::sync::atomic::AtomicUsize;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("jobs");
        let jobs = Jobs::open(&root).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let first = jobs
            .submit_once(
                "github",
                "ci_github_dispatch",
                "build",
                ("merge-1", "digest"),
                move || {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(json!({"dispatched":true}))
                },
            )
            .unwrap();
        let second = jobs
            .submit_once(
                "github",
                "ci_github_dispatch",
                "build",
                ("merge-1", "digest"),
                || panic!("Duplicate request executed"),
            )
            .unwrap();
        assert_eq!(first["id"], second["id"]);
        assert!(
            jobs.submit_once(
                "github",
                "ci_github_dispatch",
                "build",
                ("merge-1", "different"),
                || Ok(json!({}))
            )
            .is_err()
        );
        while jobs.pending() > 0 {
            tokio::task::yield_now().await;
        }
        let recovered = Jobs::open(&root).unwrap();
        assert_eq!(
            recovered
                .submit_once(
                    "github",
                    "ci_github_dispatch",
                    "build",
                    ("merge-1", "digest"),
                    || panic!("Restart replayed trigger")
                )
                .unwrap()["status"],
            "succeeded"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let mut record = recovered.get(first["id"].as_str().unwrap()).unwrap();
        record["status"] = json!("running");
        write_private(
            &root.join(format!("{}.json", first["id"].as_str().unwrap())),
            &record,
        )
        .unwrap();
        let recovered = Jobs::open(&root).unwrap();
        assert_eq!(
            recovered
                .existing_request("merge-1", "digest")
                .unwrap()
                .unwrap()["status"],
            "failed"
        );
        assert!(recovered.existing_request("merge-1", "different").is_err());
    }
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
    async fn wait_idle(jobs: &Jobs) {
        for _ in 0..200 {
            if jobs.pending() == 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("pending stayed at {}", jobs.pending());
    }
    #[tokio::test]
    async fn pending_returns_to_zero_when_the_record_goes_missing() {
        for (operation, ci) in [("create", false), ("ci_run", true)] {
            let tmp = tempfile::tempdir().unwrap();
            let jobs = Jobs::open(&tmp.path().join("jobs")).unwrap();
            let pool = if ci { &jobs.ci_workers } else { &jobs.workers };
            // Hold every worker so the job is queued while its record is removed.
            let held = pool.clone().acquire_many_owned(4).await.unwrap();
            let job = jobs
                .submit("test", operation, "ebox-test", || Ok(json!({})))
                .unwrap();
            assert_eq!(jobs.pending(), 1);
            jobs.records
                .lock()
                .unwrap()
                .remove(job["id"].as_str().unwrap());
            drop(held);
            wait_idle(&jobs).await;
        }
    }
    #[tokio::test]
    async fn pending_returns_to_zero_when_the_job_task_panics() {
        for (operation, ci) in [("create", false), ("ci_run", true)] {
            let tmp = tempfile::tempdir().unwrap();
            let jobs = Jobs::open(&tmp.path().join("jobs")).unwrap();
            let pool = if ci { &jobs.ci_workers } else { &jobs.workers };
            let held = pool.clone().acquire_many_owned(4).await.unwrap();
            jobs.submit("test", operation, "ebox-test", || Ok(json!({})))
                .unwrap();
            assert_eq!(jobs.pending(), 1);
            // A poisoned record lock makes the queued task panic.
            let records = jobs.records.clone();
            let _ = std::thread::spawn(move || {
                let _lock = records.lock().unwrap();
                panic!("poison");
            })
            .join();
            drop(held);
            wait_idle(&jobs).await;
        }
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
