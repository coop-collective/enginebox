//! Running repository automations (docs/PROJECT-CONFIG.md, "Automations"):
//! reading `.enginebox/automations/*.yml` from each repository's default
//! branch (at activation, on a GitHub push webhook and by polling), firing
//! schedules and push triggers once each, starting runs through the ordinary
//! pool run path, settling them and serving their history.
//!
//! Everything that touches a repository's record holds [`Engine::lock`], so
//! the background sweep and "run now" never interleave.

use crate::automations::{self, Concurrency, Record, Run, Trigger};
use crate::{Principal, RunTarget, Runtime, blocking, repo_config, repo_source, start_run};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use enginebox_agents::repository::Credential;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// How often a repository with push triggers is polled for a new head.
const PUSH_POLL: Duration = Duration::from_secs(60);
/// How often any other repository is polled (to discover new files).
const IDLE_POLL: Duration = Duration::from_secs(300);
/// Most repositories read per sweep.
const READS_PER_SWEEP: usize = 25;

/// Reads repositories (git in production, a stand-in in tests).
pub trait Source: Send + Sync {
    fn read(
        &self,
        credential: &Credential,
        known: Option<&str>,
    ) -> Result<Option<repo_source::Automations>>;
    fn branch_exists(&self, credential: &Credential, branch: &str) -> Result<bool>;
}
pub struct GitSource;
impl Source for GitSource {
    fn read(
        &self,
        credential: &Credential,
        known: Option<&str>,
    ) -> Result<Option<repo_source::Automations>> {
        repo_source::read_automations(credential, known)
    }
    fn branch_exists(&self, credential: &Credential, branch: &str) -> Result<bool> {
        repo_source::branch_exists(credential, branch)
    }
}

/// A push the GitHub webhook reported, handled at the next sweep.
#[derive(Clone, Debug)]
pub struct PushEvent {
    pub repository: String,
    pub branch: String,
    pub commit: String,
    pub paths: Option<Vec<String>>,
}

#[derive(Clone)]
pub struct Engine {
    pub store: automations::Store,
    pub lock: Arc<tokio::sync::Mutex<()>>,
    source: Arc<dyn Source>,
    /// When each repository was last read.
    checked: Arc<Mutex<BTreeMap<String, Instant>>>,
    pushes: Arc<Mutex<Vec<PushEvent>>>,
    /// Deployment tier map: `{provider: {tier: model}}` (ENGINEBOX_AUTOMATION_MODELS).
    tiers: Value,
    /// Whether schedules and push triggers fire (ENGINEBOX_AUTOMATIONS=off stops them).
    pub enabled: bool,
}

impl Engine {
    pub fn new(root: &std::path::Path, source: Option<Arc<dyn Source>>) -> Self {
        let tiers = match std::env::var("ENGINEBOX_AUTOMATION_MODELS") {
            Ok(text) => serde_json::from_str::<Value>(&text)
                .ok()
                .filter(Value::is_object)
                .unwrap_or_else(|| {
                    eprintln!(
                        "{}",
                        json!({"event":"enginebox.automations.models_invalid"})
                    );
                    Value::Null
                }),
            Err(_) => Value::Null,
        };
        Self {
            store: automations::Store::open(root),
            lock: Arc::new(tokio::sync::Mutex::new(())),
            source: source.unwrap_or_else(|| Arc::new(GitSource)),
            checked: Arc::default(),
            pushes: Arc::default(),
            tiers,
            enabled: std::env::var("ENGINEBOX_AUTOMATIONS").as_deref() != Ok("off"),
        }
    }

    /// Queue a webhook push for the next sweep and read that repository then.
    pub fn nudge(&self, event: PushEvent) {
        self.checked.lock().unwrap().remove(&event.repository);
        let mut pushes = self.pushes.lock().unwrap();
        if pushes.len() < 256 {
            pushes.push(event);
        }
    }
}

/// A repository as the sweep needs it.
#[derive(Clone, Debug)]
struct Repo {
    reference: String,
    org: String,
    default_branch: String,
}

fn repos(runtime: &Runtime) -> Vec<Repo> {
    runtime.repositories.list_all()["repositories"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            Some(Repo {
                reference: repo_config::repository_ref(r["project"].as_str()?, r["name"].as_str()?),
                org: r["org_id"].as_str()?.to_owned(),
                default_branch: r["default_branch"].as_str()?.to_owned(),
            })
        })
        .collect()
}

fn repo(runtime: &Runtime, org: &str, project: &str, name: &str) -> Option<Repo> {
    let reference = repo_config::repository_ref(project, name);
    repos(runtime)
        .into_iter()
        .find(|r| r.reference == reference && r.org == org)
}

/// The GitHub repository (`owner/name`) a webhook names, as a repository ref.
pub fn github_repository(runtime: &Runtime, full_name: &str) -> Option<String> {
    let remote = format!("https://github.com/{full_name}.git");
    runtime.repositories.list_all()["repositories"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|r| r["backend"] == "github" && r["remote"] == remote.as_str())
        .and_then(|r| {
            Some(repo_config::repository_ref(
                r["project"].as_str()?,
                r["name"].as_str()?,
            ))
        })
}

/// A GitHub push payload as `(owner/name, event)` (the event's repository is
/// filled in by the caller). GitHub lists at most 20 commits; with 20 or more
/// (or none) the changed paths count as unknown. Deleted branches are ignored.
pub fn github_push(payload: &Value) -> Option<(String, PushEvent)> {
    let full_name = payload["repository"]["full_name"].as_str()?;
    let branch = payload["ref"].as_str()?.strip_prefix("refs/heads/")?;
    let commit = payload["after"].as_str()?;
    if payload["deleted"] == true
        || !enginebox_agents::repository::valid_branch(branch)
        || !(40..=64).contains(&commit.len())
        || !commit.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    let commits = payload["commits"].as_array().cloned().unwrap_or_default();
    let paths = (!commits.is_empty() && commits.len() < 20).then(|| {
        let mut paths: Vec<String> = commits
            .iter()
            .flat_map(|c| {
                ["added", "modified", "removed"]
                    .into_iter()
                    .flat_map(move |k| c[k].as_array().cloned().unwrap_or_default())
            })
            .filter_map(|p| p.as_str().map(str::to_owned))
            .collect();
        paths.sort();
        paths.dedup();
        paths
    });
    Some((
        full_name.to_owned(),
        PushEvent {
            repository: String::new(),
            branch: branch.to_owned(),
            commit: commit.to_owned(),
            paths,
        },
    ))
}

/// Read a repository's files into `record` when its head moved. Returns the
/// push trigger input (branch, commit, changed paths) when it moved after an
/// earlier read; the first read never fires push triggers.
async fn refresh(
    runtime: &Runtime,
    engine: &Engine,
    repo: &Repo,
    record: &mut Record,
    now: DateTime<Utc>,
) -> Option<(String, Option<Vec<String>>)> {
    engine
        .checked
        .lock()
        .unwrap()
        .insert(repo.reference.clone(), Instant::now());
    let known = record.commit.clone();
    let (repositories, source, reference) = (
        runtime.repositories.clone(),
        engine.source.clone(),
        repo.reference.clone(),
    );
    let read = blocking(move || {
        let credential = repositories.read_credential(&reference, 300)?;
        source.read(&credential, known.as_deref())
    })
    .await;
    match read {
        Ok(None) => None,
        Ok(Some(head)) => {
            let fired = record
                .commit
                .is_some()
                .then(|| (head.commit.clone(), head.changed.clone()));
            record.set_files(&head.commit, head.files, now);
            fired
        }
        Err(error) => {
            let reason: String = format!("{error:#}").chars().take(200).collect();
            eprintln!(
                "{}",
                json!({"event":"enginebox.automations.read_failed","repository":repo.reference,"reason":reason})
            );
            record.read_error = Some(reason);
            None
        }
    }
}

fn new_record(repo: &Repo) -> Record {
    Record {
        repository: repo.reference.clone(),
        org_id: repo.org.clone(),
        ..Default::default()
    }
}

/// Fire one automation: dedupe by firing key, apply concurrency, start the run.
async fn fire(
    runtime: &Runtime,
    engine: &Engine,
    repo: &Repo,
    record: &mut Record,
    id: &str,
    trigger: Trigger,
    now: DateTime<Utc>,
) -> (Run, bool) {
    let key = automations::firing_key(&repo.reference, id, &trigger);
    if let Some(existing) = record.runs.iter().find(|r| r.id == key) {
        return (existing.clone(), true);
    }
    let mut run = Run::new(key, id, trigger, now);
    run.commit = record.commit.clone();
    let Some(automation) = record.automation(id).cloned() else {
        run.status = "refused".into();
        run.error = Some("The automation is not defined on the default branch.".into());
        record.runs.push(run.clone());
        return (run, false);
    };
    if !automation.enabled {
        run.status = "skipped".into();
        run.error = Some("The automation is disabled (enabled: false).".into());
    } else if record
        .runs
        .iter()
        .any(|r| r.automation == id && r.active() && r.status != "pending")
    {
        match automation.concurrency {
            Concurrency::Skip => {
                run.status = "skipped".into();
                run.error = Some("A run of this automation was still active.".into());
            }
            Concurrency::Queue => {
                for older in record
                    .runs
                    .iter_mut()
                    .filter(|r| r.automation == id && r.status == "pending")
                {
                    older.status = "skipped".into();
                    older.error = Some("Replaced by a newer firing.".into());
                }
                run.status = "pending".into();
            }
        }
    } else {
        start(runtime, engine, repo, record, &automation, &mut run, now).await;
    }
    record.runs.push(run.clone());
    record.prune();
    (run, false)
}

/// Start `run` in the repository's project pool, trying the automation's
/// runners in order until one is accepted.
async fn start(
    runtime: &Runtime,
    engine: &Engine,
    repo: &Repo,
    record: &Record,
    automation: &automations::Automation,
    run: &mut Run,
    now: DateTime<Utc>,
) {
    let refuse = |run: &mut Run, message: String| {
        run.status = "refused".into();
        run.error = Some(message.chars().take(300).collect());
    };
    let branch = match &automation.branch_prefix {
        Some(prefix) if automations::prefix_covers_default(prefix, &repo.default_branch) => {
            return refuse(
                run,
                format!(
                    "push.branch-prefix {prefix} could name the default branch {}.",
                    repo.default_branch
                ),
            );
        }
        Some(prefix) => {
            let taken: Vec<&str> = record
                .runs
                .iter()
                .filter_map(|r| r.branch.as_deref())
                .collect();
            Some(automations::branch_for(prefix, now, &taken))
        }
        None => None,
    };
    let Ok((project, _)) = enginebox_agents::repository::parse_ref(&repo.reference) else {
        return refuse(run, "Invalid repository.".into());
    };
    let Some(pool) = runtime
        .agents
        .pools()
        .in_project(&project)
        .into_iter()
        .find(|p| p.org() == repo.org)
        .map(|p| p.name)
    else {
        return refuse(
            run,
            "The repository's project has no pool to run in.".into(),
        );
    };
    let commit = match &run.trigger {
        Trigger::Push { branch, commit, .. } if *branch == repo.default_branch => {
            Some(commit.clone())
        }
        _ => record.commit.clone(),
    };
    let prompt = automations::prompt(
        automation,
        &repo.reference,
        &repo.default_branch,
        commit.as_deref(),
        &run.trigger,
        branch.as_deref(),
    );
    let mut last_error = String::from("No runner accepted the run.");
    for provider in &automation.runners {
        let model = automation.model_for(provider, &engine.tiers);
        let mut request = json!({
            "provider": provider,
            "prompt": prompt,
            "workdir": ".",
            "timeout_seconds": automation.timeout_minutes * 60,
            "repository": repo.reference,
            "run_kind": automations::RUN_KIND,
            "idempotency_key": automations::run_idempotency_key(&automation.id, &run.id, provider),
        });
        if let Some(m) = &model {
            request["model"] = json!(m);
        }
        if let Some(p) = &automation.profile {
            request["repo_profile"] = json!(p);
        }
        match start_run(
            runtime,
            RunTarget::Pool(pool.clone()),
            request,
            &Principal::Owner,
            &repo.org,
        )
        .await
        {
            Ok((status, value)) if status.is_success() => {
                run.run_id = value["run"]["id"].as_str().map(str::to_owned);
                run.status = "queued".into();
                run.pool = Some(pool);
                run.provider = Some(provider.clone());
                run.model = model;
                run.branch = branch;
                return;
            }
            Ok((_, value)) => {
                last_error = format!(
                    "{provider}: {}",
                    value["error"].as_str().unwrap_or("the run did not start")
                );
            }
            Err(error) => last_error = format!("{provider}: {error:#}"),
        }
    }
    refuse(run, last_error);
}

/// Settle active runs from their native records, check pushed branches, and
/// start a waiting firing once its automation is free.
async fn settle(
    runtime: &Runtime,
    engine: &Engine,
    repo: &Repo,
    record: &mut Record,
    now: DateTime<Utc>,
) {
    for i in 0..record.runs.len() {
        let r = &record.runs[i];
        let (Some(pool), Some(run_id)) = (r.pool.clone(), r.run_id.clone()) else {
            continue;
        };
        if !matches!(r.status.as_str(), "queued" | "running" | "starting") {
            continue;
        }
        let native = match runtime.agents.get_pool_run(&pool, &run_id) {
            Ok(v) => v,
            Err(_) => {
                json!({"status":"failed","failure":{"reason":"The run is no longer known to the controller."}})
            }
        };
        record.runs[i].settle(&native);
        let r = &record.runs[i];
        if !r.active()
            && let Some(branch) = r.branch.clone()
        {
            let (repositories, source, reference) = (
                runtime.repositories.clone(),
                engine.source.clone(),
                repo.reference.clone(),
            );
            let exists = blocking(move || {
                let credential = repositories.read_credential(&reference, 300)?;
                source.branch_exists(&credential, &branch)
            })
            .await;
            record.runs[i].branch_pushed = exists.ok();
        }
    }
    let waiting: Vec<usize> = (0..record.runs.len())
        .filter(|&i| record.runs[i].status == "pending")
        .collect();
    for i in waiting {
        let id = record.runs[i].automation.clone();
        let busy = record
            .runs
            .iter()
            .any(|r| r.automation == id && r.active() && r.status != "pending");
        if busy {
            continue;
        }
        let Some(automation) = record.automation(&id).cloned() else {
            record.runs[i].status = "skipped".into();
            record.runs[i].error = Some("The automation was removed.".into());
            continue;
        };
        let mut run = record.runs[i].clone();
        run.commit = record.commit.clone();
        start(runtime, engine, repo, record, &automation, &mut run, now).await;
        record.runs[i] = run;
    }
}

fn save(engine: &Engine, record: &Record) {
    if let Err(error) = engine.store.put(record) {
        eprintln!(
            "{}",
            json!({"event":"enginebox.automations.save_failed","repository":record.repository,"reason":error.to_string()})
        );
    }
}

/// One sweep over every repository: read moved heads, fire due schedules and
/// pushes, settle runs.
pub async fn sweep(runtime: &Runtime, engine: &Engine, now: DateTime<Utc>) {
    let _guard = engine.lock.lock().await;
    let pushes: Vec<PushEvent> = std::mem::take(&mut *engine.pushes.lock().unwrap());
    let mut reads = 0;
    for repo in repos(runtime) {
        let stored = engine
            .store
            .get(&repo.reference)
            .filter(|r| r.org_id == repo.org);
        let has_push = stored
            .as_ref()
            .is_some_and(|r| r.automations().any(|a| a.push_trigger.is_some()));
        let due_read = engine
            .checked
            .lock()
            .unwrap()
            .get(&repo.reference)
            .is_none_or(|at| at.elapsed() >= if has_push { PUSH_POLL } else { IDLE_POLL });
        let repo_pushes: Vec<&PushEvent> = pushes
            .iter()
            .filter(|p| p.repository == repo.reference)
            .collect();
        if stored.is_none() && !due_read {
            continue;
        }
        let mut record = stored.clone().unwrap_or_else(|| new_record(&repo));
        let mut firings: Vec<(String, Trigger)> = Vec::new();
        if due_read && reads < READS_PER_SWEEP {
            reads += 1;
            if let Some((commit, changed)) = refresh(runtime, engine, &repo, &mut record, now).await
            {
                for a in record.automations() {
                    if a.push_fires(
                        &repo.default_branch,
                        &repo.default_branch,
                        changed.as_deref(),
                    ) {
                        firings.push((
                            a.id.clone(),
                            Trigger::push(&repo.default_branch, &commit, changed.as_deref()),
                        ));
                    }
                }
            }
        }
        if engine.enabled {
            // Pushes to other branches come only from the webhook.
            for p in repo_pushes
                .iter()
                .filter(|p| p.branch != repo.default_branch)
            {
                for a in record.automations() {
                    if a.push_fires(&repo.default_branch, &p.branch, p.paths.as_deref()) {
                        firings.push((
                            a.id.clone(),
                            Trigger::push(&p.branch, &p.commit, p.paths.as_deref()),
                        ));
                    }
                }
            }
            firings.extend(record.due(now));
        } else {
            firings.clear();
        }
        for (id, trigger) in firings {
            fire(runtime, engine, &repo, &mut record, &id, trigger, now).await;
        }
        settle(runtime, engine, &repo, &mut record, now).await;
        // A record (even without files) keeps the head, so an unchanged head
        // costs one ls-remote; a repository never read keeps none.
        if stored.as_ref() != Some(&record) && (stored.is_some() || record.commit.is_some()) {
            save(engine, &record);
        }
    }
}

/// `GET …/automations`: the files as last read, with each one's last run.
pub fn list(
    runtime: &Runtime,
    engine: &Engine,
    org: &str,
    project: &str,
    name: &str,
) -> Result<Value> {
    let repo = repo(runtime, org, project, name).context("Unknown repository")?;
    let record = engine
        .store
        .get(&repo.reference)
        .filter(|r| r.org_id == org)
        .unwrap_or_else(|| new_record(&repo));
    let mut v = record.public(Utc::now());
    v["enabled"] = json!(engine.enabled);
    Ok(v)
}

/// `GET …/automations/{id}`: one automation and its recent runs, newest first.
pub fn get(
    runtime: &Runtime,
    engine: &Engine,
    org: &str,
    project: &str,
    name: &str,
    id: &str,
) -> Result<Value> {
    let repo = repo(runtime, org, project, name).context("Unknown repository")?;
    let record = engine
        .store
        .get(&repo.reference)
        .filter(|r| r.org_id == org)
        .context("Unknown automation")?;
    let public = record.public(Utc::now());
    let automation = public["automations"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["id"] == id)
        .cloned()
        .context("Unknown automation")?;
    let runs: Vec<&Run> = record
        .runs
        .iter()
        .rev()
        .filter(|r| r.automation == id)
        .take(50)
        .collect();
    Ok(
        json!({"repository": repo.reference, "commit": record.commit, "automation": automation, "runs": runs}),
    )
}

/// `GET …/automation-runs`: every recorded run of the repository, newest first.
pub fn runs(
    runtime: &Runtime,
    engine: &Engine,
    org: &str,
    project: &str,
    name: &str,
) -> Result<Value> {
    let repo = repo(runtime, org, project, name).context("Unknown repository")?;
    let record = engine
        .store
        .get(&repo.reference)
        .filter(|r| r.org_id == org);
    let runs: Vec<Run> = record
        .map(|r| r.runs.into_iter().rev().collect())
        .unwrap_or_default();
    Ok(json!({"repository": repo.reference, "runs": runs}))
}

/// `POST …/automations/refresh {}` and `POST …/automations/{id}/run {key?}`.
/// Run now re-reads the default branch first, so a just-merged file applies.
pub async fn write(
    runtime: &Runtime,
    org: &str,
    actor: &str,
    project: &str,
    name: &str,
    id: Option<&str>,
    body: &Value,
) -> (axum::http::StatusCode, Value) {
    use axum::http::StatusCode;
    let engine = &runtime.automations;
    let Some(repo) = repo(runtime, org, project, name) else {
        return (
            StatusCode::NOT_FOUND,
            json!({"error":"Unknown repository."}),
        );
    };
    let allowed: &[&str] = if id.is_some() { &["key"] } else { &[] };
    let object = body.as_object();
    if object.is_none_or(|o| o.keys().any(|k| !allowed.contains(&k.as_str()))) {
        let message = if id.is_some() {
            "The request accepts key (an idempotency key)."
        } else {
            "The request takes no fields."
        };
        return (StatusCode::BAD_REQUEST, json!({"error": message}));
    }
    let key = match body.get("key") {
        None | Some(Value::Null) => None,
        Some(Value::String(k)) if enginebox_agents::service::valid_idempotency_key(k) => {
            Some(k.clone())
        }
        Some(_) => {
            return (
                StatusCode::BAD_REQUEST,
                json!({"error":"key must be 1-128 characters of A-Z a-z 0-9 . _ : -"}),
            );
        }
    };
    let now = Utc::now();
    let _guard = engine.lock.lock().await;
    let stored = engine
        .store
        .get(&repo.reference)
        .filter(|r| r.org_id == org);
    let mut record = stored.clone().unwrap_or_else(|| new_record(&repo));
    let known = record.commit.clone();
    refresh(runtime, engine, &repo, &mut record, now).await;
    if known.is_some() && record.commit != known {
        // Push triggers fire only from the sweep: keep the old head so the
        // next sweep sees the move (and reads at once) and fires them.
        record.commit = known;
        engine.checked.lock().unwrap().remove(&repo.reference);
    }
    let reply = match id {
        None => (StatusCode::OK, record.public(now)),
        Some(id) => match record.automation(id) {
            None => (
                StatusCode::NOT_FOUND,
                json!({"error":"Unknown automation."}),
            ),
            Some(a) if !a.manual => (
                StatusCode::CONFLICT,
                json!({"error":"This automation turns off manual runs (on.manual: false)."}),
            ),
            Some(_) => {
                let trigger = Trigger::Manual {
                    by: actor.to_owned(),
                    key,
                };
                let (run, replayed) =
                    fire(runtime, engine, &repo, &mut record, id, trigger, now).await;
                let status = if replayed {
                    StatusCode::OK
                } else if run.status == "refused" {
                    StatusCode::CONFLICT
                } else {
                    StatusCode::ACCEPTED
                };
                (status, json!({"run": run, "replayed": replayed}))
            }
        },
    };
    if stored.as_ref() != Some(&record) && (stored.is_some() || record.commit.is_some()) {
        save(engine, &record);
    }
    reply
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_pushes_carry_their_changed_paths_when_github_listed_them_all() {
        let sha = "a".repeat(40);
        let payload = json!({
            "ref": "refs/heads/main",
            "after": sha,
            "repository": {"full_name": "o/r"},
            "commits": [
                {"added": ["docs/new.md"], "modified": ["src/a.rs"], "removed": []},
                {"added": [], "modified": ["docs/new.md"], "removed": ["old.txt"]},
            ],
        });
        let (full_name, event) = github_push(&payload).unwrap();
        assert_eq!((full_name.as_str(), event.branch.as_str()), ("o/r", "main"));
        assert_eq!(
            event.paths,
            Some(vec![
                "docs/new.md".into(),
                "old.txt".into(),
                "src/a.rs".into()
            ])
        );
        let mut many = payload.clone();
        many["commits"] = json!(vec![json!({"added": ["x"]}); 20]);
        assert_eq!(github_push(&many).unwrap().1.paths, None);
        let mut deleted = payload.clone();
        deleted["deleted"] = json!(true);
        assert!(github_push(&deleted).is_none());
        let mut tag = payload.clone();
        tag["ref"] = json!("refs/tags/v1");
        assert!(github_push(&tag).is_none());
        let mut bad = payload;
        bad["after"] = json!("not-a-sha");
        assert!(github_push(&bad).is_none());
    }
}
