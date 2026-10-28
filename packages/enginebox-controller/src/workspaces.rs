//! Owner-only, read-only projection of durable project identity. Never return
//! task text, process output, paths, credentials, or infer acceptance from a run.
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn rows<'a>(value: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    value[key].as_array().into_iter().flatten()
}
fn operation(job: &Value) -> &str {
    job["operation"]
        .as_str()
        .unwrap_or("")
        .trim_start_matches("storage_")
}
fn fields(value: &Value, keys: &[&str]) -> Value {
    let mut result = json!({});
    for key in keys {
        if let Some(v) = value.get(*key) {
            result[*key] = v.clone();
        }
    }
    result
}
fn command_timings(checkout: &Value) -> Value {
    json!(
        rows(checkout, "command_timings")
            .filter(|r| {
                matches!(
                    r["kind"].as_str(),
                    Some("test" | "build" | "dependencies" | "dev" | "other")
                ) && matches!(
                    r["status"].as_str(),
                    Some("succeeded" | "failed" | "uncertain")
                ) && (r["elapsed_ms"].is_null()
                    || r["elapsed_ms"].as_f64().is_some_and(|n| n >= 0.0))
                    && r["observed_at_unix_ms"].is_u64()
            })
            .take(5)
            .map(|r| fields(r, &["kind", "status", "elapsed_ms", "observed_at_unix_ms"]))
            .collect::<Vec<_>>()
    )
}
/// Drop potentially large process output before cloning a retained job snapshot.
pub(crate) fn job_observation(job: &Value) -> Value {
    let mut view = fields(
        job,
        &[
            "id",
            "host_id",
            "operation",
            "name",
            "project",
            "status",
            "created_at",
            "started_at",
            "finished_at",
            "duration_seconds",
        ],
    );
    let result = &job["result"]["result"];
    let mut observed = fields(result, &["bytes", "files"]);
    observed["workspaces"] = json!(
        rows(result, "workspaces")
            .map(|w| fields(
                w,
                &[
                    "name",
                    "project",
                    "github_project",
                    "status",
                    "source_commit",
                    "last_checkpoint",
                    "command_timings"
                ]
            ))
            .collect::<Vec<_>>()
    );
    observed["github_projects"] = json!(
        rows(result, "github_projects")
            .map(|p| {
                let mut pview = fields(p, &["id", "repository", "branch"]);
                pview["generations"] = json!(
                    rows(p, "generations")
                        .map(|g| fields(g, &["project"]))
                        .collect::<Vec<_>>()
                );
                pview
            })
            .collect::<Vec<_>>()
    );
    observed["main"] = fields(&result["main"], &["revision", "sequence"]);
    observed["sync"] = json!({"enabled":result["sync"]["enabled"],"attempt":{"status":result["sync"]["attempt"]["status"]}});
    let usage = &result["storage_usage"];
    if usage["schema"] == 1 {
        let mut measured = fields(usage, &["schema", "observed_at_unix_ms", "status"]);
        for key in ["projects", "shared_caches"] {
            measured[key] = json!(
                rows(usage, key)
                    .map(|p| fields(
                        p,
                        &[
                            "project",
                            "kind",
                            "status",
                            "apparent_bytes",
                            "allocated_bytes",
                            "files"
                        ]
                    ))
                    .collect::<Vec<_>>()
            );
        }
        observed["storage_usage"] = measured;
    }
    for checkout in observed["workspaces"].as_array_mut().unwrap() {
        checkout["command_timings"] = command_timings(checkout);
        if checkout["last_checkpoint"].is_object() {
            checkout["last_checkpoint"] = fields(&checkout["last_checkpoint"], &["kind", "head"]);
        }
    }
    view["result"] = json!({"result":observed});
    view
}
fn workspace<'a>(all: &'a mut BTreeMap<String, Value>, project: &str) -> Option<&'a mut Value> {
    if !enginebox_agents::pools::valid_project(project) {
        return None;
    }
    Some(all.entry(project.to_owned()).or_insert_with(|| json!({
        "id": project, "repositories":[], "pools":[], "runs":[], "storage_nodes":[],
        "checkouts":[], "operations":[], "launch_events":[], "source_observations":[], "integration_observations":[], "storage_usage":[]
    })))
}
fn push(workspace: &mut Value, key: &str, value: Value) {
    workspace[key].as_array_mut().unwrap().push(value);
}

/// Attribute checkout operations from the newest successful inventory on that
/// host at admission. Unobserved checkouts remain unattributed.
pub fn checkout_project(jobs: &[Value], host: &str, name: &str) -> Option<String> {
    let latest = jobs
        .iter()
        .filter(|j| j["host_id"] == host && operation(j) == "list" && j["status"] == "succeeded")
        .max_by_key(|j| j["created_at"].as_str().unwrap_or(""))?;
    rows(&latest["result"]["result"], "workspaces")
        .find(|w| w["name"] == name)
        .and_then(|w| {
            w["github_project"]
                .as_str()
                .or_else(|| w["project"].as_str())
        })
        .filter(|p| enginebox_agents::pools::valid_project(p))
        .map(str::to_owned)
}

pub fn catalog(
    repositories: &Value,
    pools: &[Value],
    storage: &Value,
    runs: &Value,
    events: &Value,
    jobs: &[Value],
) -> Value {
    let mut all = BTreeMap::new();
    for repository in rows(repositories, "repositories") {
        if let Some(w) = workspace(&mut all, repository["project"].as_str().unwrap_or("")) {
            push(
                w,
                "repositories",
                fields(
                    repository,
                    &[
                        "project",
                        "name",
                        "backend",
                        "remote",
                        "default_branch",
                        "description",
                        "created_at",
                    ],
                ),
            );
        }
    }
    let mut pool_projects = BTreeMap::new();
    for pool in pools {
        let project = pool["project"].as_str().unwrap_or("default");
        pool_projects.insert(pool["name"].as_str().unwrap_or(""), project);
        if let Some(w) = workspace(&mut all, project) {
            push(
                w,
                "pools",
                fields(
                    pool,
                    &[
                        "name",
                        "profile",
                        "template",
                        "members",
                        "member_status",
                        "queued_runs",
                        "running_runs",
                        "creating",
                    ],
                ),
            );
        }
    }
    for node in rows(storage, "nodes") {
        for project in rows(node, "projects") {
            if let Some(w) = workspace(&mut all, project["id"].as_str().unwrap_or("")) {
                push(w, "storage_nodes", node["host_id"].clone());
            }
        }
    }
    // Only the latest successful list on each host describes observed checkouts.
    // Old snapshots must not resurrect a checkout that has since closed.
    let mut latest = BTreeMap::<&str, &Value>::new();
    for job in jobs
        .iter()
        .filter(|j| operation(j) == "list" && j["status"] == "succeeded")
    {
        let host = job["host_id"].as_str().unwrap_or("");
        if latest
            .get(host)
            .is_none_or(|old| old["created_at"].as_str() < job["created_at"].as_str())
        {
            latest.insert(host, job);
        }
    }
    for (host, job) in latest {
        for checkout in rows(&job["result"]["result"], "workspaces") {
            let project = checkout["github_project"]
                .as_str()
                .or_else(|| checkout["project"].as_str())
                .unwrap_or("");
            if let Some(w) = workspace(&mut all, project) {
                let mut view = fields(
                    checkout,
                    &[
                        "name",
                        "status",
                        "source_commit",
                        "last_checkpoint",
                        "command_timings",
                    ],
                );
                view["command_timings"] = command_timings(checkout);
                view["host_id"] = json!(host);
                view["observed_at"] = job["finished_at"]
                    .as_str()
                    .or_else(|| job["created_at"].as_str())
                    .into();
                push(w, "checkouts", view);
                if !w["storage_nodes"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(host))
                {
                    push(w, "storage_nodes", json!(host));
                }
            }
        }
        let usage = &job["result"]["result"]["storage_usage"];
        if usage["schema"] == 1 {
            let shared = rows(usage, "shared_caches")
                .map(|v| {
                    fields(
                        v,
                        &[
                            "kind",
                            "status",
                            "apparent_bytes",
                            "allocated_bytes",
                            "files",
                        ],
                    )
                })
                .collect::<Vec<_>>();
            let mut project_usage = BTreeMap::<String, Vec<Value>>::new();
            for measured in rows(usage, "projects") {
                let native = measured["project"].as_str().unwrap_or("");
                let logical = rows(&job["result"]["result"], "github_projects")
                    .find(|p| rows(p, "generations").any(|g| g["project"] == native))
                    .and_then(|p| p["id"].as_str())
                    .unwrap_or(native);
                if let Some(w) = workspace(&mut all, logical) {
                    if !w["storage_nodes"]
                        .as_array()
                        .unwrap()
                        .contains(&json!(host))
                    {
                        push(w, "storage_nodes", json!(host));
                    }
                    project_usage
                        .entry(logical.to_owned())
                        .or_default()
                        .push(fields(
                            measured,
                            &[
                                "project",
                                "status",
                                "apparent_bytes",
                                "allocated_bytes",
                                "files",
                            ],
                        ));
                }
            }
            for (project, w) in &mut all {
                if w["storage_nodes"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(host))
                {
                    push(
                        w,
                        "storage_usage",
                        json!({"host_id":host,"observed_at_unix_ms":usage["observed_at_unix_ms"],"status":usage["status"],"shared_caches":shared,"projects":project_usage.get(project).cloned().unwrap_or_default()}),
                    );
                }
            }
        }
        for project in rows(&job["result"]["result"], "github_projects") {
            if let Some(w) = workspace(&mut all, project["id"].as_str().unwrap_or("")) {
                let mut view = fields(project, &["id", "repository", "branch"]);
                // Main revision is source evidence; private sync/auth state is omitted.
                view["observed_at"] = job["created_at"].clone();
                view["host_id"] = json!(host);
                push(w, "source_observations", view);
            }
        }
    }
    let mut ordered = jobs.iter().collect::<Vec<_>>();
    ordered.sort_by(|a, b| b["created_at"].as_str().cmp(&a["created_at"].as_str()));
    for job in ordered {
        let project = job["project"].as_str();
        // A historical operation cannot safely be attributed by a reused checkout
        // name. Only explicit admission-time project attribution is accepted.
        if let Some(w) = project.and_then(|p| workspace(&mut all, p)) {
            let mut view = fields(
                job,
                &[
                    "id",
                    "host_id",
                    "operation",
                    "name",
                    "status",
                    "created_at",
                    "started_at",
                    "finished_at",
                    "duration_seconds",
                ],
            );
            view["operation"] = json!(operation(job));
            let result = &job["result"]["result"];
            if operation(job) == "github-settings"
                && job["status"] == "succeeded"
                && !w["integration_observations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|v| v["host_id"] == job["host_id"])
            {
                let receipt = json!({"host_id":job["host_id"],"observed_at":job["finished_at"].as_str().or_else(|| job["created_at"].as_str()),
                    "main":fields(&result["main"], &["revision", "sequence"]),
                    "sync_enabled":result["sync"]["enabled"],
                    "sync_status":result["sync"]["attempt"]["status"]});
                push(w, "integration_observations", receipt);
            }
            for key in ["bytes", "files"] {
                if result[key].is_u64() {
                    view[key] = result[key].clone();
                }
            }
            if w["operations"].as_array().unwrap().len() < 30 {
                push(w, "operations", view);
            }
        }
    }
    for run in rows(runs, "runs") {
        let pool_project = run["pool"]
            .as_str()
            .and_then(|p| pool_projects.get(p))
            .copied();
        let repo_project = run["repository"]
            .as_str()
            .and_then(|r| r.split_once('/'))
            .map(|(p, _)| p);
        if pool_project.is_some() && repo_project.is_some() && pool_project != repo_project {
            continue;
        }
        if let Some(w) = pool_project
            .or(repo_project)
            .and_then(|p| workspace(&mut all, p))
        {
            push(
                w,
                "runs",
                fields(
                    run,
                    &[
                        "id",
                        "host_id",
                        "name",
                        "provider",
                        "pool",
                        "repository",
                        "status",
                        "created_at",
                        "started_at",
                        "finished_at",
                        "elapsed_seconds",
                        "execution_uncertain",
                    ],
                ),
            );
        }
    }
    for event in rows(events, "events") {
        if let Some(w) = workspace(&mut all, event["project"].as_str().unwrap_or("")) {
            push(
                w,
                "launch_events",
                fields(
                    event,
                    &[
                        "at",
                        "kind",
                        "outcome",
                        "run_id",
                        "pool",
                        "host_id",
                        "workspace",
                        "stage",
                        "reason",
                        "duration_ms",
                        "queue_wait_ms",
                    ],
                ),
            );
        }
    }
    json!({"workspaces":all.into_values().collect::<Vec<_>>(), "coverage":{
        "runs_truncated":runs["truncated"], "run_history_limit":256, "operation_history_limit":200,
        "operations_per_workspace_limit":30, "physical_cache_bytes":null,
        "storage_observations":"Latest successful controller list receipts; refresh storage to observe changes.",
        "integration":"Native run success is not acceptance or merge evidence. Review LayerFS source and promotion separately."
    }})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn projects_are_attributed_and_private_run_and_job_data_are_excluded() {
        let result = catalog(
            &json!({"repositories":[{"project":"app","name":"main","backend":"github"}]}),
            &[json!({"project":"app","name":"pool"})],
            &json!({}),
            &json!({"runs":[{"id":"ok","pool":"pool","repository":"app/main","task":"SECRET","env_keys":["SECRET"]},{"id":"wrong","pool":"pool","repository":"other/main"}],"truncated":true}),
            &json!({}),
            &[
                json!({"project":"app","id":"job","operation":"storage_github-import","status":"succeeded","result":{"result":{"bytes":42,"stdout":"SECRET"}},"duration_seconds":0.2}),
            ],
        );
        let w = &result["workspaces"][0];
        assert_eq!(w["runs"].as_array().unwrap().len(), 1);
        assert_eq!(w["operations"][0]["bytes"], 42);
        assert_eq!(result["coverage"]["runs_truncated"], true);
        assert!(result["coverage"]["physical_cache_bytes"].is_null());
        assert!(!result.to_string().contains("SECRET"));
    }
    #[test]
    fn command_receipts_are_typed_redacted_and_uncertain_duration_stays_unknown() {
        let job = job_observation(
            &json!({"host_id":"node","operation":"storage_list","status":"succeeded","created_at":"1","result":{"result":{"workspaces":[{"project":"app","name":"agent","status":"mounted","last_checkpoint":{"kind":"Committed","head":"head","private":"SECRET"},"command_timings":[
            {"kind":"test","status":"uncertain","elapsed_ms":null,"observed_at_unix_ms":1,"argv":["SECRET"]},
            {"kind":"build","status":"succeeded","elapsed_ms":0,"observed_at_unix_ms":2,"output":"SECRET"},
            {"kind":"invented","status":"succeeded","elapsed_ms":1,"observed_at_unix_ms":3}]}]}}}),
        );
        assert!(!job.to_string().contains("SECRET"));
        let result = catalog(&json!({}), &[], &json!({}), &json!({}), &json!({}), &[job]);
        let checkout = &result["workspaces"][0]["checkouts"][0];
        assert_eq!(checkout["command_timings"].as_array().unwrap().len(), 2);
        assert_eq!(checkout["command_timings"][0]["status"], "uncertain");
        assert!(checkout["command_timings"][0]["elapsed_ms"].is_null());
        assert_eq!(checkout["command_timings"][1]["elapsed_ms"], 0);
        assert_eq!(checkout["last_checkpoint"]["head"], "head");
    }
    #[test]
    fn measured_usage_maps_generation_without_assigning_shared_cache_to_project() {
        let raw = json!({"id":"receipt","host_id":"node","operation":"storage_list","status":"succeeded","created_at":"2026-10-01", "result":{"result":{
            "stdout":"SECRET", "github_projects":[{"id":"app","generations":[{"project":"native-generation","private_path":"SECRET"}]}],
            "storage_usage":{"schema":1,"observed_at_unix_ms":42,"status":"partial",
                "shared_caches":[{"kind":"pnpm-store","status":"partial","apparent_bytes":100,"allocated_bytes":4096,"files":2,"path":"SECRET"}],
                "projects":[{"project":"native-generation","status":"complete","apparent_bytes":50,"allocated_bytes":4096,"files":1}]}}}});
        let receipt = job_observation(&raw);
        assert!(!receipt.to_string().contains("SECRET"));
        let result = catalog(
            &json!({}),
            &[],
            &json!({}),
            &json!({}),
            &json!({}),
            &[receipt],
        );
        let w = &result["workspaces"][0];
        assert_eq!(w["id"], "app");
        assert_eq!(
            w["storage_usage"][0]["shared_caches"][0]["apparent_bytes"],
            100
        );
        assert_eq!(w["storage_usage"][0]["projects"][0]["apparent_bytes"], 50);
        assert_eq!(w["storage_usage"][0]["status"], "partial");
    }
    #[test]
    fn acceptance_comes_only_from_successful_settings_and_checkout_attribution_is_host_scoped() {
        let inventory = json!({"host_id":"node","operation":"storage_list","status":"succeeded","created_at":"1","result":{"result":{"workspaces":[{"name":"agent","project":"native","github_project":"app","status":"closed"}]}}});
        assert_eq!(
            checkout_project(std::slice::from_ref(&inventory), "node", "agent").as_deref(),
            Some("app")
        );
        assert_eq!(
            checkout_project(std::slice::from_ref(&inventory), "other", "agent"),
            None
        );
        let result = catalog(
            &json!({}),
            &[],
            &json!({}),
            &json!({}),
            &json!({}),
            &[
                inventory,
                json!({"host_id":"node","project":"app","operation":"storage_github-settings","status":"succeeded","created_at":"2","result":{"result":{"main":{"revision":"accepted","sequence":2},"sync":{"enabled":true,"attempt":{"status":"delivered","error":"SECRET"}}}}}),
                json!({"host_id":"node","project":"app","operation":"storage_github-settings","status":"failed","created_at":"3","result":{"result":{"main":{"revision":"incorrect"}}}}),
            ],
        );
        let w = &result["workspaces"][0];
        assert_eq!(
            w["integration_observations"][0]["main"]["revision"],
            "accepted"
        );
        assert_eq!(w["integration_observations"][0]["sync_status"], "delivered");
        assert!(!result.to_string().contains("SECRET"));
    }
    #[test]
    fn latest_empty_snapshot_removes_stale_checkout_and_unattributed_jobs_stay_unknown() {
        let result = catalog(
            &json!({}),
            &[],
            &json!({"nodes":[{"host_id":"node","projects":[{"id":"app"}]}]}),
            &json!({}),
            &json!({}),
            &[
                json!({"host_id":"node","operation":"storage_list","status":"succeeded","created_at":"1","result":{"result":{"workspaces":[{"project":"app","name":"old","status":"ready"}]}}}),
                json!({"host_id":"node","operation":"storage_list","status":"succeeded","created_at":"2","result":{"result":{"workspaces":[]}}}),
                json!({"host_id":"node","operation":"storage_deps","name":"old","status":"succeeded","duration_seconds":1}),
            ],
        );
        assert_eq!(result["workspaces"][0]["checkouts"], json!([]));
        assert_eq!(result["workspaces"][0]["operations"], json!([]));
    }
}
