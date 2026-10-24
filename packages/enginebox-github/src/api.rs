use anyhow::{Context, Result, ensure};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use reqwest::blocking::Client;
use ring::{
    rand::SystemRandom,
    signature::{RSA_PKCS1_SHA256, RsaKeyPair},
};
use serde_json::{Value, json};
use std::{
    io::Read,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const API_VERSION: &str = "2026-03-10";
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[derive(Clone)]
pub struct Api {
    base: String,
    oauth: String,
}
impl Api {
    pub fn new() -> Result<Self> {
        Ok(Self {
            base: "https://api.github.com".into(),
            oauth: "https://github.com/login/oauth/access_token".into(),
        })
    }
    fn client(&self) -> Result<Client> {
        Ok(Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("EngineBox-GitHub/0.3")
            .build()?)
    }
    #[cfg(test)]
    pub fn fixture(base: String) -> Self {
        let mut api = Self::new().unwrap();
        api.oauth = format!("{base}/oauth");
        api.base = base;
        api
    }
    fn decode(response: reqwest::blocking::Response) -> Result<Value> {
        let status = response.status();
        // Provider bodies may contain credentials; never propagate them in errors.
        ensure!(
            status.is_success(),
            "GitHub request failed (HTTP {})",
            status.as_u16()
        );
        let mut bytes = vec![];
        response.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 4 * 1024 * 1024, "GitHub response too large");
        serde_json::from_slice(&bytes).context("Invalid GitHub response")
    }
    pub fn call(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        ensure!(
            path.starts_with('/') && !path.starts_with("//"),
            "Invalid GitHub path"
        );
        let mut request = self
            .client()?
            .request(method.parse()?, format!("{}{path}", self.base))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .bearer_auth(token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().context("GitHub connection failed")?;
        if response.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(json!({}));
        }
        Self::decode(response)
    }
    pub fn oauth(&self, body: Value) -> Result<Value> {
        Self::decode(
            self.client()?
                .post(&self.oauth)
                .header("Accept", "application/json")
                .json(&body)
                .send()
                .context("GitHub authorization failed")?,
        )
    }
    pub fn pages(&self, path: &str, key: &str, token: &str) -> Result<Vec<Value>> {
        let mut all = vec![];
        for page in 1..=100 {
            let response = self.call(
                "GET",
                &format!("{path}?per_page=100&page={page}"),
                token,
                None,
            )?;
            let rows = response[key].as_array().context("Invalid GitHub list")?;
            all.extend(rows.iter().cloned());
            if rows.len() < 100 {
                return Ok(all);
            }
        }
        anyhow::bail!("GitHub list exceeds 10,000 entries; narrow the installation selection")
    }
    /// After a worker proves VM deletion, remove only its own leftover JIT
    /// registration. A runner that completed a job may already be gone.
    pub fn cleanup_runner(&self, path: &str, token: &str, expected_name: &str) -> Result<()> {
        let response = self
            .client()?
            .get(format!("{}{path}", self.base))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .bearer_auth(token)
            .send()?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        let runner = Self::decode(response)?;
        ensure!(
            runner["name"] == expected_name && runner["busy"] == false,
            "Runner identity changed or GitHub still reports it busy; inspect before removing"
        );
        self.call("DELETE", path, token, None)?;
        Ok(())
    }
}

pub fn jwt(app: &crate::App) -> Result<String> {
    let der = STANDARD.decode(
        app.private_key
            .lines()
            .filter(|l| !l.starts_with("---"))
            .collect::<String>(),
    )?;
    let key = if app.private_key.contains("BEGIN RSA PRIVATE KEY") {
        RsaKeyPair::from_der(&der)
    } else {
        RsaKeyPair::from_pkcs8(&der)
    }
    .map_err(|_| anyhow::anyhow!("Invalid GitHub App RSA key"))?;
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(
        &json!({"iat":now()-60,"exp":now()+540,"iss":app.client_id}),
    )?);
    let data = format!("{header}.{payload}");
    let mut signature = vec![0; key.public().modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        data.as_bytes(),
        &mut signature,
    )
    .map_err(|_| anyhow::anyhow!("GitHub App signing failed"))?;
    Ok(format!("{data}.{}", URL_SAFE_NO_PAD.encode(signature)))
}
