//! GitHub remains the scheduler. Webhooks only request bounded runner capacity.
use crate::{GitHub, api, full_name, name, number, text};
use anyhow::{Context, Result, ensure};
use enginebox_core::transport::run_command;
use ring::hmac;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pool {
    pub id: String,
    pub installation_id: u64,
    pub organization: String,
    pub runner_group_id: u64,
    pub labels: Vec<String>,
    pub max_runners: usize,
    pub timeout_seconds: u64,
    /// Administrator-provisioned worker; opaque JIT configuration goes via stdin.
    pub worker: Vec<String>,
    /// Private, one-job runners connected before demand arrives.
    #[serde(default)]
    pub standby: Option<Standby>,
    #[serde(default)]
    pub preparation: Option<Preparation>,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Standby {
    pub minimum: usize,
    pub repository_id: u64,
    pub workflows: Vec<String>,
    pub idle_timeout_seconds: u64,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preparation {
    pub repository_id: u64,
    /// Every allowed workflow may read the preloaded repository cache. No stronger trust is implied.
    pub workflows: Vec<String>,
}

fn check_group(group: &Value, repositories: &[Value], preparation: &Preparation) -> Result<()> {
    ensure!(
        group["visibility"] == "selected"
            && group["allows_public_repositories"] == false
            && group["restricted_to_workflows"] == true
            && repositories.len() == 1
            && repositories[0]["id"] == preparation.repository_id,
        "Prepared runners require a private single-repository group with workflow restrictions"
    );
    let actual = group["selected_workflows"]
        .as_array()
        .context("Missing workflow restrictions")?
        .iter()
        .map(|v| v.as_str().context("Invalid workflow restriction"))
        .collect::<Result<BTreeSet<_>>>()?;
    let expected = preparation
        .workflows
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    ensure!(
        actual == expected,
        "Runner group workflow restrictions differ from preparation policy"
    );
    Ok(())
}
pub fn validate_pools(pools: &[Pool]) -> Result<()> {
    let mut ids = BTreeSet::new();
    ensure!(pools.len() <= 16, "Too many CI pools");
    for p in pools {
        enginebox_core::identifier(&p.id, false)?;
        name(&p.organization)?;
        ensure!(
            ids.insert(&p.id)
                && p.installation_id > 0
                && p.runner_group_id > 0
                && (1..=8).contains(&p.max_runners)
                && (60..=21600).contains(&p.timeout_seconds)
                && !p.worker.is_empty()
                && Path::new(&p.worker[0]).is_absolute()
                && p.worker.iter().all(|s| !s.contains('\0')),
            "Invalid CI pool configuration"
        );
        ensure!(
            (1..=10).contains(&p.labels.len())
                && p.labels.contains(&format!("enginebox-{}", p.id))
                && p.labels.iter().all(|s| name(s).is_ok()),
            "Each pool needs its own enginebox-<id> label"
        );
        if let Some(standby) = &p.standby {
            ensure!(
                standby.minimum == 1 && (60..=21600).contains(&standby.idle_timeout_seconds),
                "Standby requires one bounded private runner"
            );
            if let Some(preparation) = &p.preparation {
                ensure!(
                    preparation.repository_id == standby.repository_id
                        && preparation.workflows == standby.workflows,
                    "Standby and preparation must share one repository/workflow policy"
                );
            }
        }
        let standby_policy = p.standby.as_ref().map(|s| Preparation {
            repository_id: s.repository_id,
            workflows: s.workflows.clone(),
        });
        for preparation in p.preparation.iter().chain(standby_policy.iter()) {
            ensure!(
                preparation.repository_id > 0 && (1..=16).contains(&preparation.workflows.len()),
                "Bind preparation to a repository and protected workflows"
            );
            let mut workflows = BTreeSet::new();
            for workflow in &preparation.workflows {
                ensure!(
                    workflow.len() <= 512
                        && !workflow.chars().any(char::is_whitespace)
                        && workflow.starts_with(&format!("{}/", p.organization))
                        && workflow.contains("/.github/workflows/")
                        && workflow.contains("@refs/heads/")
                        && workflows.insert(workflow),
                    "Use unique protected-branch workflow references"
                );
            }
        }
    }
    Ok(())
}
pub fn signature(secret: &str, supplied: &str, body: &[u8]) -> Result<()> {
    let hex = supplied
        .strip_prefix("sha256=")
        .context("Missing SHA-256 webhook signature")?;
    ensure!(hex.len() == 64, "Invalid signature");
    let bytes = (0..64)
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(hex.get(i..i + 2).context("Invalid signature")?, 16)
                .context("Invalid signature")
        })
        .collect::<Result<Vec<_>>>()?;
    hmac::verify(
        &hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes()),
        body,
        &bytes,
    )
    .map_err(|_| anyhow::anyhow!("Invalid signature"))
}
fn matches(pool: &Pool, payload: &Value) -> bool {
    payload["installation"]["id"] == pool.installation_id
        && pool
            .standby
            .as_ref()
            .is_none_or(|s| payload["repository"]["id"] == s.repository_id)
        && pool
            .preparation
            .as_ref()
            .is_none_or(|p| payload["repository"]["id"] == p.repository_id)
        && payload["repository"]["owner"]["login"]
            .as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case(&pool.organization))
        && payload["workflow_job"]["labels"]
            .as_array()
            .is_some_and(|labels| {
                labels
                    .iter()
                    .any(|l| l.as_str() == Some(&format!("enginebox-{}", pool.id)))
                    && labels.iter().all(|l| {
                        l.as_str().is_some_and(|label| {
                            pool.labels.iter().any(|v| v.eq_ignore_ascii_case(label))
                        })
                    })
            })
}
impl GitHub {
    pub fn webhook(&self, kind: &str, event: &str, supplied: &str, body: &[u8]) -> Result<Value> {
        let received_at_ms = now_ms();
        signature(&self.app(kind)?.webhook_secret, supplied, body)?;
        let payload: Value = serde_json::from_slice(body)?;
        if (kind != "runners" && self.account_kind(kind)? != "github") || event != "workflow_job" {
            return Ok(json!({"accepted":true,"ignored":true}));
        }
        let config = self.config.as_ref().context("GitHub disabled")?;
        let action = text(&payload, "action")?;
        ensure!(
            ["queued", "in_progress", "completed"].contains(&action),
            "Unknown workflow job action"
        );
        let id = number(&payload["workflow_job"], "id")?;
        let repo_id = number(&payload["repository"], "id")?;
        let mut data = self.data.lock().unwrap();
        let selected = match data.repositories.get(&repo_id) {
            Some(r) => r.clone(),
            None => return Ok(json!({"accepted":true,"ignored":true})),
        };
        ensure!(
            payload["repository"]["full_name"] == selected.full_name,
            "Repository identity changed"
        );
        let Some(pool) = config.pools.iter().find(|p| matches(p, &payload)) else {
            return Ok(json!({"accepted":true,"ignored":true}));
        };
        let key = format!("{}-{id}", pool.installation_id);
        let mut demand_added = false;
        if action != "queued" {
            for reserve in data.jobs.values_mut().filter(|j| {
                j["reserve"] == true
                    && j["pool"] == pool.id
                    && j["status"] == "running"
                    && ((payload["workflow_job"]["runner_id"]
                        .as_u64()
                        .is_some_and(|id| id > 0)
                        && j["runner_id"] == payload["workflow_job"]["runner_id"])
                        || (payload["workflow_job"]["runner_name"]
                            .as_str()
                            .is_some_and(|name| !name.is_empty())
                            && j["runner_name"] == payload["workflow_job"]["runner_name"]))
            }) {
                if reserve["assigned_job_id"].is_null() {
                    reserve["assigned_job_id"] = json!(id);
                    reserve["assigned_run_id"] = payload["workflow_job"]["run_id"].clone();
                    reserve["timings"]["assignment_observed_at_ms"] = json!(received_at_ms);
                    demand_added = true;
                }
            }
        }
        if let Some(job) = data.jobs.get_mut(&key) {
            ensure!(job["repository_id"] == repo_id, "Job repository mismatch");
            // Events can arrive out of order; terminal and running states never requeue.
            if action == "completed" {
                job["github_status"] = json!("completed");
                job["conclusion"] = payload["workflow_job"]["conclusion"].clone();
                if job["status"] == "queued" {
                    job["status"] = json!("cancelled");
                }
            } else if action == "in_progress" && job["github_status"] != "completed" {
                job["github_status"] = json!("in_progress");
                if job["status"] == "queued" {
                    job["status"] = json!("claimed_elsewhere");
                }
            }
        } else {
            data.jobs.retain(|_, j| {
                !matches!(
                    j["status"].as_str(),
                    Some("finished" | "cancelled" | "claimed_elsewhere")
                ) || j["created_at"].as_u64().unwrap_or(0) + 7 * 86400 > api::now()
            });
            ensure!(data.jobs.len() < 256, "CI inbox is full; inspect receipts");
            data.jobs.insert(key,json!({"id":id,"pool":pool.id,"repository_id":repo_id,"repository":selected.full_name,"run_id":payload["workflow_job"]["run_id"],"status":if action=="queued" {"queued"} else {"claimed_elsewhere"},"github_status":action,"created_at":api::now(),"timings":{"demand_received_at_ms":received_at_ms}}));
            demand_added |= action == "queued";
        }
        self.save(&data)?;
        drop(data);
        if demand_added {
            self.wake_dispatch();
        }
        Ok(json!({"accepted":true}))
    }
    pub fn set_draining(&self, draining: bool) -> Result<Value> {
        let mut data = self.data.lock().unwrap();
        data.draining = draining;
        self.save(&data)?;
        drop(data);
        if draining {
            self.retire_reserves();
        } else {
            self.wake_dispatch();
        }
        Ok(json!({"draining":draining,"active_runners":self.pending()}))
    }
    /// Called only while the controller is active. Running workers are part of its drain barrier.
    pub fn tick(&self) -> Result<()> {
        self.dispatch()?;
        if let Some(config) = &self.config
            && !self.data.lock().unwrap().draining
            && !self.handed_off.load(Ordering::SeqCst)
        {
            self.retire_reserves();
            self.reconcile_standby_assignments(config);
            self.reconcile_uncertain(config);
        }
        Ok(())
    }
    /// Fast admission only: API and worker operations run on bounded job threads.
    pub fn dispatch(&self) -> Result<()> {
        let Some(config) = &self.config else {
            return Ok(());
        };
        let mut data = self.data.lock().unwrap();
        if data.draining
            || self.admission_paused.load(Ordering::SeqCst)
            || self.handed_off.load(Ordering::SeqCst)
        {
            return Ok(());
        }
        for pool in &config.pools {
            // A host shortage applies to the whole pool, not just one demand.
            // Retain queued jobs without minting a new JIT registration every tick.
            if data.jobs.values().any(|job| {
                job["pool"] == pool.id
                    && ((job["status"] == "queued"
                        && job["capacity_retry_at"].as_u64().unwrap_or(0) > api::now())
                        || job["reserve_retry_at"].as_u64().unwrap_or(0) > api::now())
            }) {
                continue;
            }
            let active = data
                .jobs
                .values()
                .filter(|j| {
                    j["pool"] == pool.id
                        && matches!(j["status"].as_str(), Some("running" | "uncertain"))
                })
                .count();
            if let Some(standby) = &pool.standby
                && active < pool.max_runners
                && !data.jobs.values().any(|j| {
                    j["pool"] == pool.id
                        && j["reserve"] == true
                        && matches!(
                            j["status"].as_str(),
                            Some("queued" | "running" | "uncertain")
                        )
                        && j["assigned_job_id"].is_null()
                        && j["reserve_busy"] != true
                })
            {
                let selected = data.repositories.get(&standby.repository_id).cloned();
                if let Some(selected) = selected.filter(|r| r.private) {
                    data.jobs.retain(|_, j| {
                        !matches!(
                            j["status"].as_str(),
                            Some("finished" | "cancelled" | "claimed_elsewhere")
                        ) || j["created_at"].as_u64().unwrap_or(0) + 7 * 86400 > api::now()
                    });
                    ensure!(data.jobs.len() < 256, "CI inbox is full; inspect receipts");
                    let key = format!("reserve-{}", uuid::Uuid::new_v4().simple());
                    data.jobs.insert(key, json!({"id":0,"reserve":true,"pool":pool.id,"repository_id":selected.id,
                        "repository":selected.full_name,"status":"queued","github_status":"standby","created_at":api::now(),
                        "timings":{"reserve_requested_at_ms":now_ms()}}));
                }
            }
            let keys = data
                .jobs
                .iter()
                .filter(|(_, j)| j["pool"] == pool.id && j["status"] == "queued")
                .collect::<Vec<_>>()
                .into_iter()
                .filter(|(_, j)| pool.standby.is_none() || j["reserve"] == true)
                .take(pool.max_runners.saturating_sub(active))
                .map(|(k, _)| k.clone())
                .collect::<Vec<_>>();
            for key in keys {
                let job = data.jobs.get_mut(&key).unwrap();
                // A proved capacity rejection may retry the same demand, but
                // none of its old runner identity or retirement proof transfers.
                for field in [
                    "runner_id",
                    "runner_name",
                    "worker_pid",
                    "worker_program",
                    "worker_output",
                    "registration_retired",
                    "retire_requested",
                    "retire_error",
                    "reserve_busy",
                    "runner_observation",
                ] {
                    job.as_object_mut().unwrap().remove(field);
                }
                if let Some(timings) = job["timings"].as_object_mut() {
                    timings.retain(|phase, _| {
                        matches!(
                            phase.as_str(),
                            "demand_received_at_ms" | "reserve_requested_at_ms"
                        )
                    });
                }
                job["status"] = json!("running");
                job["timings"]["dispatch_started_at_ms"] = json!(now_ms());
                job.as_object_mut().unwrap().remove("capacity_retry_at");
                self.save(&data)?;
                self.pending.fetch_add(1, Ordering::SeqCst);
                let this = self.clone();
                let pool = pool.clone();
                std::thread::spawn(move || {
                    let started = Instant::now();
                    let released = Arc::new(AtomicBool::new(false));
                    let outcome = this.run_job(&key, &pool, &released);
                    if !released.swap(true, Ordering::SeqCst) {
                        this.pending.fetch_sub(1, Ordering::SeqCst);
                    }
                    if this.handed_off.load(Ordering::SeqCst) {
                        // The next owner adopted the worker.
                        return;
                    }
                    let mut data = this.data.lock().unwrap();
                    if this.handed_off.load(Ordering::SeqCst) {
                        return;
                    }
                    let job = data.jobs.get_mut(&key).unwrap();
                    match outcome {
                        Ok(result) => {
                            finish_capacity(job, result);
                        }
                        Err(_) => {
                            job["status"] = json!("uncertain");
                            job["error"] = json!(
                                "Runner operation did not complete. Inspect GitHub and the private worker receipt; no automatic replay."
                            );
                        }
                    }
                    job["duration_seconds"] = json!(started.elapsed().as_secs_f64());
                    if this.save(&data).is_err() {
                        job_persistence_failure(&mut data, &key);
                    }
                    drop(data);
                    this.wake_dispatch();
                });
            }
        }
        Ok(())
    }
    pub fn pause_admission(&self) {
        self.admission_paused.store(true, Ordering::SeqCst);
        self.retire_reserves();
    }
    pub fn resume_admission(&self) {
        self.admission_paused.store(false, Ordering::SeqCst);
        self.wake_dispatch();
    }
    /// Retiring an idle listener requests its normal cleanup. Only the worker's
    /// terminal proof and registration cleanup release the owner's drain hold.
    fn retire_reserves(&self) {
        if !self.data.lock().unwrap().jobs.values().any(|job| {
            job["reserve"] == true
                && job["status"] == "running"
                && job["assigned_job_id"].is_null()
                && job["reserve_busy"] != true
        }) {
            return;
        }
        if self.retiring_reserves.swap(true, Ordering::SeqCst) {
            return;
        }
        let this = self.clone();
        std::thread::spawn(move || {
            loop {
                if this.handed_off.load(Ordering::SeqCst) {
                    break;
                }
                let (jobs, mut provisioning) = {
                    let data = this.data.lock().unwrap();
                    let stop_all = data.draining || this.admission_paused.load(Ordering::SeqCst);
                    let idle = data
                        .jobs
                        .iter()
                        .filter(|(_, j)| {
                            j["reserve"] == true
                                && j["status"] == "running"
                                && j["assigned_job_id"].is_null()
                                && j["reserve_busy"] != true
                                && (stop_all
                                    || j["retire_requested"] == true
                                    || this.config.as_ref().is_some_and(|config| {
                                        config
                                            .pools
                                            .iter()
                                            .find(|p| j["pool"] == p.id)
                                            .and_then(|p| p.standby.as_ref())
                                            .is_some_and(|policy| {
                                                j["timings"]["jit_received_at_ms"]
                                                    .as_u64()
                                                    .is_some_and(|at| {
                                                        at + policy.idle_timeout_seconds * 1000
                                                            <= now_ms()
                                                    })
                                            })
                                    }))
                        })
                        .collect::<Vec<_>>();
                    let provisioning = idle.iter().any(|(_, j)| !j["worker_pid"].is_u64());
                    let jobs = idle
                        .into_iter()
                        .filter(|(_, j)| j["worker_pid"].is_u64())
                        .map(|(k, j)| (k.clone(), j.clone()))
                        .collect::<Vec<_>>();
                    (jobs, provisioning)
                };
                for (key, attempt) in jobs {
                    let result = this.retire_reserve(&key, &attempt);
                    let mut data = this.data.lock().unwrap();
                    if let Some(job) = data.jobs.get_mut(&key) {
                        if job["runner_name"] != attempt["runner_name"]
                            || job["runner_id"] != attempt["runner_id"]
                        {
                            continue;
                        }
                        match result {
                            Ok(Some(true)) => job["reserve_busy"] = json!(true),
                            Ok(Some(false)) => {
                                job["retire_requested"] = json!(true);
                                provisioning = true;
                            }
                            Ok(None) => provisioning = true,
                            Err(_) => {
                                job["retire_error"] = json!(true);
                                provisioning = true;
                            }
                        }
                        if this.save(&data).is_err() {
                            job_persistence_failure(&mut data, &key);
                        }
                    }
                }
                if !provisioning {
                    break;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            this.retiring_reserves.store(false, Ordering::SeqCst);
        });
    }
    fn retire_reserve(&self, key: &str, job: &Value) -> Result<Option<bool>> {
        let pool = self
            .config
            .as_ref()
            .context("GitHub disabled")?
            .pools
            .iter()
            .find(|p| job["pool"] == p.id)
            .context("Reserve pool vanished")?;
        let runner_name = text(job, "runner_name")?;
        if job["registration_retired"] != true {
            let token = self.api.call(
                "POST",
                &format!("/app/installations/{}/access_tokens", pool.installation_id),
                &api::jwt(self.app("runners")?)?,
                Some(json!({"permissions":{"organization_self_hosted_runners":"write"}})),
            )?;
            if !self.api.retire_runner(
                &format!(
                    "/orgs/{}/actions/runners/{}",
                    pool.organization,
                    number(job, "runner_id")?
                ),
                text(&token, "token")?,
                runner_name,
            )? {
                return Ok(Some(true));
            }
            // No VM retirement is authorized until the exact registration has
            // been removed. A failed/ambiguous delete leaves the worker alone.
            let mut data = self.data.lock().unwrap();
            let current = data.jobs.get(key).context("Reserve vanished")?;
            ensure!(
                current["runner_id"] == job["runner_id"]
                    && current["runner_name"] == job["runner_name"],
                "Reserve attempt changed"
            );
            let job = data.jobs.get_mut(key).unwrap();
            job["registration_retired"] = json!(true);
            job["timings"]["registration_retired_at_ms"] = json!(now_ms());
            job.as_object_mut().unwrap().remove("runner_observation");
            self.save(&data)?;
        }

        let mut command = std::process::Command::new(&pool.worker[0]);
        command
            .args(&pool.worker[1..])
            .args(["--retire-reserve", runner_name])
            .env_clear();
        for key in ["PATH", "HOME", "SSH_AUTH_SOCK"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        let output = run_command(command, None, Duration::from_secs(60))?;
        let result: Value = serde_json::from_slice(&output)?;
        ensure!(
            result["schema"] == 1
                && result["name"] == runner_name
                && matches!(
                    result["reserve_retire"].as_str(),
                    Some("requested" | "busy" | "gone")
                ),
            "Invalid reserve retirement result"
        );
        if result["reserve_retire"] == "gone" && worker_alive(job) {
            return Ok(None);
        }
        Ok(Some(result["reserve_retire"] == "busy"))
    }
    /// This owner hands state to the next controller: worker threads stop
    /// writing, and live workers are left for the next owner to adopt.
    pub fn hand_off(&self) {
        let _data = self.data.lock().unwrap();
        self.handed_off.store(true, Ordering::SeqCst);
    }
    /// Follow workers a previous owner left running to their end, once each.
    pub fn adopt_workers(&self) {
        let Some(config) = &self.config else {
            return;
        };
        for key in std::mem::take(&mut *self.adoptable.lock().unwrap()) {
            let job = self.data.lock().unwrap().jobs.get(&key).cloned();
            let Some(pool) = job
                .as_ref()
                .and_then(|j| config.pools.iter().find(|p| j["pool"] == p.id))
                .cloned()
            else {
                continue;
            };
            let this = self.clone();
            std::thread::spawn(move || {
                let started = Instant::now();
                while job.as_ref().is_some_and(worker_alive) {
                    if this.handed_off.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(Duration::from_secs(5));
                }
                let outcome = job
                    .as_ref()
                    .context("Adopted job vanished")
                    .and_then(read_worker_output)
                    .and_then(|output| this.complete_job(&key, &pool, &output));
                if this.handed_off.load(Ordering::SeqCst) {
                    return;
                }
                let mut data = this.data.lock().unwrap();
                if this.handed_off.load(Ordering::SeqCst) {
                    return;
                }
                let Some(job) = data.jobs.get_mut(&key) else {
                    return;
                };
                match outcome {
                    Ok(result) => finish_capacity(job, result),
                    Err(_) => {
                        job["status"] = json!("uncertain");
                        job["error"] = json!(
                            "Adopted runner did not report cleanup. Inspect GitHub and the private worker receipt; no automatic replay."
                        );
                    }
                }
                job["adopted_seconds"] = json!(started.elapsed().as_secs_f64());
                if this.save(&data).is_err() {
                    job_persistence_failure(&mut data, &key);
                }
                drop(data);
                this.wake_dispatch();
            });
        }
    }
    /// Signed assignment events are the fast path. Recover a missed event by
    /// observing only our exact live runner; an offline/missing one keeps its slot.
    fn reconcile_standby_assignments(&self, config: &crate::Config) {
        let candidates = self
            .data
            .lock()
            .unwrap()
            .jobs
            .iter()
            .filter(|(_, j)| {
                j["reserve"] == true
                    && j["status"] == "running"
                    && j["runner_id"].is_u64()
                    && j["assigned_job_id"].is_null()
                    && j["reserve_busy"] != true
            })
            .map(|(k, j)| (k.clone(), j.clone()))
            .collect::<Vec<_>>();
        for (key, job) in candidates {
            if self.admission_paused.load(Ordering::SeqCst)
                || self.handed_off.load(Ordering::SeqCst)
            {
                return;
            }
            let Some(pool) = config.pools.iter().find(|p| job["pool"] == p.id) else {
                continue;
            };
            let observation = (|| -> Result<Value> {
                let token = self.api.call(
                    "POST",
                    &format!("/app/installations/{}/access_tokens", pool.installation_id),
                    &api::jwt(self.app("runners")?)?,
                    Some(json!({"permissions":{"organization_self_hosted_runners":"write"}})),
                )?;
                self.api.call(
                    "GET",
                    &format!(
                        "/orgs/{}/actions/runners/{}",
                        pool.organization,
                        number(&job, "runner_id")?
                    ),
                    text(&token, "token")?,
                    None,
                )
            })();
            if let Ok(runner) = observation
                && runner["name"] == job["runner_name"]
            {
                let mut data = self.data.lock().unwrap();
                if let Some(job) = data.jobs.get_mut(&key)
                    && job["status"] == "running"
                {
                    job["runner_observation"] = json!({"online":runner["status"] == "online", "busy":runner["busy"] == true,"observed_at_ms":now_ms()});
                    if runner["status"] == "online"
                        && job["timings"]["runner_online_observed_at_ms"].is_null()
                    {
                        job["timings"]["runner_online_observed_at_ms"] = json!(now_ms());
                    }
                    if runner["busy"] == true {
                        job["reserve_busy"] = json!(true);
                        job["timings"]["assignment_observed_at_ms"] = json!(now_ms());
                    }
                    if self.save(&data).is_err() {
                        job_persistence_failure(&mut data, &key);
                    }
                }
                drop(data);
                self.wake_dispatch();
            }
        }
    }
    /// Uncertain jobs (a restart lost the worker's report): when GitHub shows
    /// the job finished and its runner gone, have the worker remove that
    /// request's VM (only the VM its receipt names) and mark the job finished.
    /// Anything else waits and is retried later; nothing is replayed.
    fn reconcile_uncertain(&self, config: &crate::Config) {
        let now = api::now();
        let candidates: Vec<(String, Value)> = {
            let data = self.data.lock().unwrap();
            data.jobs
                .iter()
                .filter(|(_, j)| {
                    j["status"] == "uncertain"
                        && j["runner_name"].is_string()
                        && j["runner_id"].is_u64()
                        && j["reconcile_after"].as_u64().unwrap_or(0) <= now
                })
                .map(|(k, j)| (k.clone(), j.clone()))
                .collect()
        };
        for (key, job) in candidates {
            let Some(pool) = config.pools.iter().find(|p| job["pool"] == p.id) else {
                continue;
            };
            let outcome = self.reclaim(pool, &job);
            if self.handed_off.load(Ordering::SeqCst) {
                return;
            }
            let mut data = self.data.lock().unwrap();
            let Some(job) = data.jobs.get_mut(&key) else {
                continue;
            };
            match outcome {
                Ok(Some(result)) => {
                    job["status"] = json!("finished");
                    job["reclaimed"] = result["reclaimed"].clone();
                    job.as_object_mut().unwrap().remove("error");
                    job.as_object_mut().unwrap().remove("reconcile_after");
                }
                // Not yet provably over, or the check failed: look again later.
                Ok(None) | Err(_) => job["reconcile_after"] = json!(now + RECONCILE_INTERVAL),
            }
            if self.save(&data).is_err() {
                job_persistence_failure(&mut data, &key);
            }
        }
    }
    /// `Some(worker result)` when the job was provably over and its VM reclaimed.
    fn reclaim(&self, pool: &Pool, job: &Value) -> Result<Option<Value>> {
        let job_id = if job["reserve"] == true {
            let Some(id) = job["assigned_job_id"].as_u64().filter(|id| *id > 0) else {
                return Ok(None);
            };
            id
        } else {
            number(job, "id")?
        };
        let token = self.api.call(
            "POST",
            &format!("/app/installations/{}/access_tokens", pool.installation_id),
            &api::jwt(self.app("runners")?)?,
            Some(json!({"permissions":{"organization_self_hosted_runners":"write","actions":"read"}})),
        )?;
        let token = text(&token, "token")?;
        let repo = text(job, "repository")?;
        full_name(repo)?;
        let current = self.api.call(
            "GET",
            &format!("/repos/{repo}/actions/jobs/{job_id}"),
            token,
            None,
        )?;
        if current["status"] != "completed" {
            return Ok(None);
        }
        let runner_name = text(job, "runner_name")?;
        // Gone already, or idle and ours: removed. Busy or renamed: refused.
        self.api.cleanup_runner(
            &format!(
                "/orgs/{}/actions/runners/{}",
                pool.organization,
                number(job, "runner_id")?
            ),
            token,
            runner_name,
        )?;
        let mut command = std::process::Command::new(&pool.worker[0]);
        command
            .args(&pool.worker[1..])
            .args(["--reclaim", runner_name])
            .env_clear();
        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }
        let output = run_command(command, None, Duration::from_secs(180))?;
        let result: Value = serde_json::from_slice(&output)?;
        ensure!(
            result["schema"] == 1 && result["cleaned"] == true && result["name"] == runner_name,
            "Worker did not prove reclamation"
        );
        Ok(Some(result))
    }
    fn run_job(&self, key: &str, pool: &Pool, released: &AtomicBool) -> Result<Value> {
        let (job, user_token) = {
            let mut data = self.data.lock().unwrap();
            let user_token = self.user_token("runners", &mut data)?;
            (data.jobs.get(key).cloned(), user_token)
        };
        let installations = self
            .api
            .pages("/user/installations", "installations", &user_token)?;
        ensure!(
            installations.iter().any(|i| i["id"] == pool.installation_id
                && i["account"]["login"]
                    .as_str()
                    .is_some_and(|s| s.eq_ignore_ascii_case(&pool.organization))
                && i["suspended_at"].is_null()
                && i["permissions"]["organization_self_hosted_runners"] == "write"),
            "Approve organization self-hosted runners permission in the GitHub App installation"
        );
        let job = job.context("CI job vanished")?;
        let token = self.api.call("POST", &format!("/app/installations/{}/access_tokens", pool.installation_id),
            &api::jwt(self.app("runners")?)?, Some(json!({"permissions":{"organization_self_hosted_runners":"write","actions":"read"}})))?;
        let token = text(&token, "token")?.to_owned();
        let repo = text(&job, "repository")?;
        full_name(repo)?;
        // User grant and selected installation remain live requirements.
        let selected = {
            self.data
                .lock()
                .unwrap()
                .repositories
                .get(&number(&job, "repository_id")?)
                .cloned()
                .context("Repository deselected")?
        };
        let available = self.repositories(selected.installation_id)?;
        ensure!(
            available["repositories"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["id"] == selected.id),
            "Repository access revoked"
        );
        let reserve = job["reserve"] == true;
        if !reserve {
            let current = self.api.call(
                "GET",
                &format!("/repos/{repo}/actions/jobs/{}", number(&job, "id")?),
                &token,
                None,
            )?;
            if current["status"] != "queued" {
                return Ok(json!({"skipped":true,"reason":"Job is no longer queued"}));
            }
            ensure!(
                current["run_id"] == job["run_id"],
                "Workflow job identity changed"
            );
        }
        if let Some(standby) = &pool.standby {
            ensure!(
                selected.private && selected.id == standby.repository_id,
                "Standby repository changed"
            );
            let group_path = format!(
                "/orgs/{}/actions/runner-groups/{}",
                pool.organization, pool.runner_group_id
            );
            let group = self.api.call("GET", &group_path, &token, None)?;
            let repositories = self.api.pages(
                &format!("{group_path}/repositories"),
                "repositories",
                &token,
            )?;
            check_group(
                &group,
                &repositories,
                &Preparation {
                    repository_id: standby.repository_id,
                    workflows: standby.workflows.clone(),
                },
            )?;
        }
        let verification_token = if let Some(preparation) = &pool.preparation {
            ensure!(
                selected.private && selected.id == preparation.repository_id,
                "Prepared repository changed"
            );
            let group_path = format!(
                "/orgs/{}/actions/runner-groups/{}",
                pool.organization, pool.runner_group_id
            );
            let group = self.api.call("GET", &group_path, &token, None)?;
            let repositories = self.api.pages(
                &format!("{group_path}/repositories"),
                "repositories",
                &token,
            )?;
            check_group(&group, &repositories, preparation)?;
            let credential = self.api.call(
                "POST",
                &format!("/app/installations/{}/access_tokens", pool.installation_id),
                &api::jwt(self.app("runners")?)?,
                Some(json!({"repository_ids":[selected.id],"permissions":{"actions":"read"}})),
            )?;
            Some(text(&credential, "token")?.to_owned())
        } else {
            None
        };
        let runner_name = format!("ebox-ci-{}", uuid::Uuid::new_v4().simple());
        self.record_phase(key, "jit_requested_at_ms")?;
        // JIT is single-use; a lost response is never retried automatically.
        let generated=self.api.call("POST",&format!("/orgs/{}/actions/runners/generate-jitconfig",pool.organization),&token,Some(json!({"name":runner_name,"runner_group_id":pool.runner_group_id,"labels":pool.labels,"work_folder":if pool.preparation.is_some() {"/project/work"} else {"_work"}})))?;
        let runner_id = number(&generated["runner"], "id")?;
        {
            let mut data = self.data.lock().unwrap();
            let job = data.jobs.get_mut(key).unwrap();
            job["timings"]["jit_received_at_ms"] = json!(now_ms());
            job["runner_id"] = json!(runner_id);
            job["runner_name"] = json!(runner_name);
            self.save(&data)?;
        }
        let mut request = json!({"schema":1,"name":runner_name,"job_id":job["id"],"repository":repo,"jitconfig":text(&generated,"encoded_jit_config")?,"timeout_seconds":pool.timeout_seconds});
        if reserve {
            request["reserve"] = json!(true);
            request["idle_timeout_seconds"] = json!(
                pool.standby
                    .as_ref()
                    .context("Standby policy missing")?
                    .idle_timeout_seconds
            );
        }
        if let Some(preparation) = &pool.preparation {
            request["preparation"] = json!({"repository_id":selected.id,"installation_id":pool.installation_id,"runner_id":runner_id,"workflows":preparation.workflows,"verification_token":verification_token});
        }
        let mut command = std::process::Command::new(&pool.worker[0]);
        command.args(&pool.worker[1..]).env_clear();
        for key in [
            "PATH",
            "HOME",
            "SystemRoot",
            "SYSTEMROOT",
            "WINDIR",
            "TMPDIR",
            "TEMP",
            "TMP",
            "SSH_AUTH_SOCK",
        ] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        // Cold generation resolution has its own 20-minute bound before VM execution.
        let timeout = Duration::from_secs(
            pool.timeout_seconds
                + if reserve {
                    pool.standby.as_ref().unwrap().idle_timeout_seconds
                } else {
                    0
                }
                + if pool.preparation.is_some() {
                    1800
                } else {
                    600
                },
        );
        // The worker runs detached (its own process group, output to a private
        // file): a controller handoff neither waits for nor interrupts it.
        let out = self.root.join("workers").join(format!("{key}.out"));
        self.record_phase(key, "worker_launch_requested_at_ms")?;
        let mut child = spawn_worker(command, &serde_json::to_vec(&request)?, &out)?;
        {
            let mut data = self.data.lock().unwrap();
            let job = data.jobs.get_mut(key).unwrap();
            job["timings"]["worker_launched_at_ms"] = json!(now_ms());
            job["worker_pid"] = json!(child.id());
            job["worker_program"] = json!(pool.worker[0]);
            job["worker_output"] = json!(out.display().to_string());
            self.save(&data)?;
        }
        if !released.swap(true, Ordering::SeqCst) {
            self.pending.fetch_sub(1, Ordering::SeqCst);
        }
        let deadline = Instant::now() + timeout;
        loop {
            if self.handed_off.load(Ordering::SeqCst) {
                anyhow::bail!("Handed to the next controller");
            }
            if child.try_wait()?.is_some() {
                break;
            }
            if !reserve && Instant::now() > deadline {
                kill_group(&mut child);
                anyhow::bail!("Worker exceeded its bound");
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        let job = self.data.lock().unwrap().jobs[key].clone();
        let output = read_worker_output(&job)?;
        self.complete_job(key, pool, &output)
    }
    fn record_phase(&self, key: &str, phase: &str) -> Result<()> {
        let mut data = self.data.lock().unwrap();
        data.jobs.get_mut(key).context("CI job vanished")?["timings"][phase] = json!(now_ms());
        self.save(&data)
    }
    /// Check a finished worker's report, remove its runner registration and
    /// confirm the job's identity (also used for an adopted worker).
    fn complete_job(&self, key: &str, pool: &Pool, output: &[u8]) -> Result<Value> {
        let job = self.data.lock().unwrap().jobs[key].clone();
        let repo = text(&job, "repository")?;
        let runner_id = number(&job, "runner_id")?;
        let runner_name = text(&job, "runner_name")?.to_owned();
        let selected_id = number(&job, "repository_id")?;
        let result: Value = serde_json::from_slice(output)?;
        ensure!(
            result["schema"] == 1
                && result["cleaned"] == true
                && result["name"] == runner_name
                && result["job_id"] == job["id"]
                && result["runner_version"]
                    .as_str()
                    .is_some_and(|v| v.len() <= 32
                        && v.split('.').count() == 3
                        && v.split('.').all(|p| p.parse::<u32>().is_ok()))
                && result["duration_seconds"]
                    .as_f64()
                    .is_some_and(|v| v.is_finite() && v >= 0.0),
            "Worker did not prove cleanup"
        );
        let capacity_wait = capacity_wait_proof(&result)?;
        let reserve_expired = job["reserve"] == true
            && result["status"] == "reserve_expired"
            && result["assignment"].is_null();
        if !capacity_wait && !reserve_expired && pool.preparation.is_some() {
            let actual = &result["assignment"];
            ensure!(
                actual["verified"] == true
                    && actual["repository_id"] == selected_id
                    && actual["runner_id"] == runner_id
                    && actual["job_id"].as_u64().is_some_and(|id| id > 0)
                    && actual["run_id"].as_u64().is_some_and(|id| id > 0)
                    && actual["run_attempt"].as_u64().is_some_and(|id| id > 0),
                "Worker did not verify actual assignment"
            );
        }
        let cleanup_token = self.api.call(
            "POST",
            &format!("/app/installations/{}/access_tokens", pool.installation_id),
            &api::jwt(self.app("runners")?)?,
            Some(json!({"permissions":{"organization_self_hosted_runners":"write","actions":"read"}})),
        )?;
        self.api.cleanup_runner(
            &format!("/orgs/{}/actions/runners/{runner_id}", pool.organization),
            text(&cleanup_token, "token")?,
            &runner_name,
        )?;
        // JIT runners accept any matching job. Only proved cleanup plus a live
        // queued observation permits a new capacity attempt for this demand.
        let demand_pending = if job["reserve"] == true {
            false
        } else {
            let remaining = self.api.call(
                "GET",
                &format!("/repos/{repo}/actions/jobs/{}", number(&job, "id")?),
                text(&cleanup_token, "token")?,
                None,
            )?;
            ensure!(
                remaining["id"] == job["id"] && remaining["run_id"] == job["run_id"],
                "Workflow job identity changed"
            );
            remaining["status"] == "queued"
        };
        let worker_phases = [
            "worker_started_at_ms",
            "allocation_started_at_ms",
            "allocated_at_ms",
            "preparation_resolved_at_ms",
            "preparation_attached_at_ms",
            "boot_started_at_ms",
            "guest_ready_at_ms",
            "runner_started_at_ms",
            "job_started_at_ms",
            "assignment_verified_at_ms",
            "runner_exited_at_ms",
            "destroyed_at_ms",
        ]
        .into_iter()
        .filter_map(|phase| {
            result["phases"][phase]
                .as_u64()
                .map(|at| (phase.to_owned(), json!(at)))
        })
        .collect::<serde_json::Map<String, Value>>();
        // Do not publish arbitrary worker stdout (could include JIT or workflow secrets).
        Ok(
            json!({"cleaned":true,"runner_id":runner_id,"runner_version":result["runner_version"],"worker_seconds":result["duration_seconds"],"demand_pending":demand_pending, "prepared":pool.preparation.is_some(),"actual_job_id":result["assignment"]["job_id"].as_u64(),"capacity_wait":capacity_wait,"reserve_expired":reserve_expired,"worker_phases":worker_phases}),
        )
    }
}
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
/// How long an uncertain job waits between reconciliation attempts.
const RECONCILE_INTERVAL: u64 = 600;
const MAX_WORKER_OUTPUT: u64 = 1024 * 1024;
/// Start a worker in its own process group with stdin from a private file
/// and stdout to `out`, so it outlives this controller process.
fn spawn_worker(
    mut command: std::process::Command,
    input: &[u8],
    out: &Path,
) -> Result<std::process::Child> {
    enginebox_core::state::private_dir(out.parent().context("Invalid worker output path")?)?;
    let mut stdin = tempfile::tempfile()?;
    stdin.write_all(input)?;
    stdin.seek(SeekFrom::Start(0))?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let stdout = options.open(out)?;
    command
        .stdin(stdin)
        .stdout(stdout)
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command.spawn().context("Cannot start CI worker")
}
fn kill_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}
/// A finished worker's report, then its output file is removed.
fn read_worker_output(job: &Value) -> Result<Vec<u8>> {
    let path = PathBuf::from(text(job, "worker_output")?);
    let mut bytes = Vec::new();
    std::fs::File::open(&path)?
        .take(MAX_WORKER_OUTPUT + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_WORKER_OUTPUT,
        "Oversized worker report"
    );
    let _ = std::fs::remove_file(&path);
    Ok(bytes)
}
/// Whether the job's recorded worker process is still that worker (not a reused pid).
pub(crate) fn worker_alive(job: &Value) -> bool {
    let (Some(pid), Some(program)) = (job["worker_pid"].as_u64(), job["worker_program"].as_str())
    else {
        return false;
    };
    #[cfg(target_os = "linux")]
    {
        std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|raw| {
            raw.split(|b| *b == 0)
                .next()
                .is_some_and(|first| first == program.as_bytes())
        })
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let _ = program;
        i32::try_from(pid).is_ok_and(|pid| unsafe { libc::kill(pid, 0) == 0 })
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, program);
        false
    }
}
fn capacity_wait_proof(result: &Value) -> Result<bool> {
    if result["status"] != "waiting_capacity" {
        return Ok(false);
    }
    ensure!(
        result["cleaned"] == true
            && result["allocation_started"] == false
            && result["allocation_id"].is_null()
            && result["instance_uuid"].is_null()
            && result["checkout"].is_null()
            && result["assignment"].is_null(),
        "Capacity wait must prove no allocation or assignment was attempted"
    );
    Ok(true)
}
fn finish_capacity(job: &mut Value, result: Value) {
    // A newer webhook wins over the earlier API observation.
    job["status"] = json!(
        if (result["demand_pending"] == true && job["github_status"] == "queued")
            || (job["reserve"] == true && result["capacity_wait"] == true)
        {
            "queued"
        } else {
            "finished"
        }
    );
    if job["status"] == "queued" && result["capacity_wait"] == true {
        job["capacity_retry_at"] = json!(api::now() + 60);
    } else {
        job.as_object_mut().unwrap().remove("capacity_retry_at");
    }
    if job["reserve"] == true
        && result["reserve_expired"] == true
        && job["assigned_job_id"].is_null()
    {
        job["reserve_retry_at"] = json!(api::now() + 60);
    }
    job["result"] = result;
}
fn job_persistence_failure(data: &mut crate::Data, key: &str) {
    data.jobs.get_mut(key).unwrap()["status"] = json!("uncertain");
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn retirement_checks_github_before_invoking_the_worker() {
        use crate::tests::{api_fixture, shared_service, with_app_key};
        use std::os::unix::fs::PermissionsExt;
        for busy in [false, true] {
            let (root, mut g) = shared_service();
            with_app_key(&mut g);
            let marker = root.path().join("retire-called");
            let worker = root.path().join("retire-fixture");
            std::fs::write(&worker,format!("#!/bin/sh\ntouch '{}'\nprintf '%s' '{{\"schema\":1,\"name\":\"ebox-ci-fixture\",\"reserve_retire\":\"requested\"}}'\n",marker.display())).unwrap();
            std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o700)).unwrap();
            g.config.as_mut().unwrap().pools[0].worker = vec![worker.display().to_string()];
            let job = json!({"id":0,"reserve":true,"pool":"linux","status":"running","runner_name":"ebox-ci-fixture","runner_id":501});
            g.data
                .lock()
                .unwrap()
                .jobs
                .insert("reserve-test".into(), job.clone());
            let mut responses = vec![
                (
                    "/app/installations/2/access_tokens",
                    json!({"token":"fixture-retire-token"}),
                ),
                (
                    "GET /orgs/fixture/actions/runners/501",
                    json!({"name":"ebox-ci-fixture","busy":busy}),
                ),
            ];
            if !busy {
                responses.push(("DELETE /orgs/fixture/actions/runners/501", json!({})));
            }
            let (api, server) = api_fixture(responses);
            g.api = api;
            assert_eq!(g.retire_reserve("reserve-test", &job).unwrap(), Some(busy));
            server.join().unwrap();
            assert_eq!(marker.exists(), !busy);
            assert_eq!(
                g.data.lock().unwrap().jobs["reserve-test"]["registration_retired"] == true,
                !busy
            );
            // No retirement response proves VM deletion or releases its slot.
            assert_eq!(g.pending(), 1);
        }
    }
    #[test]
    fn unassigned_uncertain_reserve_does_not_query_a_fictitious_job() {
        let (_root, g) = crate::tests::service();
        let result = g
            .reclaim(
                &g.config.as_ref().unwrap().pools[0],
                &json!({"reserve":true,"id":0}),
            )
            .unwrap();
        assert!(result.is_none());
    }
    #[test]
    fn periodic_runner_observation_recovers_missed_assignment() {
        for busy in [false, true] {
            let (_root, mut g) = crate::tests::shared_service();
            crate::tests::with_app_key(&mut g);
            g.data.lock().unwrap().jobs.insert("reserve-test".into(),json!({"id":0,"reserve":true,"pool":"linux","status":"running","runner_name":"ebox-ci-fixture","runner_id":501}));
            let (api, server) = crate::tests::api_fixture(vec![
                (
                    "/app/installations/2/access_tokens",
                    json!({"token":"fixture-observe-token"}),
                ),
                (
                    "GET /orgs/fixture/actions/runners/501",
                    json!({"name":"ebox-ci-fixture","busy":busy,"status":"online"}),
                ),
            ]);
            g.api = api;
            g.reconcile_standby_assignments(g.config.as_ref().unwrap());
            server.join().unwrap();
            let data = g.data.lock().unwrap();
            let job = &data.jobs["reserve-test"];
            assert_eq!(job["reserve_busy"] == true, busy);
            assert!(
                job["timings"]["runner_online_observed_at_ms"]
                    .as_u64()
                    .unwrap()
                    > 0
            );
            assert_eq!(job["runner_observation"]["online"], true);
        }
    }
    #[cfg(unix)]
    #[test]
    fn drain_retires_idle_reserves_but_busy_jobs_keep_running() {
        use std::os::unix::fs::PermissionsExt;
        for state in ["requested", "busy"] {
            let (root, mut g) = crate::tests::service();
            let worker = root.path().join("retire-fixture");
            std::fs::write(&worker, format!("#!/bin/sh\ntest \"$1\" = --retire-reserve || exit 2\nprintf '%s' '{{\"schema\":1,\"name\":\"ebox-ci-fixture\",\"reserve_retire\":\"{state}\"}}'\n")).unwrap();
            std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o700)).unwrap();
            g.config.as_mut().unwrap().pools[0].worker = vec![worker.display().to_string()];
            g.data.lock().unwrap().jobs.insert("reserve-test".into(),json!({"id":0,"reserve":true,"pool":"linux","status":"running","runner_name":"ebox-ci-fixture","worker_pid":1,"registration_retired":true}));
            assert_eq!(g.pending(), 1);
            g.pause_admission();
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let data = g.data.lock().unwrap();
                let job = &data.jobs["reserve-test"];
                if job["retire_requested"] == true || job["reserve_busy"] == true {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "retirement did not reach the worker"
                );
                drop(data);
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(
                g.data.lock().unwrap().jobs["reserve-test"]["status"],
                "running"
            );
            if state == "requested" {
                assert!(g.pending() >= 1);
            }
            // Only normal worker completion + exact registration cleanup may
            // publish finished. A retirement response alone never does so.
            g.data.lock().unwrap().jobs.get_mut("reserve-test").unwrap()["status"] =
                json!("finished");
            while g.retiring_reserves.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(!g.retiring_reserves.load(Ordering::SeqCst));
            assert_eq!(g.pending(), 0);
            g.data.lock().unwrap().jobs.get_mut("reserve-test").unwrap()["status"] =
                json!("uncertain");
            assert_eq!(g.pending(), 1);
        }
    }
    #[test]
    fn capacity_wait_never_accepts_an_ambiguous_or_started_allocation() {
        let clean = json!({"status":"waiting_capacity","allocation_started":false,"cleaned":true});
        assert!(capacity_wait_proof(&clean).unwrap());
        for key in ["allocation_id", "instance_uuid", "checkout", "assignment"] {
            let mut value = clean.clone();
            value[key] = json!("present");
            assert!(capacity_wait_proof(&value).is_err());
        }
        for key in ["allocation_started", "cleaned"] {
            let mut value = clean.clone();
            value.as_object_mut().unwrap().remove(key);
            assert!(capacity_wait_proof(&value).is_err());
        }
        let mut started = clean;
        started["allocation_started"] = json!(true);
        assert!(capacity_wait_proof(&started).is_err());
        assert!(!capacity_wait_proof(&json!({"status":"finished"})).unwrap());
    }
    #[test]
    fn shared_session_does_not_bypass_org_approval_or_suspension() {
        for installation in [
            json!({"id":2,"account":{"login":"fixture"},"permissions":{"contents":"write"}}),
            json!({"id":2,"account":{"login":"fixture"},"permissions":{"organization_self_hosted_runners":"write"},"suspended_at":"2026-01-01"}),
        ] {
            let (_root, mut g) = crate::tests::shared_service();
            crate::tests::account(&g);
            let (api, server) = crate::tests::api_fixture(vec![(
                "/user/installations?",
                json!({"installations":[installation]}),
            )]);
            g.api = api;
            assert!(
                g.run_job(
                    "2-99",
                    &g.config.as_ref().unwrap().pools[0],
                    &AtomicBool::new(true)
                )
                .unwrap_err()
                .to_string()
                .contains("Approve organization")
            );
            server.join().unwrap();
            assert!(g.status()["jobs"].as_array().unwrap().is_empty());
        }
    }
    #[test]
    fn prepared_groups_enforce_repo_and_workflow_scope() {
        let preparation = Preparation {
            repository_id: 10,
            workflows: vec!["fixture/repo/.github/workflows/ci.yml@refs/heads/main".into()],
        };
        let mut group = json!({"visibility":"selected","allows_public_repositories":false,"restricted_to_workflows":true,"selected_workflows":preparation.workflows});
        check_group(&group, &[json!({"id":10})], &preparation).unwrap();
        assert!(check_group(&group, &[json!({"id":10}), json!({"id":11})], &preparation).is_err());
        assert!(check_group(&group, &[json!({"id":11})], &preparation).is_err());
        group["restricted_to_workflows"] = json!(false);
        assert!(check_group(&group, &[json!({"id":10})], &preparation).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn jit_worker_cleanup_is_owned_and_worker_output_is_filtered() {
        for shared in [false, true] {
            jit_fixture(false, shared, false, false);
            jit_fixture(true, shared, false, false);
        }
    }
    #[cfg(unix)]
    #[test]
    fn capacity_wait_removes_unused_registration_and_defers_the_pool() {
        for shared in [false, true] {
            jit_fixture(false, shared, true, false);
            jit_fixture(true, shared, true, false);
        }
    }
    #[cfg(unix)]
    #[test]
    fn a_live_worker_is_adopted_after_a_restart_and_a_dead_one_is_uncertain() {
        let (root, g) = crate::tests::shared_service();
        {
            let mut data = g.data.lock().unwrap();
            // This test process stands in for a worker that outlived its controller.
            data.jobs.insert("2-1".into(), json!({"id":1,"status":"running","pool":"linux","worker_pid":std::process::id(),"worker_program":std::env::args().next().unwrap()}));
            data.jobs.insert("2-2".into(), json!({"id":2,"status":"running","pool":"linux","worker_pid":999_999_999u64,"worker_program":"/nonexistent"}));
            data.jobs.insert("reserve-live".into(), json!({"id":0,"reserve":true,"assigned_job_id":42,"status":"running","pool":"linux","worker_pid":std::process::id(),"worker_program":std::env::args().next().unwrap()}));
            data.jobs.insert("reserve-dead".into(), json!({"id":0,"reserve":true,"status":"running","pool":"linux","worker_pid":999_999_999u64,"worker_program":"/nonexistent"}));
            g.save(&data).unwrap();
        }
        let reopened = GitHub::open(root.path()).unwrap();
        assert_eq!(
            reopened.adoptable.lock().unwrap().as_slice(),
            ["2-1".to_owned(), "reserve-live".to_owned()]
        );
        let data = reopened.data.lock().unwrap();
        assert_eq!(data.jobs["2-1"]["status"], "running");
        assert_eq!(data.jobs["2-2"]["status"], "uncertain");
        assert_eq!(data.jobs["reserve-live"]["assigned_job_id"], 42);
        assert_eq!(data.jobs["reserve-live"]["status"], "running");
        assert_eq!(data.jobs["reserve-dead"]["status"], "uncertain");
        drop(data);
        assert_eq!(reopened.pending(), 1);
    }
    #[cfg(unix)]
    #[test]
    fn an_uncertain_job_is_reclaimed_once_github_shows_it_over() {
        use std::os::unix::fs::PermissionsExt;
        let (root, mut g) = crate::tests::shared_service();
        crate::tests::with_app_key(&mut g);
        let worker = root.path().join("reclaim-worker");
        std::fs::write(
            &worker,
            "#!/bin/sh\n[ \"$1\" = --reclaim ] || exit 2\nprintf '{\"schema\":1,\"name\":\"%s\",\"reclaimed\":true,\"cleaned\":true}\\n' \"$2\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o700)).unwrap();
        g.config.as_mut().unwrap().pools[0].worker = vec![worker.display().to_string()];
        let name = "ebox-ci-0123456789abcdef0123456789abcdef";
        let job = |status: &str| json!({"id":99,"run_id":44,"status":status,"pool":"linux","repository":"fixture/private","repository_id":101,"runner_id":501,"runner_name":name,"error":"Controller restarted; inspect runner and worker receipts before recovery. No job replayed."});
        // Still running on GitHub: left alone and retried later.
        g.data
            .lock()
            .unwrap()
            .jobs
            .insert("2-99".into(), job("uncertain"));
        let (api, server) = crate::tests::api_fixture(vec![
            (
                "/app/installations/2/access_tokens",
                json!({"token":"fixture-installation-token"}),
            ),
            (
                "/repos/fixture/private/actions/jobs/99",
                json!({"id":99,"status":"in_progress"}),
            ),
        ]);
        g.api = api;
        let config = g.config.clone().unwrap();
        g.reconcile_uncertain(&config);
        server.join().unwrap();
        {
            let data = g.data.lock().unwrap();
            assert_eq!(data.jobs["2-99"]["status"], "uncertain");
            assert!(data.jobs["2-99"]["reconcile_after"].as_u64().unwrap() > api::now());
        }
        // Over, runner idle: the runner and the VM are removed, the job finished.
        g.data
            .lock()
            .unwrap()
            .jobs
            .insert("2-99".into(), job("uncertain"));
        let (api, server) = crate::tests::api_fixture(vec![
            (
                "/app/installations/2/access_tokens",
                json!({"token":"fixture-installation-token"}),
            ),
            (
                "/repos/fixture/private/actions/jobs/99",
                json!({"id":99,"status":"completed"}),
            ),
            (
                "GET /orgs/fixture/actions/runners/501",
                json!({"name":name,"busy":false}),
            ),
            ("DELETE /orgs/fixture/actions/runners/501", json!({})),
        ]);
        g.api = api;
        g.reconcile_uncertain(&config);
        server.join().unwrap();
        let data = g.data.lock().unwrap();
        assert_eq!(data.jobs["2-99"]["status"], "finished");
        assert_eq!(data.jobs["2-99"]["reclaimed"], true);
        assert!(data.jobs["2-99"]["error"].is_null());
    }
    #[test]
    fn a_handed_off_service_stops_writing() {
        let (_root, g) = crate::tests::shared_service();
        g.hand_off();
        assert!(g.handed_off.load(Ordering::SeqCst));
    }
    #[cfg(unix)]
    #[test]
    fn standby_jit_is_scoped_and_cleaned_without_a_fictitious_github_job() {
        for prepared in [false, true] {
            jit_fixture(prepared, true, false, true);
            jit_fixture(prepared, true, true, true);
        }
    }
    #[cfg(unix)]
    fn jit_fixture(prepared: bool, shared: bool, capacity_wait: bool, reserve: bool) {
        use crate::tests::{account, api_fixture, service, shared_service};
        use std::{
            fs,
            os::unix::fs::PermissionsExt,
            process::{Command, Stdio},
        };
        let (root, mut g) = if shared { shared_service() } else { service() };
        account(&g);
        let key = Command::new("openssl")
            .args(["genrsa", "2048"])
            .stderr(Stdio::null())
            .output()
            .unwrap();
        assert!(key.status.success());
        let config = g.config.as_mut().unwrap();
        config
            .app
            .as_mut()
            .or(config.runners.as_mut())
            .unwrap()
            .private_key = String::from_utf8(key.stdout).unwrap();
        let worker = root.path().join("fixture-worker");
        fs::write(&worker,r#"#!/usr/bin/env python3
import json,sys
waiting=CAPACITY_WAIT
r=json.load(sys.stdin)
assert r['jitconfig']=='fixture-jit-config'
assert set(r)=={'schema','name','job_id','repository','jitconfig','timeout_seconds'} | ({'preparation'} if 'preparation' in r else set()) | ({'reserve','idle_timeout_seconds'} if r.get('reserve') else set())
if r.get('reserve'): assert r['job_id']==0 and r['idle_timeout_seconds']==600
a=None
if 'preparation' in r:
 assert r['preparation']['verification_token']=='fixture-verification-token'
 if not waiting: a={'verified':True,'job_id':1000,'run_id':55,'run_attempt':2,'runner_id':501,'repository_id':101}
print(json.dumps({'schema':1,'name':r['name'],'job_id':r['job_id'],'cleaned':True,'runner_version':'2.337.0','duration_seconds':1,'private_extra':'must-not-expose','assignment':a,'status':'waiting_capacity' if waiting else 'finished','allocation_started':not waiting}))
"#.replace("CAPACITY_WAIT", if capacity_wait { "True" } else { "False" })).unwrap();
        fs::set_permissions(&worker, fs::Permissions::from_mode(0o700)).unwrap();
        g.config.as_mut().unwrap().pools[0].worker = vec![worker.to_str().unwrap().into()];
        {
            let mut data = g.data.lock().unwrap();
            if !shared {
                let repo_account = data.accounts["repos"].clone();
                data.accounts.insert("runners".into(), repo_account);
            }
            data.repositories.insert(
                101,
                crate::Repository {
                    id: 101,
                    installation_id: if shared { 2 } else { 4 },
                    full_name: "fixture/private".into(),
                    private: true,
                    branch: "main".into(),
                },
            );
            data.jobs.insert("2-99".into(),json!({"id":99,"repository":"fixture/private","repository_id":101,"run_id":44,"status":"running"}));
        }
        if reserve {
            g.data.lock().unwrap().jobs.get_mut("2-99").unwrap()["reserve"] = json!(true);
            g.data.lock().unwrap().jobs.get_mut("2-99").unwrap()["id"] = json!(0);
            g.config.as_mut().unwrap().pools[0].standby = Some(Standby {
                minimum: 1,
                repository_id: 101,
                workflows: vec!["fixture/private/.github/workflows/ci.yml@refs/heads/main".into()],
                idle_timeout_seconds: 600,
            });
        }
        let mut responses = vec![
            (
                "/user/installations?",
                json!({"installations":[{"id":2,"account":{"login":"fixture"},"permissions":{"organization_self_hosted_runners":"write"},"suspended_at":null}]}),
            ),
            (
                "/app/installations/2/access_tokens",
                json!({"token":"fixture-installation-token"}),
            ),
            (
                if shared {
                    "/user/installations/2/repositories?"
                } else {
                    "/user/installations/4/repositories?"
                },
                json!({"repositories":[{"id":101}]}),
            ),
            (
                "/repos/fixture/private/actions/jobs/99",
                json!({"id":99,"run_id":44,"status":"queued"}),
            ),
            (
                "generate-jitconfig",
                json!({"runner":{"id":501},"encoded_jit_config":"fixture-jit-config","expected_work_folder":if prepared {"/project/work"} else {"_work"}}),
            ),
            (
                "/app/installations/2/access_tokens",
                json!({"token":"fixture-cleanup-token"}),
            ),
            (
                "GET /orgs/fixture/actions/runners/501",
                json!({"name":"fixture-created-runner","busy":false}),
            ),
            ("DELETE /orgs/fixture/actions/runners/501", json!({})),
            (
                "/repos/fixture/private/actions/jobs/99",
                json!({"id":99,"run_id":44,"status":"queued"}),
            ),
        ];
        if prepared {
            let workflows =
                vec!["fixture/private/.github/workflows/ci.yml@refs/heads/main".to_owned()];
            g.config.as_mut().unwrap().pools[0].preparation = Some(Preparation {
                repository_id: 101,
                workflows: workflows.clone(),
            });
            responses.splice(4..4, vec![("GET /orgs/fixture/actions/runner-groups/1 ", json!({"visibility":"selected","allows_public_repositories":false,"restricted_to_workflows":true,"selected_workflows":workflows})), ("/runner-groups/1/repositories?",json!({"repositories":[{"id":101}]})), ("/app/installations/2/access_tokens",json!({"token":"fixture-verification-token"}))]);
        }
        if reserve {
            responses.retain(|(path, _)| !path.contains("actions/jobs/99"));
            let workflows = &g.config.as_ref().unwrap().pools[0]
                .standby
                .as_ref()
                .unwrap()
                .workflows;
            responses.splice(3..3, vec![("GET /orgs/fixture/actions/runner-groups/1 ", json!({"visibility":"selected","allows_public_repositories":false,"restricted_to_workflows":true,"selected_workflows":workflows})), ("/runner-groups/1/repositories?",json!({"repositories":[{"id":101}]}))]);
        }
        let (api, server) = api_fixture(responses);
        g.api = api;
        let result = g
            .run_job(
                "2-99",
                &g.config.as_ref().unwrap().pools[0],
                &AtomicBool::new(true),
            )
            .unwrap();
        server.join().unwrap();
        assert_eq!(result["cleaned"], true);
        assert_eq!(result["runner_id"], 501);
        assert_eq!(result["demand_pending"], !reserve);
        assert_eq!(result["prepared"], prepared);
        assert_eq!(result["capacity_wait"], capacity_wait);
        if prepared && !capacity_wait {
            assert_eq!(result["actual_job_id"], 1000);
        }
        let mut demand = json!({"status":"running","github_status":"queued","reserve":reserve});
        finish_capacity(&mut demand, result.clone());
        assert_eq!(
            demand["status"],
            if !reserve || capacity_wait {
                "queued"
            } else {
                "finished"
            }
        );
        if capacity_wait {
            assert!(demand["capacity_retry_at"].as_u64().unwrap() > api::now());
            demand["pool"] = json!(g.config.as_ref().unwrap().pools[0].id);
            g.data
                .lock()
                .unwrap()
                .jobs
                .insert("2-99".into(), demand.clone());
            g.tick().unwrap();
            assert_eq!(g.pending(), 0);
            assert_eq!(g.data.lock().unwrap().jobs["2-99"]["status"], "queued");
        }
        demand["reserve"] = json!(false);
        demand["github_status"] = json!("completed");
        finish_capacity(&mut demand, result.clone());
        assert_eq!(demand["status"], "finished");
        assert!(demand["capacity_retry_at"].is_null());
        if !capacity_wait {
            let data = g.data.lock().unwrap();
            let timings = &data.jobs["2-99"]["timings"];
            let values = [
                "jit_requested_at_ms",
                "jit_received_at_ms",
                "worker_launch_requested_at_ms",
                "worker_launched_at_ms",
            ]
            .map(|key| timings[key].as_u64().unwrap());
            assert!(values.windows(2).all(|pair| pair[0] <= pair[1]));
        }
        for secret in [
            "fixture-jit-config",
            "fixture-installation-token",
            "fixture-verification-token",
            "must-not-expose",
        ] {
            assert!(!result.to_string().contains(secret));
            assert!(
                !fs::read_to_string(g.root.join("state.json"))
                    .unwrap()
                    .contains(secret)
            );
        }
    }
    #[test]
    fn signature_is_raw_body_bound() {
        let key = hmac::Key::new(hmac::HMAC_SHA256, b"fixture");
        let signed = hmac::sign(&key, b"{\"ok\":true}");
        let signature_value = format!(
            "sha256={}",
            signed
                .as_ref()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        signature("fixture", &signature_value, b"{\"ok\":true}").unwrap();
        assert!(signature("fixture", &signature_value, b"{ \"ok\":true}").is_err());
        assert!(signature("fixture", &format!("sha256={}", "é".repeat(32)), b"{}").is_err());
    }
    #[test]
    fn explicit_pool_label_and_every_requested_label_are_required() {
        let pool = Pool {
            standby: None,
            id: "linux".into(),
            installation_id: 1,
            organization: "fixture".into(),
            runner_group_id: 1,
            labels: vec![
                "self-hosted".into(),
                "Linux".into(),
                "X64".into(),
                "enginebox-linux".into(),
            ],
            max_runners: 1,
            timeout_seconds: 300,
            worker: vec![
                std::env::current_exe()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            ],
            preparation: None,
        };
        validate_pools(std::slice::from_ref(&pool)).unwrap();
        let mut payload = json!({"installation":{"id":1},"repository":{"owner":{"login":"fixture"}},"workflow_job":{"labels":["self-hosted","linux","enginebox-linux"]}});
        assert!(matches(&pool, &payload));
        payload["workflow_job"]["labels"] = json!(["self-hosted"]);
        assert!(!matches(&pool, &payload));
        payload["workflow_job"]["labels"] = json!(["enginebox-linux", "gpu"]);
        assert!(!matches(&pool, &payload));
    }
}
