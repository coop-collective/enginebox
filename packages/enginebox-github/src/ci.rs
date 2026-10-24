//! GitHub remains the scheduler. Webhooks only request bounded runner capacity.
use crate::{GitHub, api, full_name, name, number, text};
use anyhow::{Context, Result, ensure};
use enginebox_core::transport::run_process;
use ring::hmac;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    path::Path,
    sync::atomic::Ordering,
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
        signature(&self.app(kind)?.webhook_secret, supplied, body)?;
        let payload: Value = serde_json::from_slice(body)?;
        if kind != "runners" || event != "workflow_job" {
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
        if let Some(job) = data.jobs.get_mut(&key) {
            ensure!(job["repository_id"] == repo_id, "Job repository mismatch");
            // Events can arrive out of order; terminal and running states never requeue.
            if action == "completed" {
                job["github_status"] = json!("completed");
                job["conclusion"] = payload["workflow_job"]["conclusion"].clone();
                if job["status"] == "queued" {
                    job["status"] = json!("cancelled");
                }
            } else if action == "in_progress" {
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
            data.jobs.insert(key,json!({"id":id,"pool":pool.id,"repository_id":repo_id,"repository":selected.full_name,"run_id":payload["workflow_job"]["run_id"],"status":if action=="queued" {"queued"} else {"claimed_elsewhere"},"github_status":action,"created_at":api::now()}));
        }
        self.save(&data)?;
        Ok(json!({"accepted":true}))
    }
    pub fn set_draining(&self, draining: bool) -> Result<Value> {
        let mut data = self.data.lock().unwrap();
        data.draining = draining;
        self.save(&data)?;
        Ok(json!({"draining":draining,"active_runners":self.pending()}))
    }
    /// Called only while the controller is active. Running workers are part of its drain barrier.
    pub fn tick(&self) -> Result<()> {
        let Some(config) = &self.config else {
            return Ok(());
        };
        let mut data = self.data.lock().unwrap();
        if data.draining {
            return Ok(());
        }
        for pool in &config.pools {
            let active = data
                .jobs
                .values()
                .filter(|j| {
                    j["pool"] == pool.id
                        && matches!(j["status"].as_str(), Some("running" | "uncertain"))
                })
                .count();
            let keys = data
                .jobs
                .iter()
                .filter(|(_, j)| j["pool"] == pool.id && j["status"] == "queued")
                .take(pool.max_runners.saturating_sub(active))
                .map(|(k, _)| k.clone())
                .collect::<Vec<_>>();
            for key in keys {
                data.jobs.get_mut(&key).unwrap()["status"] = json!("running");
                self.save(&data)?;
                self.pending.fetch_add(1, Ordering::SeqCst);
                let this = self.clone();
                let pool = pool.clone();
                std::thread::spawn(move || {
                    let started = Instant::now();
                    let outcome = this.run_job(&key, &pool);
                    let mut data = this.data.lock().unwrap();
                    let job = data.jobs.get_mut(&key).unwrap();
                    match outcome {
                        Ok(result) => {
                            job["status"] = json!("finished");
                            job["result"] = result;
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
                    this.pending.fetch_sub(1, Ordering::SeqCst);
                });
            }
        }
        Ok(())
    }
    fn run_job(&self, key: &str, pool: &Pool) -> Result<Value> {
        let (job, token) = {
            let mut data = self.data.lock().unwrap();
            let installations = self.installations("runners", &mut data)?;
            ensure!(
                installations.iter().any(|i| i["id"] == pool.installation_id
                    && i["account"]["login"]
                        .as_str()
                        .is_some_and(|s| s.eq_ignore_ascii_case(&pool.organization))
                    && i["suspended_at"].is_null()
                    && i["permissions"]["organization_self_hosted_runners"] == "write"),
                "Approve organization self-hosted runners permission in the runner App installation"
            );
            let token=self.api.call("POST",&format!("/app/installations/{}/access_tokens",pool.installation_id),&api::jwt(self.app("runners")?)?,Some(json!({"permissions":{"organization_self_hosted_runners":"write","actions":"read"}})))?;
            (data.jobs[key].clone(), text(&token, "token")?.to_owned())
        };
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
        let runner_name = format!("ebox-ci-{}", uuid::Uuid::new_v4().simple());
        // JIT is single-use; a lost response is never retried automatically.
        let generated=self.api.call("POST",&format!("/orgs/{}/actions/runners/generate-jitconfig",pool.organization),&token,Some(json!({"name":runner_name,"runner_group_id":pool.runner_group_id,"labels":pool.labels,"work_folder":"_work"})))?;
        let runner_id = number(&generated["runner"], "id")?;
        {
            let mut data = self.data.lock().unwrap();
            let job = data.jobs.get_mut(key).unwrap();
            job["runner_id"] = json!(runner_id);
            job["runner_name"] = json!(runner_name);
            self.save(&data)?;
        }
        let request = json!({"schema":1,"name":runner_name,"job_id":job["id"],"repository":repo,"jitconfig":text(&generated,"encoded_jit_config")?,"timeout_seconds":pool.timeout_seconds});
        let output = run_process(
            &pool.worker,
            Some(&serde_json::to_vec(&request)?),
            Duration::from_secs(pool.timeout_seconds + 600),
        )?;
        let result: Value = serde_json::from_slice(&output)?;
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
        let cleanup_token = self.api.call(
            "POST",
            &format!("/app/installations/{}/access_tokens", pool.installation_id),
            &api::jwt(self.app("runners")?)?,
            Some(json!({"permissions":{"organization_self_hosted_runners":"write"}})),
        )?;
        self.api.cleanup_runner(
            &format!("/orgs/{}/actions/runners/{runner_id}", pool.organization),
            text(&cleanup_token, "token")?,
            &runner_name,
        )?;
        // Do not publish arbitrary worker stdout (could include JIT or workflow secrets).
        Ok(
            json!({"cleaned":true,"runner_id":runner_id,"runner_version":result["runner_version"],"worker_seconds":result["duration_seconds"]}),
        )
    }
}
fn job_persistence_failure(data: &mut crate::Data, key: &str) {
    data.jobs.get_mut(key).unwrap()["status"] = json!("uncertain");
}

#[cfg(test)]
mod tests {
    use super::*;
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
            worker: vec!["/fixture/worker".into()],
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
