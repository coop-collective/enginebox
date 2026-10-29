pub mod acp;
pub mod ci;
pub mod tasks;
use anyhow::{Context, Result, ensure};
use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::io::Read;
use std::{path::PathBuf, time::Duration};

#[derive(Clone)]
pub struct FleetClient {
    client: Client,
    base: String,
    token: String,
    /// `ENGINEBOX_ORG`: act for this org (sent as `X-EngineBox-Org`); unset
    /// acts for the platform org, as before orgs.
    org: Option<String>,
}
impl FleetClient {
    pub fn new(base: &str, token: String) -> Result<Self> {
        let url = url::Url::parse(base)?;
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && url.path() == "/",
            "Use a fleet server origin without credentials, path, query or fragment"
        );
        let local = url.host_str().is_some_and(|host| {
            host == "localhost"
                || host == "[::1]"
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        ensure!(
            url.scheme() == "https" || (url.scheme() == "http" && local),
            "Remote fleet API requires HTTPS"
        );
        ensure!(
            token.len() >= 32 && token.bytes().all(|b| (33..=126).contains(&b)),
            "Set a valid API token"
        );
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(45))
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            client,
            base: base.trim_end_matches('/').to_owned(),
            token,
            org: None,
        })
    }
    pub fn from_env() -> Result<Self> {
        ensure!(
            std::env::var("ENGINEBOX_GUEST").unwrap_or_default() != "1",
            "Operator fleet access is unavailable in guest mode"
        );
        let base =
            std::env::var("ENGINEBOX_API_URL").unwrap_or_else(|_| "http://127.0.0.1:8765".into());
        let token = match std::env::var_os("ENGINEBOX_API_TOKEN_FILE") {
            Some(path) => std::fs::read_to_string(PathBuf::from(path))?
                .trim()
                .to_owned(),
            None => std::env::var("ENGINEBOX_API_TOKEN")
                .context("Set ENGINEBOX_API_TOKEN_FILE or ENGINEBOX_API_TOKEN")?,
        };
        let mut client = Self::new(&base, token)?;
        if let Ok(org) = std::env::var("ENGINEBOX_ORG")
            && !org.is_empty()
        {
            ensure!(
                org.len() == 28
                    && org.starts_with("org_")
                    && org[4..]
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "ENGINEBOX_ORG must be an org ID"
            );
            client.org = Some(org);
        }
        Ok(client)
    }
    pub fn request(&self, path: &str, body: Option<Value>) -> Result<Value> {
        ensure!(
            path.starts_with("/v1/")
                && !path.contains(['?', '#', '%', '\\'])
                && !path.split('/').any(|s| s == ".." || s == "."),
            "Invalid API path"
        );
        let url = format!("{}{path}", self.base);
        let req = if let Some(body) = body {
            self.client.post(url).json(&body)
        } else {
            self.client.get(url)
        };
        let req = match &self.org {
            Some(org) => req.header("X-EngineBox-Org", org),
            None => req,
        };
        let response = req
            .bearer_auth(&self.token)
            .send()
            .context("Fleet API request failed")?;
        let status = response.status();
        let mut bytes = Vec::new();
        std::io::Read::take(response, 4 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 4 * 1024 * 1024,
            "Fleet response exceeded the size limit"
        );
        let v: Value = serde_json::from_slice(&bytes).context("Fleet did not return JSON")?;
        ensure!(
            status.is_success(),
            "{}",
            v["error"].as_str().unwrap_or("Fleet request failed")
        );
        Ok(v)
    }
}

fn property(kind: &str, description: &str) -> Value {
    json!({"type":kind,"description":description})
}
fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false}})
}
pub fn tools() -> Vec<Value> {
    vec![
        tool(
            "workspace_catalog",
            "Read the owner's unified workspace sources, environments, retained activity and measured storage receipts. Observations are not CI acceptance evidence.",
            json!({}),
            &[],
        ),
        tool(
            "repository_list",
            "List source repositories registered to an authorized project.",
            json!({"project":{"type":"string"}}),
            &["project"],
        ),
        tool(
            "repository_create",
            "Create an idempotent hosted source repository for an authorized project. Requires a configured repository backend; does not prepare a LayerFS checkout.",
            json!({"project":{"type":"string"},"name":{"type":"string"},"description":{"type":"string","maxLength":512}}),
            &["project", "name"],
        ),
        tool(
            "repository_link_github",
            "Owner-only: map an already selected GitHub repository to a project. Selection and grants remain enforced; this does not import or overwrite LayerFS source.",
            json!({"project":{"type":"string"},"name":{"type":"string"},"full_name":{"type":"string"},"description":{"type":"string","maxLength":512}}),
            &["project", "name", "full_name"],
        ),
        tool(
            "pool_agent_launch",
            "Submit a native agent task to an authorized project pool. May consume provider allowance. An unconfirmed response must be reconciled using agent_runs, never automatically replayed. Run success does not approve promotion.",
            json!({"pool":{"type":"string"},"provider":{"type":"string","enum":["codex","claude","cursor"]},"prompt":{"type":"string","minLength":1,"maxLength":12000},"repository":{"type":"string","description":"Optional existing project/name source; credential access does not prepare source."},"timeout_seconds":{"type":"integer","minimum":1,"maximum":86400},"model":{"type":"string"}}),
            &["pool", "provider", "prompt"],
        ),
        tool(
            "pool_agent_run",
            "Read a pool run, including a queued run without an assigned environment.",
            json!({"pool":{"type":"string"},"run_id":{"type":"string","pattern":"^[a-f0-9]{32}$"}}),
            &["pool", "run_id"],
        ),
        tool(
            "pool_agent_cancel",
            "Request cancellation of a pool run. Poll pool_agent_run until termination is confirmed; retained capacity is not released on an uncertain response.",
            json!({"pool":{"type":"string"},"run_id":{"type":"string","pattern":"^[a-f0-9]{32}$"}}),
            &["pool", "run_id"],
        ),
        tool(
            "ci_templates",
            "List enabled CI toolchain recipes and per-node verification/install status.",
            json!({}),
            &[],
        ),
        tool(
            "ci_template_register",
            "Register an owner-authored CI toolchain recipe. New recipes start disabled; enabled revisions synchronize to compatible dedicated CI nodes.",
            json!({"manifest":{"type":"object"}}),
            &["manifest"],
        ),
        tool(
            "ci_template_enable",
            "Enable or disable a CI recipe. Enable installs/verifies it on compatible nodes; disable stops new placements without removing files or stopping running jobs.",
            json!({"template":{"type":"string"},"enabled":{"type":"boolean"}}),
            &["template", "enabled"],
        ),
        tool(
            "ci_template_retry",
            "Explicitly retry a failed toolchain installation on one dedicated CI node after inspecting its failure.",
            json!({"template":{"type":"string"},"node":{"type":"string"}}),
            &["template", "node"],
        ),
        tool(
            "ci_nodes",
            "List registered build workers and their declared OS/image/toolchain capabilities.",
            json!({}),
            &[],
        ),
        tool(
            "ci_register",
            "Register an authorized, already provisioned dedicated CI node. Does not install an OS image.",
            json!({"manifest":{"type":"object"}}),
            &["manifest"],
        ),
        tool(
            "ci_plan",
            "Resolve a non-agent CI trigger without executing it.",
            json!({"request":{"type":"object"}}),
            &["request"],
        ),
        tool(
            "ci_trigger",
            "Trigger a shell automation or GitHub workflow once per request_id; poll fleet_job. GitHub dispatch is not build success.",
            json!({"request":{"type":"object"}}),
            &["request"],
        ),
        tool(
            "fleet_inventory",
            "List configured hosts, capacity budgets, prepared templates and workspaces.",
            json!({}),
            &[],
        ),
        tool(
            "fleet_create",
            "Queue a clean VM clone after admission; poll fleet_job.",
            json!({"host_id":property("string","Host ID"),"name":property("string","New ebox- workspace name"),"template":property("string","Prepared template ID"),"size":{"type":"string","enum":["small","standard","large"],"default":"standard"}}),
            &["host_id", "name", "template"],
        ),
        tool(
            "fleet_allocate",
            "Assign a fresh warm workspace, otherwise clone. Always use result.name for subsequent calls.",
            json!({"host_id":property("string","Host ID"),"name":property("string","New ebox- workspace label"),"template":property("string","Prepared template ID"),"size":{"type":"string","enum":["minimum","recommended","small","standard","large"],"default":"recommended"},"resources":{"type":"object","properties":{"cpu":{"type":"integer","minimum":1},"memory_gib":{"type":"integer","minimum":1},"disk_gib":{"type":"integer","minimum":1}},"required":["cpu","memory_gib","disk_gib"],"additionalProperties":false}}),
            &["host_id", "name", "template"],
        ),
        tool(
            "template_catalog",
            "List controller templates and installation status across every node.",
            json!({}),
            &[],
        ),
        tool(
            "template_register",
            "Register a JSON manifest or public HTTPS URL; synchronizes every node.",
            json!({"manifest":{"type":"object"},"url":{"type":"string"}}),
            &[],
        ),
        tool(
            "template_install",
            "Retry a template on one node; controller registration already synchronizes all nodes.",
            json!({"host_id":{"type":"string"},"template":{"type":"string"}}),
            &["host_id", "template"],
        ),
        tool(
            "template_sync",
            "Retry synchronization of a registered template to all nodes.",
            json!({"template":{"type":"string"}}),
            &["template"],
        ),
        tool(
            "agent_connections",
            "Read native agent connection status; never returns credentials.",
            json!({}),
            &[],
        ),
        tool(
            "agent_launch",
            "Run an official native CLI in a private workspace using its existing home/memory. May consume provider allowance. Resume only an exact native_session_id returned by agent_run.",
            json!({"host_id":{"type":"string"},"name":{"type":"string"},"provider":{"type":"string","enum":["codex","claude","cursor"]},"prompt":{"type":"string","minLength":1,"maxLength":12000},"resume_id":{"type":["string","null"]},"workdir":{"type":"string","default":"."},"timeout_seconds":{"type":"integer","minimum":1,"maximum":300,"default":120},"model":{"type":["string","null"]}}),
            &["host_id", "name", "provider", "prompt"],
        ),
        tool(
            "agent_runs",
            "List bounded task summaries across the owner fleet, including agent type, state, elapsed time and reported token usage.",
            json!({}),
            &[],
        ),
        tool(
            "agent_cancel",
            "Request cancellation of one native run. Poll agent_run until cancelled; acceptance is not proof of termination.",
            json!({"host_id":{"type":"string"},"name":{"type":"string"},"run_id":{"type":"string","pattern":"^[a-f0-9]{32}$"}}),
            &["host_id", "name", "run_id"],
        ),
        tool(
            "agent_run",
            "Read a private run's bounded output and native_session_id for exact resume.",
            json!({"host_id":{"type":"string"},"name":{"type":"string"},"run_id":{"type":"string","pattern":"^[a-f0-9]{32}$"}}),
            &["host_id", "name", "run_id"],
        ),
        tool(
            "fleet_action",
            "Queue pause, resume, hibernate or stop. Stop discards RAM; hibernate preserves it.",
            json!({"host_id":{"type":"string"},"name":{"type":"string"},"action":{"type":"string","enum":["pause","resume","hibernate","stop"]}}),
            &["host_id", "name", "action"],
        ),
        tool(
            "fleet_job",
            "Poll queued/running/succeeded/failed fleet job status.",
            json!({"job_id":{"type":"string","pattern":"^[a-f0-9]{32}$"}}),
            &["job_id"],
        ),
        tool(
            "storage_catalog",
            "List controller-registered LayerFS nodes and repository aliases. Registration does not imply live node health.",
            json!({}),
            &[],
        ),
        tool(
            "storage_prepare",
            "Prepare a stopped reserved VM for future LayerFS attachment after compatibility and capacity checks. Does not attach storage, start the VM or delegate credentials; automatic finalization is not implemented. Poll fleet_job.",
            json!({"host_id":{"type":"string"},"project":{"type":"string"},"name":{"type":"string"},"template":{"type":"string"},"size":{"type":"string"},"resources":{"type":"object"}}),
            &["host_id", "project", "name", "template"],
        ),
        tool(
            "storage_allocation",
            "Read the durable stopped-allocation receipt; receipt status is not a live VM health check. Poll fleet_job.",
            json!({"host_id":{"type":"string"},"name":{"type":"string"}}),
            &["host_id", "name"],
        ),
        tool(
            "storage_inspect",
            "Queue a node prerequisite check or read its durable workspace receipts; poll fleet_job. List is not a live health probe.",
            json!({"host_id":{"type":"string"},"operation":{"type":"string","enum":["inspect","list","compatibility"],"default":"list"}}),
            &["host_id"],
        ),
        tool(
            "storage_dev_start",
            "Bootstrap or retry the project's dev environment from its accepted LayerFS .enginebox/config.json. Poll fleet_job, then storage_dev_status for readiness and URLs.",
            json!({"host_id":{"type":"string"},"project":{"type":"string"}}),
            &["host_id", "project"],
        ),
        tool(
            "storage_dev_status",
            "Read project bootstrap readiness, errors and dev URLs. Poll fleet_job.",
            json!({"host_id":{"type":"string"},"project":{"type":"string"}}),
            &["host_id", "project"],
        ),
        tool(
            "storage_github_settings",
            "Inspect a LayerFS project's main revision and optional GitHub delivery state; set enabled to change outbound synchronization. Poll fleet_job.",
            json!({"host_id":{"type":"string"},"project":{"type":"string"},"enabled":{"type":"boolean"}}),
            &["host_id", "project"],
        ),
        tool(
            "storage_github_promote",
            "Promote or merge a closed LayerFS checkpoint to main against expected_main. Enabled GitHub sync reconciles separately; conflicts produce a specialized agent task. Poll fleet_job.",
            json!({"host_id":{"type":"string"},"project":{"type":"string"},"name":{"type":"string"},"expected_main":{"type":"string"},"merge":{"type":"boolean"},"message":{"type":"string"}}),
            &["host_id", "project", "name", "expected_main"],
        ),
        tool(
            "storage_github_delivery",
            "Export accepted LayerFS main, continue delivery, request the dedicated conflict-agent task, accept a resolved agent result, or retry stale reconciliation. The connector alone pushes after LayerFS acceptance. Poll fleet_job.",
            json!({"host_id":{"type":"string"},"project":{"type":"string"},"action":{"type":"string","enum":["export","deliver","agent-task","resolve","retry"]},"attempt":{"type":"string"}}),
            &["host_id", "project", "action"],
        ),
        tool(
            "storage_github_preview",
            "Fetch a GitHub branch on a registered storage node and review its exact commit and approval digest. Omit branch to discover the default on first import; subsequent checks retain the selected branch. Poll fleet_job.",
            json!({"host_id":{"type":"string"},"project":{"type":"string"},"repository":{"type":"string"},"branch":{"type":"string"}}),
            &["host_id", "project", "repository"],
        ),
        tool(
            "storage_github_import",
            "Import a reviewed GitHub tree into LayerFS without Git metadata and queue repo-defined dev bootstrap when configured. Poll fleet_job, then storage_dev_status for dev readiness.",
            json!({"host_id":{"type":"string"},"project":{"type":"string"},"revision":{"type":"string"},"approval":{"type":"string"},"credential_free":{"type":"boolean","const":true}}),
            &[
                "host_id",
                "project",
                "revision",
                "approval",
                "credential_free",
            ],
        ),
        tool(
            "storage_github_fork",
            "Fork the current published GitHub generation. Optional from must be a completed checkpoint in that same generation. Poll fleet_job.",
            json!({"host_id":{"type":"string"},"project":{"type":"string"},"name":{"type":"string"},"from":{"type":"string"}}),
            &["host_id", "project", "name"],
        ),
        tool(
            "storage_preview",
            "Review a registered repository's tracked file list and exact commit approval digest before import. Does not fetch Git or import files.",
            json!({"host_id":{"type":"string"},"project":{"type":"string"},"revision":{"type":"string","default":"HEAD"}}),
            &["host_id", "project"],
        ),
        tool(
            "storage_import",
            "Import a reviewed credential-free Git commit once on a registered node. Requires the exact commit and approval digest from storage_preview. Poll fleet_job.",
            json!({"host_id":{"type":"string"},"project":{"type":"string"},"revision":{"type":"string","pattern":"^([a-f0-9]{40}|[a-f0-9]{64})$"},"approval":{"type":"string","pattern":"^[a-f0-9]{64}$"},"credential_free":{"type":"boolean","const":true}}),
            &[
                "host_id",
                "project",
                "revision",
                "approval",
                "credential_free",
            ],
        ),
        tool(
            "storage_fork",
            "Create a private writable LayerFS checkout from a registered project. Optional from explicitly reuses that workspace's completed checkpoint and installed files; it does not publish a fleet template. Poll fleet_job.",
            json!({"host_id":{"type":"string"},"project":{"type":"string"},"name":{"type":"string"},"from":{"type":"string"}}),
            &["host_id", "project", "name"],
        ),
        tool(
            "storage_action",
            "Queue offline dependencies, file checkpoint, clean remount/close, or evidence-checked recovery. Recovery never replays interrupted commands. These are file operations, not VM RAM snapshots. Poll fleet_job.",
            json!({"host_id":{"type":"string"},"name":{"type":"string"},"action":{"type":"string","enum":["deps","checkpoint","remount","close","recover"]},"message":{"type":"string","maxLength":4096}}),
            &["host_id", "name", "action"],
        ),
        tool(
            "connection_list",
            "Owner/admin: list the org's catalog connections (MCP servers and secret sources) with the profiles that use each. Secret values are never returned; only their names.",
            json!({}),
            &[],
        ),
        tool(
            "connection_create",
            "Owner/admin: create a named connection profiles can share. kind mcp_http (url, headers), mcp_stdio (command under /usr/bin, /usr/local/bin or /opt, args, env, template) or secret_source (source {provider: stored|kitbash, integration, project, environment}, names). Header and env values are references: {env: VAR} (the run supplies it), {secret: NAME} (a value in this request's secrets) or {secret_source: <connection>, name: NAME}. secrets are write-only: stored once, never echoed. Prefer a Kitbash secret_source over pasting values. Audited.",
            json!({"name":{"type":"string"},"kind":{"type":"string","enum":["mcp_http","mcp_stdio","secret_source"]},"description":{"type":"string","maxLength":512},"url":{"type":"string"},"headers":{"type":"object"},"command":{"type":"string"},"args":{"type":"array","items":{"type":"string"}},"env":{"type":"object"},"template":{"type":"string"},"source":{"type":"object"},"names":{"type":"array","items":{"type":"string"}},"secrets":{"type":"object","description":"NAME -> value; write-only"}}),
            &["name", "kind"],
        ),
        tool(
            "connection_update",
            "Owner/admin: change a connection's fields or secrets. Every profile using it picks the change up on its next run. secrets sets values (write-only); remove_secrets drops names. Name and kind never change. Audited.",
            json!({"name":{"type":"string"},"description":{"type":"string","maxLength":512},"url":{"type":"string"},"headers":{"type":"object"},"command":{"type":"string"},"args":{"type":"array","items":{"type":"string"}},"env":{"type":"object"},"template":{"type":"string"},"source":{"type":"object"},"names":{"type":"array","items":{"type":"string"}},"secrets":{"type":"object"},"remove_secrets":{"type":"array","items":{"type":"string"}}}),
            &["name"],
        ),
        tool(
            "connection_delete",
            "Owner/admin: delete a connection and its stored secrets. Refused while a profile uses it (detach first). Audited.",
            json!({"name":{"type":"string"}}),
            &["name"],
        ),
        tool(
            "connection_test",
            "Owner/admin: check that a connection's values resolve and, for mcp_http, that the server answers and accepts the credentials. Reports names and statuses only. Audited.",
            json!({"name":{"type":"string"}}),
            &["name"],
        ),
        tool(
            "profile_get",
            "Owner/admin: read a profile: template, per-provider tools and skills, attached connections and the per-run-kind connection policy. Env values and callback keys are never returned.",
            json!({"profile":{"type":"string"}}),
            &["profile"],
        ),
        tool(
            "profile_attach_connection",
            "Owner/admin: attach a catalog connection to a profile. Without run_kinds it applies to every run kind the policy does not restrict; with run_kinds (e.g. [\"debug\"]) only to runs that state one of those kinds. Audited.",
            json!({"profile":{"type":"string"},"connection":{"type":"string"},"run_kinds":{"type":"array","items":{"type":"string"}}}),
            &["profile", "connection"],
        ),
        tool(
            "profile_detach_connection",
            "Owner/admin: remove a connection from a profile and from its run-kind policy. Audited.",
            json!({"profile":{"type":"string"},"connection":{"type":"string"}}),
            &["profile", "connection"],
        ),
        tool(
            "github_actions_secret_set",
            "Owner only, platform org: create or update a GitHub Actions secret on a repository selected in the GitHub panel (or one of its environments) through the repository GitHub App, which needs the Secrets: read and write permission. The value comes from a secret_source connection (from: {connection, name}; preferred) or write-only value; it is encrypted to the repository's public key and never echoed. Audited.",
            json!({"repository":{"type":"string","description":"owner/repo"},"environment":{"type":"string"},"name":{"type":"string"},"value":{"type":"string"},"from":{"type":"object","properties":{"connection":{"type":"string"},"name":{"type":"string"}}}}),
            &["repository", "name"],
        ),
        tool(
            "repo_requirements_get",
            "Read what a repository's .enginebox/config.json (schema 2) declares per agent profile: secret requirements by name with qualifier and run kinds, MCP connections and servers, and whether each is met, unmet, proposed or needs re-approval. Never values.",
            json!({"project":{"type":"string"},"name":{"type":"string"}}),
            &["project", "name"],
        ),
        tool(
            "repo_config_register",
            "Register the .enginebox/config.json a checkout of this repository contains (and its commit), so its requirements can be reviewed and mapped. A config only declares; it never grants access.",
            json!({"project":{"type":"string"},"name":{"type":"string"},"commit":{"type":"string"},"config":{"type":"object"}}),
            &["project", "name", "config"],
        ),
        tool(
            "repo_requirement_propose_mapping",
            "Propose which catalog connection should satisfy a repository requirement (repo_profile, requirement, kind secret|mcp|mcp_server, connection, optional source_name). Only an org owner or admin can approve it; until then the requirement stays unmet.",
            json!({"project":{"type":"string"},"name":{"type":"string"},"repo_profile":{"type":"string"},"kind":{"type":"string","enum":["secret","mcp","mcp_server"]},"requirement":{"type":"string"},"connection":{"type":"string"},"source_name":{"type":"string"}}),
            &["project", "name", "repo_profile", "requirement"],
        ),
        tool(
            "catalog_apply",
            "Owner/admin: make the org's connections and profiles match a declarative catalog document (schema enginebox.catalog/v1; secret references only, never values). Idempotent. dry_run returns the diff without writing; prune deletes records the document does not mention. Audited.",
            json!({"document":{"type":"object"},"dry_run":{"type":"boolean"},"prune":{"type":"boolean"}}),
            &["document"],
        ),
    ]
}
/// A catalog record name (connection or profile).
fn catalog_name(v: &Value, key: &str) -> Result<String> {
    let value = field(v, key)?;
    ensure!(
        (1..=63).contains(&value.len())
            && value.as_bytes()[0].is_ascii_lowercase()
            && value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
        "Invalid {key}"
    );
    Ok(value.to_owned())
}
fn field<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key].as_str().context("Required string field missing")
}
fn id(v: &Value, key: &str, instance: bool) -> Result<String> {
    Ok(enginebox_core::identifier(field(v, key)?, instance)?.to_owned())
}
fn record_id(v: &Value, key: &str) -> Result<String> {
    let value = field(v, key)?;
    ensure!(
        value.len() == 32
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "Invalid record ID"
    );
    Ok(value.to_owned())
}
fn storage_id(v: &Value, key: &str) -> Result<String> {
    let value = id(v, key, false)?;
    ensure!(
        value.len() <= 48 && !value.ends_with('-'),
        "Invalid storage ID"
    );
    Ok(value)
}
fn repository_name(value: &str) -> Result<&str> {
    ensure!(
        (1..=63).contains(&value.len())
            && value.as_bytes()[0].is_ascii_alphanumeric()
            && value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
            && !value.ends_with(".git")
            && !value.ends_with(".lock")
            && !value.contains(".."),
        "Invalid repository name"
    );
    Ok(value)
}

pub fn tool_request(name: &str, args: &Value) -> Result<(String, Option<Value>)> {
    let spec = tools()
        .into_iter()
        .find(|t| t["name"] == name)
        .context("Unknown tool")?;
    let object = args.as_object().context("Arguments must be an object")?;
    ensure!(
        object
            .keys()
            .all(|k| spec["inputSchema"]["properties"].get(k).is_some()),
        "Unsupported tool argument"
    );
    for key in spec["inputSchema"]["required"].as_array().unwrap() {
        ensure!(
            object.contains_key(key.as_str().unwrap()),
            "Required argument missing"
        );
    }
    let result = match name {
        "workspace_catalog" => ("/v1/workspaces".into(), None),
        "connection_list" => ("/v1/connections".into(), None),
        "connection_create" => ("/v1/connections".into(), Some(args.clone())),
        "connection_update" => {
            let mut body = args.clone();
            body.as_object_mut().unwrap().remove("name");
            (
                format!("/v1/connections/{}", catalog_name(args, "name")?),
                Some(body),
            )
        }
        "connection_delete" | "connection_test" => (
            format!(
                "/v1/connections/{}/{}",
                catalog_name(args, "name")?,
                if name == "connection_delete" {
                    "delete"
                } else {
                    "test"
                }
            ),
            Some(json!({})),
        ),
        "profile_get" => (
            format!("/v1/profiles/{}", catalog_name(args, "profile")?),
            None,
        ),
        "profile_attach_connection" | "profile_detach_connection" => {
            let mut body = json!({"connection": catalog_name(args, "connection")?});
            if name == "profile_attach_connection"
                && let Some(kinds) = args.get("run_kinds")
            {
                body["run_kinds"] = kinds.clone();
            }
            (
                format!(
                    "/v1/profiles/{}/connections/{}",
                    catalog_name(args, "profile")?,
                    if name == "profile_attach_connection" {
                        "attach"
                    } else {
                        "detach"
                    }
                ),
                Some(body),
            )
        }
        "catalog_apply" => ("/v1/catalog/apply".into(), Some(args.clone())),
        "repo_requirements_get" | "repo_config_register" | "repo_requirement_propose_mapping" => {
            let base = format!(
                "/v1/projects/{}/repositories/{}",
                storage_id(args, "project")?,
                repository_name(field(args, "name")?)?
            );
            let mut body = args.clone();
            let map = body.as_object_mut().unwrap();
            map.remove("project");
            map.remove("name");
            match name {
                "repo_requirements_get" => (format!("{base}/requirements"), None),
                "repo_config_register" => (format!("{base}/agent-config"), Some(body)),
                _ => (format!("{base}/requirements/propose"), Some(body)),
            }
        }
        "github_actions_secret_set" => {
            ensure!(
                args.get("value").is_some() != args.get("from").is_some(),
                "Send exactly one of value or from"
            );
            ("/v1/actions-secrets".into(), Some(args.clone()))
        }
        "repository_list" | "repository_create" | "repository_link_github" => {
            let project = storage_id(args, "project")?;
            let mut path = format!("/v1/projects/{project}/repositories");
            if name == "repository_list" {
                (path, None)
            } else {
                let mut body = json!({"name":repository_name(field(args,"name")?)?});
                if let Some(description) = args.get("description") {
                    let description = description
                        .as_str()
                        .context("Description must be a string")?;
                    ensure!(
                        description.len() <= 512 && !description.chars().any(char::is_control),
                        "Invalid repository description"
                    );
                    body["description"] = json!(description);
                }
                if name == "repository_link_github" {
                    let full_name = field(args, "full_name")?;
                    // Normalize only a repository identity; no arbitrary URL or credential input.
                    body["full_name"] =
                        json!(enginebox_storage_layerfs::github::repository(full_name)?);
                    path.push_str("/github");
                }
                (path, Some(body))
            }
        }
        "pool_agent_run" | "pool_agent_cancel" => (
            format!(
                "/v1/pools/{}/agent-runs/{}{}",
                id(args, "pool", false)?,
                record_id(args, "run_id")?,
                if name == "pool_agent_cancel" {
                    "/cancel"
                } else {
                    ""
                }
            ),
            (name == "pool_agent_cancel").then(|| json!({})),
        ),
        "pool_agent_launch" => {
            let pool = id(args, "pool", false)?;
            ensure!(
                ["codex", "claude", "cursor"].contains(&field(args, "provider")?),
                "Unknown agent"
            );
            let prompt = field(args, "prompt")?;
            ensure!(
                !prompt.trim().is_empty() && prompt.len() <= 12000 && !prompt.contains('\0'),
                "Invalid prompt"
            );
            if let Some(repository) = args.get("repository") {
                let (project, name) = repository
                    .as_str()
                    .and_then(|r| r.split_once('/'))
                    .context("Repository must be project/name")?;
                enginebox_storage_layerfs::config::identifier(project)?;
                repository_name(name)?;
            }
            if let Some(timeout) = args.get("timeout_seconds") {
                ensure!(
                    timeout.as_u64().is_some_and(|n| (1..=86400).contains(&n)),
                    "Invalid run timeout"
                );
            }
            if let Some(model) = args.get("model") {
                ensure!(
                    model.as_str().is_some_and(|m| !m.is_empty()
                        && m.len() <= 256
                        && !m.chars().any(char::is_control)),
                    "Invalid model"
                );
            }
            let mut body = args.clone();
            body.as_object_mut().unwrap().remove("pool");
            (format!("/v1/pools/{pool}/agent-runs"), Some(body))
        }
        "ci_templates" => ("/v1/ci/templates".into(), None),
        "ci_template_register" => {
            let manifest: enginebox_ci::templates::Template =
                serde_json::from_value(args["manifest"].clone())?;
            manifest.validate()?;
            (
                "/v1/ci/templates".into(),
                Some(json!({"manifest":manifest})),
            )
        }
        "ci_template_enable" | "ci_template_retry" => {
            let id = args["template"].as_str().context("Template required")?;
            ensure!(enginebox_ci::model::identifier(id), "Invalid template ID");
            if name == "ci_template_retry" {
                let node = args["node"].as_str().context("Node required")?;
                ensure!(enginebox_ci::model::identifier(node), "Invalid node ID");
                (
                    format!("/v1/ci/templates/{id}/retry"),
                    Some(json!({"node":node})),
                )
            } else {
                let enabled = args["enabled"]
                    .as_bool()
                    .context("Enabled must be boolean")?;
                (
                    format!(
                        "/v1/ci/templates/{id}/{}",
                        if enabled { "enable" } else { "disable" }
                    ),
                    Some(json!({})),
                )
            }
        }
        "ci_nodes" => ("/v1/ci".into(), None),
        "ci_register" => {
            let node: enginebox_ci::Node = serde_json::from_value(args["manifest"].clone())?;
            node.validate()?;
            ("/v1/ci/nodes".into(), Some(serde_json::to_value(node)?))
        }
        "ci_plan" | "ci_trigger" => {
            let request: enginebox_ci::Request = serde_json::from_value(args["request"].clone())?;
            request.validate()?;
            (
                format!("/v1/ci/{}", if name == "ci_plan" { "plan" } else { "runs" }),
                Some(serde_json::to_value(request)?),
            )
        }
        "fleet_inventory" => ("/v1/fleet".into(), None),
        "template_catalog" => ("/v1/templates".into(), None),
        "agent_connections" => ("/v1/agents".into(), None),
        "agent_runs" => ("/v1/agent-runs".into(), None),
        "storage_catalog" => ("/v1/storage".into(), None),
        "storage_dev_start"
        | "storage_dev_status"
        | "storage_inspect"
        | "storage_preview"
        | "storage_import"
        | "storage_fork"
        | "storage_action"
        | "storage_prepare"
        | "storage_allocation"
        | "storage_github_preview"
        | "storage_github_import"
        | "storage_github_fork"
        | "storage_github_settings"
        | "storage_github_promote"
        | "storage_github_delivery" => {
            let mut body = json!({});
            let operation = match name {
                "storage_dev_start" | "storage_dev_status" => {
                    body = json!({"project": storage_id(args, "project")?});
                    if name == "storage_dev_start" {
                        "dev-start"
                    } else {
                        "dev-status"
                    }
                }
                "storage_inspect" => {
                    let operation = args
                        .get("operation")
                        .map(|v| v.as_str().context("Invalid storage observation"))
                        .transpose()?
                        .unwrap_or("list");
                    ensure!(
                        ["list", "inspect", "compatibility"].contains(&operation),
                        "Unknown storage observation"
                    );
                    operation
                }
                "storage_prepare" => {
                    body = json!({"project":storage_id(args,"project")?,"name":id(args,"name",true)?,"template":id(args,"template",false)?});
                    if let Some(size) = object.get("size") {
                        ensure!(size.is_string(), "Invalid allocation size");
                        body["size"] = size.clone();
                    }
                    if let Some(resources) = object.get("resources") {
                        ensure!(resources.is_object(), "Invalid allocation resources");
                        body["resources"] = resources.clone();
                    }
                    "prepare"
                }
                "storage_allocation" => {
                    body = json!({"name":id(args,"name",true)?});
                    "allocation"
                }
                "storage_github_settings"
                | "storage_github_promote"
                | "storage_github_delivery" => {
                    body["project"] = json!(storage_id(args, "project")?);
                    if name == "storage_github_settings" {
                        if let Some(enabled) = object.get("enabled") {
                            ensure!(enabled.is_boolean(), "enabled must be boolean");
                            body["enabled"] = enabled.clone();
                        }
                        "github-settings"
                    } else if name == "storage_github_promote" {
                        body["name"] = json!(storage_id(args, "name")?);
                        let expected = field(args, "expected_main")?;
                        ensure!(
                            enginebox_storage_layerfs::config::digest(expected),
                            "Exact main revision required"
                        );
                        body["expected_main"] = json!(expected);
                        if let Some(merge) = object.get("merge") {
                            ensure!(merge.is_boolean(), "merge must be boolean");
                            body["merge"] = merge.clone();
                        }
                        if let Some(message) = object.get("message") {
                            let text = message.as_str().context("Invalid message")?;
                            ensure!(
                                text.len() <= 4096 && !text.contains('\0'),
                                "Invalid message"
                            );
                            body["message"] = message.clone();
                        }
                        "github-promote"
                    } else {
                        let action = field(args, "action")?;
                        ensure!(
                            ["export", "deliver", "agent-task", "resolve", "retry"]
                                .contains(&action),
                            "Unsupported sync action"
                        );
                        body["action"] = json!(action);
                        if action != "export" {
                            body["attempt"] = json!(storage_id(args, "attempt")?);
                        } else {
                            ensure!(
                                !object.contains_key("attempt"),
                                "Export selects accepted main"
                            );
                        }
                        "github-delivery"
                    }
                }
                "storage_github_preview" => {
                    body = json!({"project":storage_id(args,"project")?,"repository":enginebox_storage_layerfs::github::repository(field(args,"repository")?)?});
                    if object.contains_key("branch") {
                        let branch = field(args, "branch")?;
                        enginebox_storage_layerfs::github::branch(branch)?;
                        body["branch"] = json!(branch);
                    }
                    "github-preview"
                }
                "storage_preview" | "storage_import" | "storage_github_import" => {
                    body["project"] = json!(storage_id(args, "project")?);
                    let revision = args
                        .get("revision")
                        .map(|v| v.as_str().context("Invalid revision"))
                        .transpose()?
                        .unwrap_or("HEAD");
                    ensure!(
                        !revision.is_empty()
                            && revision.len() <= 256
                            && !revision.starts_with('-')
                            && !revision.chars().any(char::is_control),
                        "Invalid revision"
                    );
                    body["revision"] = json!(revision);
                    if name != "storage_preview" {
                        let approval = field(args, "approval")?;
                        ensure!(
                            [40, 64].contains(&revision.len())
                                && revision
                                    .bytes()
                                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                            "Exact commit ID required"
                        );
                        ensure!(
                            approval.len() == 64
                                && approval
                                    .bytes()
                                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                            "Invalid approval digest"
                        );
                        ensure!(
                            args["credential_free"] == true,
                            "Explicit credential-free approval required"
                        );
                        body["approval"] = json!(approval);
                        body["credential_free"] = json!(true);
                        if name == "storage_github_import" {
                            "github-import"
                        } else {
                            "import"
                        }
                    } else {
                        "preview"
                    }
                }
                "storage_fork" | "storage_github_fork" => {
                    body = json!({"project":storage_id(args,"project")?,"name":storage_id(args,"name")?});
                    if object.contains_key("from") {
                        body["from"] = json!(storage_id(args, "from")?);
                    }
                    if name == "storage_github_fork" {
                        "github-fork"
                    } else {
                        "fork"
                    }
                }
                "storage_action" => {
                    let action = field(args, "action")?;
                    ensure!(
                        ["deps", "checkpoint", "remount", "close", "recover"].contains(&action),
                        "Unsupported storage action"
                    );
                    body["name"] = json!(storage_id(args, "name")?);
                    if object.contains_key("message") {
                        ensure!(action == "checkpoint", "Only checkpoints accept a message");
                        let message = field(args, "message")?;
                        ensure!(
                            message.len() <= 4096 && !message.contains('\0'),
                            "Invalid checkpoint message"
                        );
                        body["message"] = json!(message);
                    }
                    action
                }
                _ => unreachable!(),
            };
            (
                format!(
                    "/v1/storage/hosts/{}/{}",
                    id(args, "host_id", false)?,
                    operation
                ),
                Some(body),
            )
        }
        "fleet_job" => (format!("/v1/jobs/{}", record_id(args, "job_id")?), None),
        "template_register" => {
            ensure!(
                object.len() == 1 && (args["manifest"].is_object() || args["url"].is_string()),
                "Provide exactly one manifest object or URL"
            );
            ("/v1/templates".into(), Some(args.clone()))
        }
        "template_sync" => (
            format!("/v1/templates/{}/sync", id(args, "template", false)?),
            Some(json!({})),
        ),
        "template_install" => (
            format!(
                "/v1/hosts/{}/templates/{}/install",
                id(args, "host_id", false)?,
                id(args, "template", false)?
            ),
            Some(json!({})),
        ),
        "fleet_create" | "fleet_allocate" => {
            let mut body =
                json!({"name":id(args,"name",true)?,"template":id(args,"template",false)?});
            if let Some(resources) = args.get("resources") {
                ensure!(!object.contains_key("size"), "Choose resources or size");
                enginebox_core::Resources::from_value(resources, None)?;
                body["resources"] = resources.clone();
            } else {
                let size =
                    args.get("size")
                        .and_then(Value::as_str)
                        .unwrap_or(if name == "fleet_create" {
                            "standard"
                        } else {
                            "recommended"
                        });
                ensure!(
                    ["minimum", "recommended", "small", "standard", "large"].contains(&size),
                    "Unknown size"
                );
                body["size"] = json!(size);
            }
            (
                format!(
                    "/v1/hosts/{}/{}",
                    id(args, "host_id", false)?,
                    if name == "fleet_create" {
                        "instances"
                    } else {
                        "allocations"
                    }
                ),
                Some(body),
            )
        }
        "fleet_action" => {
            let action = field(args, "action")?;
            ensure!(
                ["pause", "resume", "hibernate", "stop"].contains(&action),
                "Unknown action"
            );
            (
                format!(
                    "/v1/hosts/{}/instances/{}/actions",
                    id(args, "host_id", false)?,
                    id(args, "name", true)?
                ),
                Some(json!({"action":action})),
            )
        }
        "agent_cancel" => (
            format!(
                "/v1/hosts/{}/instances/{}/agent-runs/{}/cancel",
                id(args, "host_id", false)?,
                id(args, "name", true)?,
                record_id(args, "run_id")?
            ),
            Some(json!({})),
        ),
        "agent_run" => (
            format!(
                "/v1/hosts/{}/instances/{}/agent-runs/{}",
                id(args, "host_id", false)?,
                id(args, "name", true)?,
                record_id(args, "run_id")?
            ),
            None,
        ),
        "agent_launch" => {
            let provider = field(args, "provider")?;
            ensure!(
                ["codex", "claude", "cursor"].contains(&provider),
                "Unknown agent"
            );
            let prompt = field(args, "prompt")?;
            ensure!(
                !prompt.trim().is_empty() && prompt.len() <= 12000 && !prompt.contains('\0'),
                "Prompt must contain 1–12000 bytes"
            );
            let mut body = args.clone();
            body.as_object_mut().unwrap().remove("host_id");
            body.as_object_mut().unwrap().remove("name");
            (
                format!(
                    "/v1/hosts/{}/instances/{}/agent-runs",
                    id(args, "host_id", false)?,
                    id(args, "name", true)?
                ),
                Some(body),
            )
        }
        _ => unreachable!(),
    };
    Ok(result)
}

pub fn rpc(request: Value, client: &FleetClient) -> Option<Value> {
    let id = request.get("id")?.clone();
    let error = |code, message: &str| json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}});
    if request["jsonrpc"] != "2.0" || (!id.is_string() && !id.is_number() && !id.is_null()) {
        return Some(error(-32600, "Invalid request"));
    }
    let result = match request["method"].as_str() {
        Some("initialize") => {
            let offered = request["params"]["protocolVersion"].as_str().unwrap_or("");
            let supported = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
            json!({"protocolVersion":if supported.contains(&offered){offered}else{"2025-11-25"},"capabilities":{"tools":{}},"serverInfo":{"name":"enginebox-fleet","version":env!("CARGO_PKG_VERSION")},"instructions":"Operate authorized private workspaces using native provider harnesses."})
        }
        Some("ping") => json!({}),
        Some("tools/list") => json!({"tools":tools()}),
        Some("tools/call") => {
            let result = (|| {
                let name = request["params"]["name"]
                    .as_str()
                    .context("Missing tool name")?;
                let args = request["params"]
                    .get("arguments")
                    .cloned()
                    .unwrap_or(json!({}));
                let (path, body) = tool_request(name, &args)?;
                client.request(&path, body)
            })();
            match result {
                Ok(value) => {
                    json!({"content":[{"type":"text","text":value.to_string()}],"structuredContent":value,"isError":false})
                }
                Err(e) => json!({"content":[{"type":"text","text":e.to_string()}],"isError":true}),
            }
        }
        _ => return Some(error(-32601, "Method not found")),
    };
    Some(json!({"jsonrpc":"2.0","id":id,"result":result}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalog_tools_map_to_owner_only_controller_routes() {
        let (path, body) = tool_request("connection_list", &json!({})).unwrap();
        assert_eq!((path.as_str(), body), ("/v1/connections", None));
        let (path, body) = tool_request(
            "connection_update",
            &json!({"name":"kitbash","secrets":{"KITBASH_TOKEN":"v-123456789"}}),
        )
        .unwrap();
        assert_eq!(path, "/v1/connections/kitbash");
        assert!(body.unwrap().get("name").is_none());
        let (path, _) = tool_request("connection_test", &json!({"name":"kitbash"})).unwrap();
        assert_eq!(path, "/v1/connections/kitbash/test");
        let (path, body) = tool_request(
            "profile_attach_connection",
            &json!({"profile":"coder","connection":"kitbash","run_kinds":["debug"]}),
        )
        .unwrap();
        assert_eq!(path, "/v1/profiles/coder/connections/attach");
        assert_eq!(body.unwrap()["run_kinds"], json!(["debug"]));
        let (path, body) = tool_request(
            "catalog_apply",
            &json!({"document":{"schema":"enginebox.catalog/v1"},"dry_run":true}),
        )
        .unwrap();
        assert_eq!(path, "/v1/catalog/apply");
        assert_eq!(body.unwrap()["dry_run"], true);
        // Names cannot traverse paths; unknown arguments are refused.
        assert!(tool_request("connection_delete", &json!({"name":"../profiles"})).is_err());
        let (path, body) = tool_request(
            "repo_requirement_propose_mapping",
            &json!({"project":"alpha","name":"site","repo_profile":"deploy","requirement":"CLOUDFLARE_API_TOKEN","connection":"cf-deploy"}),
        )
        .unwrap();
        assert_eq!(
            path,
            "/v1/projects/alpha/repositories/site/requirements/propose"
        );
        assert!(body.as_ref().unwrap().get("project").is_none());
        let (path, _) = tool_request(
            "repo_requirements_get",
            &json!({"project":"alpha","name":"site"}),
        )
        .unwrap();
        assert_eq!(path, "/v1/projects/alpha/repositories/site/requirements");
        // There is no approve tool: approval is an owner/admin action in the dashboard or API.
        assert!(tool_request("repo_requirement_approve", &json!({})).is_err());
        assert!(tool_request("profile_get", &json!({"profile":"Coder"})).is_err());
        assert!(
            tool_request(
                "profile_detach_connection",
                &json!({"profile":"coder","connection":"kitbash","run_kinds":["x"]})
            )
            .is_err()
        );
    }
    #[test]
    fn workspace_source_and_pool_tools_preserve_the_controller_boundaries() {
        assert_eq!(
            tool_request("workspace_catalog", &json!({})).unwrap(),
            ("/v1/workspaces".into(), None)
        );
        assert_eq!(
            tool_request(
                "repository_create",
                &json!({"project":"app","name":"main","description":"Fixture"})
            )
            .unwrap(),
            (
                "/v1/projects/app/repositories".into(),
                Some(json!({"name":"main","description":"Fixture"}))
            )
        );
        assert_eq!(
            tool_request(
                "repository_link_github",
                &json!({"project":"app","name":"main","full_name":"Owner/Repo"})
            )
            .unwrap(),
            (
                "/v1/projects/app/repositories/github".into(),
                Some(json!({"name":"main","full_name":"owner/repo"}))
            )
        );
        let request = json!({"pool":"app-pool","provider":"codex","prompt":"Inspect source","repository":"app/main","timeout_seconds":3600});
        let (path, body) = tool_request("pool_agent_launch", &request).unwrap();
        assert_eq!(path, "/v1/pools/app-pool/agent-runs");
        let body = body.unwrap();
        assert!(body.get("pool").is_none());
        assert_eq!(body["repository"], "app/main");
        assert_eq!(body["timeout_seconds"], 3600);
        assert_eq!(
            tool_request(
                "pool_agent_cancel",
                &json!({"pool":"app-pool","run_id":"a".repeat(32)})
            )
            .unwrap(),
            (
                format!("/v1/pools/app-pool/agent-runs/{}/cancel", "a".repeat(32)),
                Some(json!({}))
            )
        );
        for (tool, args) in [
            ("workspace_catalog", json!({"token":"secret"})),
            ("repository_list", json!({"project":"../other"})),
            (
                "repository_create",
                json!({"project":"app","name":"main.git"}),
            ),
            (
                "repository_create",
                json!({"project":"app","name":"main","description":"bad\ntext"}),
            ),
            (
                "repository_link_github",
                json!({"project":"app","name":"main","full_name":"https://user:secret@github.com/owner/repo"}),
            ),
            (
                "pool_agent_launch",
                json!({"pool":"app-pool","provider":"codex","prompt":"Inspect","repository":"app/../other"}),
            ),
            (
                "pool_agent_launch",
                json!({"pool":"app-pool","provider":"codex","prompt":"Inspect","timeout_seconds":0}),
            ),
            (
                "pool_agent_run",
                json!({"pool":"app-pool","run_id":"../private"}),
            ),
        ] {
            assert!(
                tool_request(tool, &args).is_err(),
                "{tool} accepted invalid inputs"
            );
        }
    }
    #[test]
    fn rejects_token_exfiltration_and_path_injection() {
        let token = "x".repeat(32);
        assert!(FleetClient::new("http://example.com", token.clone()).is_err());
        assert!(FleetClient::new("https://user:pass@example.com", token.clone()).is_err());
        assert!(FleetClient::new("https://example.com/path", token).is_err());
        assert!(
            tool_request(
                "fleet_action",
                &json!({"host_id":"a/../b","name":"ebox-test","action":"resume"})
            )
            .is_err()
        );
        assert!(
            tool_request(
                "template_register",
                &json!({"manifest":{},"url":"https://example.com"})
            )
            .is_err()
        );
    }
    #[test]
    fn tools_preserve_native_resume_and_allocation_contracts() {
        let (_,body)=tool_request("agent_launch",&json!({"host_id":"worker","name":"ebox-test","provider":"codex","prompt":"test","resume_id":"12345678-1234-1234-1234-123456789012"})).unwrap();
        assert_eq!(
            body.unwrap()["resume_id"],
            "12345678-1234-1234-1234-123456789012"
        );
        let (path,body)=tool_request("fleet_allocate",&json!({"host_id":"worker","name":"ebox-test","template":"native-dev","resources":{"cpu":2,"memory_gib":4,"disk_gib":40}})).unwrap();
        assert_eq!(path, "/v1/hosts/worker/allocations");
        assert!(body.unwrap().get("size").is_none());
    }
    #[test]
    fn mcp_handshake_has_tools_and_notifications_have_no_reply() {
        let client = FleetClient::new("http://127.0.0.1:1", "x".repeat(32)).unwrap();
        assert_eq!(rpc(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}),&client).unwrap()["result"]["capabilities"],json!({"tools":{}}));
        assert!(
            rpc(
                json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
                &client
            )
            .is_none()
        );
        assert_eq!(
            rpc(
                json!({"jsonrpc":"2.0","id":"list","method":"tools/list"}),
                &client
            )
            .unwrap()["result"]["tools"]
                .as_array()
                .unwrap()
                .len(),
            58
        );
    }
    #[test]
    fn github_tools_expose_review_promotion_and_fenced_resolution_without_credentials() {
        let (path,body)=tool_request("storage_github_preview",&json!({"host_id":"worker","project":"app","repository":"https://github.com/Owner/App.git"})).unwrap();
        assert_eq!(path, "/v1/storage/hosts/worker/github-preview");
        assert_eq!(body.unwrap()["repository"], "owner/app");
        let (path, body) = tool_request(
            "storage_github_delivery",
            &json!({"host_id":"worker","project":"app","action":"resolve","attempt":"sync-a"}),
        )
        .unwrap();
        assert_eq!(path, "/v1/storage/hosts/worker/github-delivery");
        assert_eq!(body.unwrap()["attempt"], "sync-a");
        for (tool, args) in [
            (
                "storage_github_preview",
                json!({"host_id":"worker","project":"app","repository":"a/b","token":"synthetic-secret"}),
            ),
            (
                "storage_github_promote",
                json!({"host_id":"worker","project":"app","name":"agent","expected_main":"HEAD"}),
            ),
            (
                "storage_github_delivery",
                json!({"host_id":"worker","project":"app","action":"force-push","attempt":"sync-a"}),
            ),
            (
                "storage_github_settings",
                json!({"host_id":"worker","project":"app","enabled":"true"}),
            ),
        ] {
            assert!(tool_request(tool, &args).is_err());
        }
    }
    #[test]
    fn storage_tools_preserve_review_and_do_not_expose_shell_or_paths() {
        for (tool, operation) in [
            ("storage_dev_start", "dev-start"),
            ("storage_dev_status", "dev-status"),
        ] {
            let (path, body) =
                tool_request(tool, &json!({"host_id":"worker","project":"app"})).unwrap();
            assert_eq!(path, format!("/v1/storage/hosts/worker/{operation}"));
            assert_eq!(body.unwrap(), json!({"project":"app"}));
            assert!(
                tool_request(
                    tool,
                    &json!({"host_id":"worker","project":"app","image":"untrusted"})
                )
                .is_err()
            );
        }
        let (path, body) = tool_request("storage_prepare", &json!({"host_id":"worker","project":"game","name":"ebox-agent","template":"native","size":"minimum"})).unwrap();
        assert_eq!(path, "/v1/storage/hosts/worker/prepare");
        assert_eq!(body.unwrap()["name"], "ebox-agent");
        assert!(
            tool_request(
                "storage_prepare",
                &json!({"host_id":"worker","project":"game","name":"agent","template":"native"})
            )
            .is_err()
        );
        let (path, body) = tool_request(
            "storage_preview",
            &json!({"host_id":"worker","project":"game"}),
        )
        .unwrap();
        assert_eq!(path, "/v1/storage/hosts/worker/preview");
        assert_eq!(body.unwrap(), json!({"project":"game","revision":"HEAD"}));
        let (_, body) = tool_request(
            "storage_fork",
            &json!({"host_id":"worker","project":"game","name":"agent-a","from":"prepared"}),
        )
        .unwrap();
        assert_eq!(body.unwrap()["from"], "prepared");
        for (name, args) in [
            (
                "storage_preview",
                json!({"host_id":"worker","project":"game","repo":"/root"}),
            ),
            (
                "storage_import",
                json!({"host_id":"worker","project":"game","revision":"HEAD","approval":"a".repeat(64),"credential_free":true}),
            ),
            (
                "storage_import",
                json!({"host_id":"worker","project":"game","revision":"b".repeat(40),"approval":"a".repeat(64),"credential_free":false}),
            ),
            (
                "storage_action",
                json!({"host_id":"worker","name":"agent-a","action":"exec"}),
            ),
            (
                "storage_action",
                json!({"host_id":"worker","name":"agent-a","action":"recover","message":"force"}),
            ),
            (
                "storage_action",
                json!({"host_id":"worker","name":"../escape","action":"close"}),
            ),
        ] {
            assert!(tool_request(name, &args).is_err(), "{name}: {args}");
        }
    }
}
