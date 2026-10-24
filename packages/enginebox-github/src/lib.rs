//! Optional GitHub integration. The runtime has no dependency on this crate.
pub mod api;
pub mod ci;
mod config;
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use enginebox_core::state;
use ring::{
    digest,
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct App {
    pub client_id: String,
    pub client_secret: String,
    pub slug: String,
    pub private_key: String,
    pub webhook_secret: String,
    #[serde(default)]
    pub workflows_write: bool,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema: u32,
    pub callback_url: String,
    pub repos: App,
    pub runners: Option<App>,
    #[serde(default)]
    pub pools: Vec<ci::Pool>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Repository {
    pub id: u64,
    pub installation_id: u64,
    pub full_name: String,
    pub private: bool,
    pub branch: String,
}
#[derive(Default, Serialize, Deserialize)]
pub(crate) struct Data {
    #[serde(default)]
    pub accounts: BTreeMap<String, Value>,
    #[serde(default)]
    pub flows: BTreeMap<String, Value>,
    #[serde(default)]
    pub repositories: BTreeMap<u64, Repository>,
    #[serde(default)]
    pub jobs: BTreeMap<String, Value>,
    #[serde(default)]
    pub draining: bool,
}
#[derive(Clone)]
pub struct GitHub {
    pub(crate) config: Option<Config>,
    pub(crate) root: PathBuf,
    pub(crate) data: Arc<Mutex<Data>>,
    pub(crate) api: api::Api,
    pub(crate) pending: Arc<AtomicUsize>,
}
pub fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key].as_str().context("Missing text field")
}
pub fn number(v: &Value, key: &str) -> Result<u64> {
    v[key]
        .as_u64()
        .filter(|v| *v > 0)
        .context("Missing positive ID")
}
pub fn name(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 100
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            && value != "."
            && value != "..",
        "Invalid GitHub name"
    );
    Ok(())
}
pub fn full_name(value: &str) -> Result<()> {
    let (owner, repo) = value.split_once('/').context("Expected owner/repo")?;
    name(owner)?;
    name(repo)
}
fn random() -> Result<String> {
    let mut bytes = [0; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("Random source unavailable"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
impl GitHub {
    // Called only under the active controller ownership lease.
    pub fn open(controller_root: &Path) -> Result<Self> {
        let root = controller_root.join("github");
        let config_path = controller_root.join("github.json");
        let config: Option<Config> = if config_path.exists() {
            Some(config::load(&config_path)?)
        } else {
            None
        };
        if let Some(c) = &config {
            ensure!(c.schema == 1, "Unsupported GitHub schema");
            let callback = url::Url::parse(&c.callback_url)?;
            ensure!(
                callback.scheme() == "https"
                    || (callback.scheme() == "http"
                        && matches!(callback.host_str(), Some("localhost" | "127.0.0.1"))),
                "GitHub callback requires HTTPS"
            );
            ensure!(
                callback.username().is_empty()
                    && callback.password().is_none()
                    && callback.query().is_none()
                    && callback.fragment().is_none(),
                "Invalid callback URL"
            );
            for app in [&c.repos, c.runners.as_ref().unwrap_or(&c.repos)] {
                name(&app.slug)?;
                ensure!(
                    !app.client_id.is_empty()
                        && !app.client_secret.is_empty()
                        && app.webhook_secret.len() >= 32,
                    "Incomplete GitHub App configuration"
                );
            }
            ensure!(
                c.runners
                    .as_ref()
                    .is_none_or(|r| r.client_id != c.repos.client_id),
                "Runner permissions require a separate GitHub App"
            );
            ci::validate_pools(&c.pools)?;
        }
        let mut data: Data = if config.is_some() {
            state::private_dir(&root)?;
            if root.join("state.json").exists() {
                serde_json::from_value(state::read_json(&root.join("state.json"))?)?
            } else {
                Data::default()
            }
        } else {
            Data::default()
        };
        for job in data.jobs.values_mut() {
            if job["status"] == "running" {
                job["status"] = json!("uncertain");
                job["error"] = json!(
                    "Controller restarted; inspect runner and worker receipts before recovery. No job replayed."
                );
            }
        }
        let service = Self {
            config,
            root,
            data: Arc::new(Mutex::new(data)),
            api: api::Api::new()?,
            pending: Arc::new(AtomicUsize::new(0)),
        };
        if service.config.is_some() {
            service.save(&service.data.lock().unwrap())?;
        }
        Ok(service)
    }
    pub(crate) fn save(&self, data: &Data) -> Result<()> {
        state::write_json(&self.root.join("state.json"), &serde_json::to_value(data)?)
    }
    pub(crate) fn app(&self, kind: &str) -> Result<&App> {
        let config = self.config.as_ref().context("GitHub is not configured")?;
        match kind {
            "repos" => Ok(&config.repos),
            "runners" => config
                .runners
                .as_ref()
                .context("Runner App is not configured"),
            _ => anyhow::bail!("Unknown GitHub connection"),
        }
    }
    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::SeqCst)
    }
    pub fn status(&self) -> Value {
        let data = self.data.lock().unwrap();
        json!({"configured": self.config.is_some(), "runner_app_configured":self.config.as_ref().is_some_and(|c|c.runners.is_some()),
            "accounts":data.accounts.iter().map(|(kind, a)|json!({"kind":kind,"login":a["login"]})).collect::<Vec<_>>(),
            "repositories":data.repositories.values().collect::<Vec<_>>(), "jobs":data.jobs.values().collect::<Vec<_>>(), "draining":data.draining,
            "pools":self.config.as_ref().map(|c|c.pools.iter().map(|p|json!({"id":p.id,"organization":p.organization,"labels":p.labels,"runner_group_id":p.runner_group_id,"max_runners":p.max_runners,"preparation":p.preparation.as_ref().map(|v|json!({"repository_id":v.repository_id,"workflows":v.workflows}))})).collect::<Vec<_>>()).unwrap_or_default()})
    }
    pub fn connect(&self, kind: &str) -> Result<Value> {
        let app = self.app(kind)?;
        let state_id = random()?;
        let verifier = random()?;
        let challenge =
            URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, verifier.as_bytes()).as_ref());
        let mut data = self.data.lock().unwrap();
        data.flows
            .retain(|_, v| v["expires"].as_u64().unwrap_or(0) > api::now());
        ensure!(data.flows.len() < 16, "Too many authorization attempts");
        data.flows.insert(
            state_id.clone(),
            json!({"kind":kind,"verifier":verifier,"expires":api::now()+600}),
        );
        self.save(&data)?;
        let mut url = url::Url::parse("https://github.com/login/oauth/authorize")?;
        url.query_pairs_mut().extend_pairs([
            ("client_id", app.client_id.as_str()),
            ("redirect_uri", &self.config.as_ref().unwrap().callback_url),
            ("state", &state_id),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("scope", "offline_access"),
        ]);
        Ok(json!({"url":url.as_str(),"state":state_id}))
    }
    pub fn callback(&self, state_id: &str, code: &str) -> Result<Value> {
        ensure!(
            code.len() <= 1024 && !code.is_empty(),
            "Invalid authorization code"
        );
        let mut data = self.data.lock().unwrap();
        let flow = data
            .flows
            .remove(state_id)
            .context("Authorization expired or already used")?;
        self.save(&data)?; // consume before exchange, including failure
        ensure!(
            flow["expires"].as_u64().unwrap_or(0) > api::now(),
            "Authorization expired"
        );
        let kind = text(&flow, "kind")?;
        let app = self.app(kind)?;
        let token = self.api.oauth(json!({"client_id":app.client_id,"client_secret":app.client_secret,"code":code,"redirect_uri":self.config.as_ref().unwrap().callback_url,"code_verifier":flow["verifier"]}))?;
        let access = text(&token, "access_token").context("GitHub authorization rejected")?;
        let user = self.api.call("GET", "/user", access, None)?;
        // Reconnecting as a different principal must not inherit selected repos/jobs.
        if let Some(old) = data.accounts.get(kind) {
            ensure!(
                old["id"] == user["id"],
                "Disconnect the existing account before switching users"
            );
        }
        let mut account = normalize_token(token)?;
        account["id"] = user["id"].clone();
        account["login"] = user["login"].clone();
        data.accounts.insert(kind.into(), account);
        self.save(&data)?;
        Ok(json!({"connected":true,"kind":kind,"login":user["login"]}))
    }
    fn user_token(&self, kind: &str, data: &mut Data) -> Result<String> {
        let account = data
            .accounts
            .get_mut(kind)
            .context("Connect GitHub first")?;
        if account["expires_at"].as_u64().unwrap_or(0) <= api::now() + 60 {
            let app = self.app(kind)?;
            let response = self.api.oauth(json!({"client_id":app.client_id,"client_secret":app.client_secret,"grant_type":"refresh_token","refresh_token":text(account,"refresh_token")?}))?;
            let mut next = normalize_token(response)?;
            next["id"] = account["id"].clone();
            next["login"] = account["login"].clone();
            *account = next;
            self.save(data)?;
        }
        Ok(text(&data.accounts[kind], "access_token")?.into())
    }
    pub(crate) fn installations(&self, kind: &str, data: &mut Data) -> Result<Vec<Value>> {
        let token = self.user_token(kind, data)?;
        self.api
            .pages("/user/installations", "installations", &token)
    }
    pub fn repositories(&self, installation: u64) -> Result<Value> {
        let mut data = self.data.lock().unwrap();
        let token = self.user_token("repos", &mut data)?;
        let repos = self.api.pages(
            &format!("/user/installations/{installation}/repositories"),
            "repositories",
            &token,
        )?;
        Ok(
            json!({"repositories":repos.iter().map(|r|json!({"id":r["id"],"full_name":r["full_name"],"private":r["private"],"default_branch":r["default_branch"],"permissions":r["permissions"]})).collect::<Vec<_>>()}),
        )
    }
    pub fn list_installations(&self, kind: &str) -> Result<Value> {
        let mut data = self.data.lock().unwrap();
        let rows = self.installations(kind, &mut data)?;
        Ok(
            json!({"installations":rows.iter().map(|i|json!({"id":i["id"],"account":i["account"]["login"],"permissions":i["permissions"],"suspended":!i["suspended_at"].is_null()})).collect::<Vec<_>>(),"install_url":format!("https://github.com/apps/{}/installations/new",self.app(kind)?.slug)}),
        )
    }
    pub fn select(&self, installation: u64, repository: u64, branch: &str) -> Result<Value> {
        enginebox_storage_layerfs::github::branch(branch)?;
        let available = self.repositories(installation)?;
        let repo = available["repositories"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == repository)
            .context("Repository is not accessible through this installation")?;
        ensure!(
            repo["permissions"]["push"] == true,
            "Two-way sync requires repository write access"
        );
        let selected = Repository {
            id: repository,
            installation_id: installation,
            full_name: text(repo, "full_name")?.into(),
            private: repo["private"] == true,
            branch: branch.into(),
        };
        full_name(&selected.full_name)?;
        let mut data = self.data.lock().unwrap();
        if let Some(old) = data.repositories.get(&repository) {
            ensure!(
                old.branch == selected.branch
                    && old.full_name == selected.full_name
                    && old.installation_id == installation,
                "Remove the selection before changing its binding"
            );
        }
        ensure!(
            data.repositories.len() < 200 || data.repositories.contains_key(&repository),
            "Repository limit reached"
        );
        data.repositories.insert(repository, selected.clone());
        self.save(&data)?;
        Ok(json!({"repository":selected}))
    }
    pub fn remove(&self, repository: u64) -> Result<Value> {
        let _lock = state::lock(&self.root.join(format!("repo-{repository}.lock")), false)?;
        let mut data = self.data.lock().unwrap();
        ensure!(
            !data.jobs.values().any(|j| j["repository_id"] == repository
                && matches!(
                    j["status"].as_str(),
                    Some("queued" | "running" | "uncertain")
                )),
            "Drain and resolve repository jobs first"
        );
        data.repositories.remove(&repository);
        self.save(&data)?;
        Ok(json!({"removed":true,"checkout_retained":true}))
    }
    pub fn disconnect(&self, kind: &str) -> Result<Value> {
        self.app(kind)?;
        let mut data = self.data.lock().unwrap();
        ensure!(
            !data.jobs.values().any(|j| matches!(
                j["status"].as_str(),
                Some("queued" | "running" | "uncertain")
            )),
            "Drain and resolve CI jobs before disconnecting"
        );
        data.accounts.remove(kind);
        data.flows.retain(|_, v| v["kind"] != kind);
        if kind == "repos" {
            data.repositories.clear();
        }
        self.save(&data)?;
        Ok(json!({"disconnected":true,"checkouts_retained":true}))
    }
    pub fn configured(&self) -> bool {
        self.config.is_some()
    }

    /// Private transport-only credential. Never expose through HTTP or job results.
    /// Validate the connected owner's grant before minting a single-repository token.
    pub fn transfer_credential(
        &self,
        repository: &str,
        branch: Option<&str>,
        write: bool,
    ) -> Result<Value> {
        full_name(repository)?;
        let mut data = self.data.lock().unwrap();
        let selected = data
            .repositories
            .values()
            .find(|r| r.full_name == repository)
            .context("Select this repository in the GitHub panel first")?
            .clone();
        ensure!(
            branch.is_none_or(|b| b == selected.branch),
            "Branch differs from the selected GitHub binding"
        );
        let user = self.user_token("repos", &mut data)?;
        let repos = self.api.pages(
            &format!(
                "/user/installations/{}/repositories",
                selected.installation_id
            ),
            "repositories",
            &user,
        )?;
        ensure!(
            repos.iter().any(|r| r["id"] == selected.id
                && r["full_name"] == selected.full_name
                && (!write || r["permissions"]["push"] == true)),
            "Repository grant changed; reconnect or reselect"
        );
        let app = self.app("repos")?;
        let mut permissions = json!({"contents": if write { "write" } else { "read" }});
        if write && app.workflows_write {
            permissions["workflows"] = json!("write");
        }
        let response = self.api.call(
            "POST",
            &format!(
                "/app/installations/{}/access_tokens",
                selected.installation_id
            ),
            &api::jwt(app)?,
            Some(json!({"repository_ids":[selected.id], "permissions":permissions})),
        )?;
        let token = text(&response, "token")?;
        ensure!(
            !token.is_empty()
                && token.len() <= 16384
                && token
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)),
            "Invalid installation token"
        );
        Ok(
            json!({"schema":1,"repository":selected.full_name,"branch":selected.branch,"token":token}),
        )
    }
}
fn normalize_token(mut token: Value) -> Result<Value> {
    ensure!(
        text(&token, "access_token")?.len() >= 8,
        "Missing GitHub access token"
    );
    let expiry = token["expires_in"]
        .as_u64()
        .filter(|s| *s > 60 && *s <= 86400)
        .context("Enable expiring GitHub App user tokens")?;
    text(&token, "refresh_token")?;
    token["expires_at"] = json!(api::now() + expiry);
    Ok(token)
}

#[cfg(test)]
mod tests;
