//! Optional controller-to-dev-service handoff shared by dashboard and MCP imports.
use anyhow::{Context, Result, ensure};
use reqwest::{Url, blocking::Client, redirect::Policy};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{io::Read, path::Path, time::Duration};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevService {
    url: String,
    token_file: String,
}
impl DevService {
    pub fn validate(&self) -> Result<()> {
        let url = Url::parse(&self.url)?;
        ensure!(
            (url.scheme() == "https"
                || (url.scheme() == "http"
                    && matches!(url.host_str(), Some("127.0.0.1" | "[::1]"))))
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && url.path() == "/"
                && Path::new(&self.token_file).is_absolute(),
            "Invalid private development service configuration"
        );
        Ok(())
    }
    pub fn request(&self, host: &str, project: &str, start: bool, retry: bool) -> Result<Value> {
        self.validate()?;
        let token = std::fs::read_to_string(&self.token_file)
            .context("Development service token unavailable")?;
        ensure!(
            token.trim().len() >= 32,
            "Invalid development service token"
        );
        let client = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(10))
            .build()?;
        let url = Url::parse(&self.url)?.join("v1/environments/projects")?;
        let request = if start {
            client
                .post(url)
                .json(&json!({"host":host,"project":project,"retry":retry}))
        } else {
            client.get(url)
        };
        let response = request
            .bearer_auth(token.trim())
            .send()
            .map_err(|_| anyhow::anyhow!("Development service unavailable"))?;
        ensure!(
            response.status().is_success(),
            "Development service rejected the request"
        );
        let mut bytes = Vec::new();
        response.take(256 * 1024 + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 256 * 1024,
            "Development service response too large"
        );
        let value: Value = serde_json::from_slice(&bytes)?;
        if start {
            ensure!(
                value["host"] == host && value["project"] == project,
                "Development service returned a different project"
            );
            Ok(value)
        } else {
            Ok(value["projects"]
                .as_array()
                .context("Invalid development catalog")?
                .iter()
                .find(|p| p["host"] == host && p["project"] == project)
                .cloned()
                .unwrap_or_else(|| json!({"host":host,"project":project,"status":"not-started"})))
        }
    }
}
