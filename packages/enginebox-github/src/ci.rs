//! GitHub remains the scheduler. Webhooks only request bounded runner capacity.
use crate::{GitHub, api, full_name, name, number, text};
use anyhow::{Context, Result, ensure};
use enginebox_core::transport::run_command;
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
    #[serde(default)]
    pub preparation: Option<Preparation>,
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
        if let Some(preparation) = &p.preparation {
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
                "Approve organization self-hosted runners permission in the GitHub App installation"
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
        // JIT is single-use; a lost response is never retried automatically.
        let generated=self.api.call("POST",&format!("/orgs/{}/actions/runners/generate-jitconfig",pool.organization),&token,Some(json!({"name":runner_name,"runner_group_id":pool.runner_group_id,"labels":pool.labels,"work_folder":if pool.preparation.is_some() {"/project/work"} else {"_work"}})))?;
        let runner_id = number(&generated["runner"], "id")?;
        {
            let mut data = self.data.lock().unwrap();
            let job = data.jobs.get_mut(key).unwrap();
            job["runner_id"] = json!(runner_id);
            job["runner_name"] = json!(runner_name);
            self.save(&data)?;
        }
        let mut request = json!({"schema":1,"name":runner_name,"job_id":job["id"],"repository":repo,"jitconfig":text(&generated,"encoded_jit_config")?,"timeout_seconds":pool.timeout_seconds});
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
        let output = run_command(
            command,
            Some(&serde_json::to_vec(&request)?),
            // Cold generation resolution has its own 20-minute bound before VM execution.
            Duration::from_secs(
                pool.timeout_seconds
                    + if pool.preparation.is_some() {
                        1800
                    } else {
                        600
                    },
            ),
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
        if pool.preparation.is_some() {
            let actual = &result["assignment"];
            ensure!(
                actual["verified"] == true
                    && actual["repository_id"] == selected.id
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
        // Do not publish arbitrary worker stdout (could include JIT or workflow secrets).
        Ok(
            json!({"cleaned":true,"runner_id":runner_id,"runner_version":result["runner_version"],"worker_seconds":result["duration_seconds"],"demand_pending":remaining["status"] == "queued", "prepared":pool.preparation.is_some(),"actual_job_id":result["assignment"]["job_id"].as_u64()}),
        )
    }
}
fn finish_capacity(job: &mut Value, result: Value) {
    // A newer webhook wins over the earlier API observation.
    job["status"] = json!(
        if result["demand_pending"] == true && job["github_status"] == "queued" {
            "queued"
        } else {
            "finished"
        }
    );
    job["result"] = result;
}
fn job_persistence_failure(data: &mut crate::Data, key: &str) {
    data.jobs.get_mut(key).unwrap()["status"] = json!("uncertain");
}

#[cfg(test)]
mod tests {
    use super::*;
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
                g.run_job("2-99", &g.config.as_ref().unwrap().pools[0])
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
            jit_fixture(false, shared);
            jit_fixture(true, shared);
        }
    }
    #[cfg(unix)]
    fn jit_fixture(prepared: bool, shared: bool) {
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
r=json.load(sys.stdin)
assert r['jitconfig']=='fixture-jit-config'
assert set(r)=={'schema','name','job_id','repository','jitconfig','timeout_seconds'} | ({'preparation'} if 'preparation' in r else set())
a={}
if 'preparation' in r:
 assert r['preparation']['verification_token']=='fixture-verification-token'
 a={'verified':True,'job_id':1000,'run_id':55,'run_attempt':2,'runner_id':501,'repository_id':101}
print(json.dumps({'schema':1,'name':r['name'],'job_id':r['job_id'],'cleaned':True,'runner_version':'2.337.0','duration_seconds':1,'private_extra':'must-not-expose','assignment':a}))
"#).unwrap();
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
        let (api, server) = api_fixture(responses);
        g.api = api;
        let result = g
            .run_job("2-99", &g.config.as_ref().unwrap().pools[0])
            .unwrap();
        server.join().unwrap();
        assert_eq!(result["cleaned"], true);
        assert_eq!(result["runner_id"], 501);
        assert_eq!(result["demand_pending"], true);
        assert_eq!(result["prepared"], prepared);
        if prepared {
            assert_eq!(result["actual_job_id"], 1000);
        }
        let mut demand = json!({"status":"running","github_status":"queued"});
        finish_capacity(&mut demand, result.clone());
        assert_eq!(demand["status"], "queued");
        demand["github_status"] = json!("completed");
        finish_capacity(&mut demand, result.clone());
        assert_eq!(demand["status"], "finished");
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
