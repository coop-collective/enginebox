//! Generating a repository's `.enginebox/config.json` (docs/PROJECT-CONFIG.md,
//! "Generating a config"): an owner or admin starts an agent run on the
//! repository with a fixed prompt; the agent pushes the file to the
//! [`BRANCH`] branch; for a GitHub repository the controller then opens a
//! pull request, and for a hosted one the branch waits for review. Nothing is
//! merged automatically: the config only takes effect once it is on the
//! default branch, where the controller reads it before each run.

use anyhow::Result;
use enginebox_core::state;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The branch the agent pushes the generated config to.
pub const BRANCH: &str = "enginebox/agent-config";
/// The run kind of a generation run.
pub const RUN_KIND: &str = "config";
/// How long a generation run may take.
pub const TIMEOUT_SECONDS: u64 = 1800;
/// How long after a refused pull request the controller asks GitHub again.
const PR_RETRY_SECONDS: i64 = 300;

/// The fixed task; never taken from the request.
pub const PROMPT: &str = r#"Write this repository's Engine Box agent config, .enginebox/config.json (schema_version 2), plus .enginebox/templates/dev.json when step 3b applies, and push it to the branch enginebox/agent-config. Change nothing else.

1. git clone "$ENGINEBOX_REPO_REMOTE" "$ENGINEBOX_RUN_DIR" (the directory is empty), cd into it, and git checkout -B enginebox/agent-config. If .enginebox/config.json exists, keep its dev section and profiles and only fix or complete them.
2. Inspect the repository: package manager and lockfile (pnpm-lock.yaml, package-lock.json, yarn.lock, bun.lockb, Cargo.lock, uv.lock...), workspace layout, package.json scripts, wrangler.toml/wrangler.jsonc, test runner config, README and CI workflows. Prefer the commands CI already runs.
3. Write the file:
{"schema_version": 2, "agents": {"profiles": {"default": {"default": true, "description": "...", "template": "native-dev", "setup": [...], "test": [...], "dev": [...], "env": {...}, "secrets": [...], "mcp": [...]}}}}
- setup, test and dev are argv arrays (no shell strings), for example ["pnpm", "install", "--frozen-lockfile"], ["pnpm", "test"], ["pnpm", "dev"]. Omit dev when the repository has no dev server; omit test only when it has no tests.
- env holds non-secret values only, such as {"NODE_ENV": "development"}. Never put a secret, token, password or URL with credentials anywhere in the file.
- secrets declares requirements by NAME only: {"name": "DATABASE_URL", "qualifier": "staging", "run_kinds": ["builder"], "description": "why it is needed"}. Use qualifiers like staging, read-only, production-read-only or deploy. List only variables the code or its config actually reads (.dev.vars.example, .env.example, wrangler bindings, process.env / env.X uses). Do not mark any requirement required.
- mcp lists catalog connection names only (for example "kitbash") and only when the repository clearly uses that service; usually leave it out.
- Keep it short: one default profile; add a "debug" profile with read-only secrets only if the repository is a deployed service.
3b. If the repository has a dev server that runs without real secrets (a vinext/Vite app, wrangler dev --local, a Node server), also add a branch preview template: "dev": {"template": "./templates/dev.json"} at the top level of config.json, and write .enginebox/templates/dev.json:
{"schema_version": 1, "kind": "dev", "id": "dev", "revision": 1, "image": "node@sha256:<digest>", "prepare": ["pnpm", "install", "--frozen-lockfile"], "command": ["pnpm", "dev", "--host", "0.0.0.0"], "installInputs": ["package.json", "pnpm-lock.yaml"], "ports": [{"name": "web", "protocol": "http", "port": 5173, "expose": true}], "health": {"port": "web", "path": "/"}, "writableDirs": ["node_modules/.vite"], "memoryMiB": 2048}
- The image must be pinned by digest: get one with TOKEN=$(curl -s "https://auth.docker.io/token?service=registry.docker.io&scope=repository:library/node:pull" | node -pe 'JSON.parse(require("fs").readFileSync(0)).token') then curl -sI -H "Authorization: Bearer $TOKEN" -H "Accept: application/vnd.oci.image.index.v1+json" https://registry-1.docker.io/v2/library/node/manifests/24-bookworm-slim and copy the docker-content-digest header. If you cannot get a digest, leave the dev section out.
- installInputs lists every file dependency installation reads (each package.json of a workspace and the lockfile). The dev command must listen on 0.0.0.0 at the declared port; application data goes under $ENGINEBOX_DATA_DIR (for wrangler: --local --persist-to /data/wrangler). If the dev server needs real secrets, leave the dev section out.
- writableDirs names only caches the dev server writes (node_modules/.vite, node_modules/.cache, .wrangler), never a whole node_modules: that would hide what prepare installed, and the template is refused.
- Environments are tuned for pnpm (a store shared across the project). With a pnpm-lock.yaml, use pnpm throughout. With another lockfile, keep that package manager so the lockfile is honoured, and say in your summary that the repository would install faster on pnpm.
4. Check it parses: node -e 'JSON.parse(require("fs").readFileSync(".enginebox/config.json","utf8"))' (or python3 -m json.tool .enginebox/config.json). Fix any error.
5. git add .enginebox, git commit -m "Add Engine Box agent config", git push --force-with-lease origin enginebox/agent-config. Push to no other branch.
6. Finish with a short summary: the commands you chose and why, and each secret requirement with where the code reads it."#;

/// One generation of a repository's config.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Generation {
    pub repository: String,
    pub org_id: String,
    pub run_id: String,
    pub pool: String,
    pub provider: String,
    pub started_by: String,
    pub started_at: String,
    /// `running`, `succeeded` or `failed`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    /// The branch holding the config once the run succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_attempted_at: Option<String>,
}

/// Generation records under `<root>/repo-generations/`, one per repository.
#[derive(Clone)]
pub struct Generations {
    root: PathBuf,
}
impl Generations {
    pub fn open(root: &Path) -> Self {
        Self {
            root: root.join("repo-generations"),
        }
    }
    fn path(&self, repository: &str) -> PathBuf {
        self.root
            .join(format!("{}.json", crate::repo_config::file_key(repository)))
    }
    pub fn get(&self, org: &str, repository: &str) -> Option<Generation> {
        state::read_json(&self.path(repository))
            .ok()
            .and_then(|v| serde_json::from_value::<Generation>(v).ok())
            .filter(|g| g.org_id == org)
    }
    pub fn put(&self, generation: &Generation) -> Result<()> {
        state::write_json(
            &self.path(&generation.repository),
            &serde_json::to_value(generation)?,
        )
    }
}

/// A short reason for a run that did not succeed, from its record.
fn failure_of(run: &Value) -> String {
    let status = run["status"].as_str().unwrap_or("failed");
    let detail = run["failure"]["reason"]
        .as_str()
        .or_else(|| run["failure"]["message"].as_str())
        .map(str::to_owned)
        .or_else(|| {
            run["result"]["exit_code"]
                .as_i64()
                .map(|c| format!("exit code {c}"))
        })
        .or_else(|| (run["result"]["timed_out"] == true).then(|| "timed out".to_owned()));
    match detail {
        Some(d) => format!("The run {status}: {d}"),
        None => format!("The run {status}"),
    }
    .chars()
    .take(200)
    .collect()
}

/// The message for a refused pull request: GitHub's status code only (its
/// bodies are never echoed), with the permission hint where it applies.
fn pr_error(error: &anyhow::Error) -> String {
    let text = format!("{error:#}");
    if text.contains(enginebox_github::pulls::PERMISSION_HINT)
        || text.contains("HTTP 403")
        || text.contains("HTTP 404")
    {
        return format!(
            "{}. Then open the generation again to retry.",
            enginebox_github::pulls::PERMISSION_HINT
        );
    }
    if text.contains("HTTP 422") {
        return format!(
            "GitHub refused the pull request (HTTP 422): the branch {BRANCH} may be missing or identical to the default branch."
        );
    }
    "GitHub did not open the pull request; it is retried shortly.".into()
}

/// Bring `g` up to date: settle a running generation from its run (`run`
/// reads it), then for a succeeded one open the pull request (`open_pr`;
/// `Ok(None)` for a repository without pull requests). Returns whether `g`
/// changed. A refused pull request is asked again after five minutes.
pub fn reconcile(
    g: &mut Generation,
    run: impl FnOnce() -> Result<Value>,
    open_pr: impl FnOnce() -> Result<Option<Value>>,
) -> bool {
    let before = g.clone();
    if g.state == "running" {
        match run() {
            Ok(run) => match run["status"].as_str() {
                Some("succeeded") => {
                    g.state = "succeeded".into();
                    g.branch = Some(BRANCH.into());
                }
                Some("queued" | "running" | "starting") | None => {}
                Some(_) => {
                    g.state = "failed".into();
                    g.failure = Some(failure_of(&run));
                }
            },
            // The run record is gone (history is bounded): nothing to settle from.
            Err(_) => {
                g.state = "failed".into();
                g.failure = Some("The run is no longer known to the controller.".into());
            }
        }
    }
    let now = chrono::Utc::now();
    let retry_due = g
        .pr_attempted_at
        .as_deref()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .is_none_or(|t| (now - t.with_timezone(&chrono::Utc)).num_seconds() >= PR_RETRY_SECONDS);
    if g.state == "succeeded" && g.pr_url.is_none() && retry_due {
        g.pr_attempted_at = Some(now.to_rfc3339());
        match open_pr() {
            Ok(Some(pr)) => {
                g.pr_url = pr["url"].as_str().map(str::to_owned);
                g.pr_number = pr["number"].as_u64();
                g.pr_error = None;
            }
            // No pull requests on this backend: the branch waits for review.
            Ok(None) => {}
            Err(error) => g.pr_error = Some(pr_error(&error)),
        }
    }
    *g != before
}

/// The pull request's description.
pub fn pr_body(run_id: &str) -> String {
    format!(
        "Engine Box generated this repository's agent config (`.enginebox/config.json`, schema version 2) in run `{run_id}`.\n\nIt declares the setup, test and dev commands agents use and the secret *names* runs may need. It contains no values: an org owner or admin approves which connection satisfies each requirement on the dashboard's Connections page. Review the commands and requirements, then merge; runs read the config from the default branch."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn generation() -> Generation {
        Generation {
            repository: "alpha/site".into(),
            org_id: "o".into(),
            run_id: "0123456789abcdef0123456789abcdef".into(),
            pool: "workers".into(),
            provider: "codex".into(),
            started_by: "owner".into(),
            started_at: "2026-10-29T00:00:00Z".into(),
            state: "running".into(),
            failure: None,
            branch: None,
            pr_url: None,
            pr_number: None,
            pr_error: None,
            pr_attempted_at: None,
        }
    }

    #[test]
    fn the_prompt_is_fixed_and_short() {
        assert!(PROMPT.len() < 5000, "{}", PROMPT.len());
        assert!(PROMPT.contains(BRANCH) && PROMPT.contains("schema_version"));
    }

    #[test]
    fn a_running_generation_settles_and_opens_its_pull_request_once() {
        let mut g = generation();
        assert!(!reconcile(
            &mut g,
            || Ok(json!({"status":"running"})),
            || panic!("no PR yet")
        ));
        let changed = reconcile(
            &mut g,
            || Ok(json!({"status":"succeeded"})),
            || {
                Ok(Some(
                    json!({"number":3,"url":"https://github.com/o/r/pull/3"}),
                ))
            },
        );
        assert!(changed);
        assert_eq!(
            (g.state.as_str(), g.branch.as_deref(), g.pr_number),
            ("succeeded", Some(BRANCH), Some(3))
        );
        assert!(!reconcile(
            &mut g,
            || panic!("settled"),
            || panic!("opened")
        ));
    }

    #[test]
    fn refusals_and_failures_are_explained() {
        let mut g = generation();
        reconcile(
            &mut g,
            || Ok(json!({"status":"succeeded"})),
            || anyhow::bail!("GitHub request failed (HTTP 403)"),
        );
        assert!(
            g.pr_error
                .as_deref()
                .unwrap()
                .contains("Pull requests: read and write")
        );
        // Not asked again within five minutes.
        assert!(!reconcile(&mut g, || unreachable!(), || panic!("too soon")));
        g.pr_attempted_at = Some("2026-01-01T00:00:00Z".into());
        reconcile(&mut g, || unreachable!(), || Ok(None));
        assert!(g.pr_url.is_none());

        let mut f = generation();
        reconcile(
            &mut f,
            || Ok(json!({"status":"failed","result":{"exit_code":2}})),
            || unreachable!(),
        );
        assert_eq!(
            (f.state.as_str(), f.failure.as_deref()),
            ("failed", Some("The run failed: exit code 2"))
        );
    }

    #[test]
    fn records_are_per_repository_and_org() {
        let t = tempfile::tempdir().unwrap();
        let store = Generations::open(t.path());
        store.put(&generation()).unwrap();
        assert_eq!(store.get("o", "alpha/site"), Some(generation()));
        assert_eq!(store.get("other", "alpha/site"), None);
    }
}
