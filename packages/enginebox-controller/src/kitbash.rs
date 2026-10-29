//! Reading values Kitbash delivers to the controller (docs/CONNECTIONS.md,
//! "Kitbash").
//!
//! Kitbash's controller receiver keeps one directory per enrolled delivery
//! (an "integration") under the operator's delivery root, for example
//! `/opt/enginebox-control/kitbash/<integration>/`:
//!
//! ```text
//! config.json        {integration, project, target, kind, names, outputDirectory, enginebox_org?}
//! receipt.json       {deliveryId, integration, status: "staged", directory, names}
//! credentials/<deliveryId>/<file>   one 0600 file per delivered value
//! ```
//!
//! For the GitHub App kinds (`runners`, `repos`) the files are
//! `<kind>-client-secret`, `<kind>-private-key` and `<kind>-webhook-secret`,
//! mapped to secret names by `config.names`. For the generic `env` kind each
//! value's file is named after the value (`CLOUDFLARE_API_TOKEN`).
//!
//! Only the operator chooses the root (`ENGINEBOX_KITBASH_ROOT`); a
//! connection names an integration directory inside it, never a path. A
//! delivery belongs to the platform org unless its `config.json` names
//! another (`enginebox_org`), and its project and target must match the
//! connection's `project` and `environment` when those are set. Nothing is
//! written; values are read when a run resolves the connection.
use crate::connections::{Source, SourceReader};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub const ROOT_ENV: &str = "ENGINEBOX_KITBASH_ROOT";
const MAX_VALUE_BYTES: u64 = 16 * 1024;
const MAX_RECORD_BYTES: u64 = 64 * 1024;

pub struct KitbashReader {
    root: PathBuf,
}

/// A regular, non-symlink, owner-only file of at most `limit` bytes.
fn private_file(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("The delivery has no {}.", display_name(path)))?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= limit,
        "A delivery file is not a regular file within its size bound."
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            meta.permissions().mode() & 0o077 == 0,
            "A delivery file is readable by other users; Kitbash writes 0600 files."
        );
    }
    Ok(std::fs::read(path)?)
}
fn private_dir(path: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("No Kitbash delivery directory {}.", display_name(path)))?;
    ensure!(
        meta.is_dir() && !meta.file_type().is_symlink(),
        "A Kitbash delivery directory is not a plain directory."
    );
    Ok(())
}
fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}
fn record(path: &Path) -> Result<Value> {
    serde_json::from_slice(&private_file(path, MAX_RECORD_BYTES)?)
        .with_context(|| format!("{} is not JSON.", display_name(path)))
}
fn kebab(field: &str) -> String {
    let mut out = String::new();
    for c in field.chars() {
        if c.is_ascii_uppercase() {
            out.push('-');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

impl KitbashReader {
    /// The reader for the operator's root, or `None` when unset.
    pub fn from_env() -> Result<Option<Self>> {
        match std::env::var_os(ROOT_ENV) {
            None => Ok(None),
            Some(root) => Self::new(PathBuf::from(root)).map(Some),
        }
    }
    pub fn new(root: PathBuf) -> Result<Self> {
        ensure!(root.is_absolute(), "{ROOT_ENV} must be an absolute path.");
        Ok(Self { root })
    }
}

impl SourceReader for KitbashReader {
    fn read(
        &self,
        org: &str,
        source: &Source,
        names: &[String],
    ) -> Result<BTreeMap<String, String>> {
        let integration = source
            .integration
            .as_deref()
            .context("Kitbash sources need an integration.")?;
        // Validated by the connection already; checked again because it
        // becomes a path component.
        ensure!(
            !integration.is_empty()
                && !integration.starts_with('.')
                && integration
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
            "Invalid Kitbash integration."
        );
        private_dir(&self.root)?;
        let dir = self.root.join(integration);
        private_dir(&dir)?;
        let config = record(&dir.join("config.json"))?;
        ensure!(
            config["integration"] == integration,
            "The Kitbash delivery's configuration names another integration."
        );
        let owner = config["enginebox_org"]
            .as_str()
            .unwrap_or(enginebox_agents::pools::DEFAULT_ORG);
        ensure!(
            owner == org,
            "The Kitbash delivery {integration} does not belong to this organization."
        );
        if let Some(project) = &source.project {
            ensure!(
                config["project"].as_str() == Some(project.as_str()),
                "The Kitbash delivery {integration} is for another project."
            );
        }
        if let Some(environment) = &source.environment {
            ensure!(
                config["target"].as_str() == Some(environment.as_str()),
                "The Kitbash delivery {integration} is for another environment."
            );
        }
        let receipt = record(&dir.join("receipt.json")).with_context(|| {
            format!("The Kitbash delivery {integration} has not been received yet.")
        })?;
        ensure!(
            receipt["status"] == "staged" && receipt["integration"] == integration,
            "The Kitbash delivery {integration} is not staged."
        );
        let delivery = receipt["deliveryId"]
            .as_str()
            .filter(|d| {
                (1..=64).contains(&d.len()) && d.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
            })
            .context("The Kitbash receipt has no valid delivery ID.")?;
        // The staged directory must be this integration's own credentials
        // directory, whatever the receipt says.
        let staged = dir.join("credentials").join(delivery);
        if let Some(claimed) = receipt["directory"].as_str() {
            ensure!(
                Path::new(claimed) == staged,
                "The Kitbash receipt points outside its integration directory."
            );
        }
        private_dir(&dir.join("credentials"))?;
        private_dir(&staged)?;
        // Secret name -> file name.
        let kind = config["kind"].as_str().unwrap_or("env");
        let mut files: BTreeMap<String, String> = BTreeMap::new();
        if let Some(map) = config["names"].as_object()
            && kind != "env"
        {
            ensure!(
                ["runners", "repos"].contains(&kind),
                "Unknown Kitbash delivery kind."
            );
            for (field, name) in map {
                if let Some(name) = name.as_str() {
                    files.insert(name.to_owned(), format!("{kind}-{}", kebab(field)));
                }
            }
        }
        let mut values = BTreeMap::new();
        for name in names {
            let file = match files.get(name) {
                Some(file) => file.clone(),
                None if kind == "env" => name.clone(),
                None => bail!("The Kitbash delivery {integration} does not deliver {name}."),
            };
            let bytes = private_file(&staged.join(&file), MAX_VALUE_BYTES).with_context(|| {
                format!("The Kitbash delivery {integration} does not deliver {name}.")
            })?;
            let value = String::from_utf8(bytes)
                .map_err(|_| anyhow::anyhow!("Delivered value {name} is not UTF-8."))?;
            ensure!(
                !value.is_empty() && !value.contains('\0'),
                "Delivered value {name} is empty or contains NUL."
            );
            values.insert(name.clone(), value);
        }
        Ok(values)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// A delivery root laid out the way Kitbash's receiver writes it.
    pub(crate) fn delivery(root: &Path, integration: &str, config: Value, files: &[(&str, &str)]) {
        let write = |path: PathBuf, bytes: &[u8]| {
            std::fs::write(&path, bytes).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
        };
        let id = "6f1c2a9e-0000-4000-8000-000000000001";
        let dir = root.join(integration);
        let staged = dir.join("credentials").join(id);
        std::fs::create_dir_all(&staged).unwrap();
        write(dir.join("config.json"), config.to_string().as_bytes());
        write(
            dir.join("receipt.json"),
            json!({"deliveryId":id,"integration":integration,"status":"staged","directory":staged})
                .to_string()
                .as_bytes(),
        );
        for (file, value) in files {
            write(staged.join(file), value.as_bytes());
        }
    }

    #[test]
    fn reads_generic_and_github_app_deliveries_with_org_project_and_target_checks() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        delivery(
            &root,
            "canvalytics-staging",
            json!({"integration":"canvalytics-staging","project":"canvalytics","target":"staging","kind":"env"}),
            &[("CLOUDFLARE_API_TOKEN", "cf-staging-value-1")],
        );
        delivery(
            &root,
            "enginebox-runners",
            json!({"integration":"enginebox-runners","project":"enginebox","target":"production","kind":"runners",
                   "names":{"clientSecret":"ENGINEBOX_GITHUB_RUNNERS_CLIENT_SECRET","privateKey":"ENGINEBOX_GITHUB_RUNNERS_PRIVATE_KEY","webhookSecret":"ENGINEBOX_GITHUB_RUNNERS_WEBHOOK_SECRET"}}),
            &[
                ("runners-client-secret", "client-secret-value"),
                ("runners-private-key", "pem"),
                ("runners-webhook-secret", "hook"),
            ],
        );
        let reader = KitbashReader::new(root.clone()).unwrap();
        let source = |integration: &str, project: Option<&str>, environment: Option<&str>| Source {
            provider: crate::connections::SourceProvider::Kitbash,
            integration: Some(integration.into()),
            project: project.map(Into::into),
            environment: environment.map(Into::into),
        };
        let names = vec!["CLOUDFLARE_API_TOKEN".to_string()];
        let values = reader
            .read(
                "default",
                &source("canvalytics-staging", Some("canvalytics"), Some("staging")),
                &names,
            )
            .unwrap();
        assert_eq!(values["CLOUDFLARE_API_TOKEN"], "cf-staging-value-1");
        let github = reader
            .read(
                "default",
                &source("enginebox-runners", None, None),
                &["ENGINEBOX_GITHUB_RUNNERS_CLIENT_SECRET".into()],
            )
            .unwrap();
        assert_eq!(
            github["ENGINEBOX_GITHUB_RUNNERS_CLIENT_SECRET"],
            "client-secret-value"
        );
        // Wrong project, target or org; missing value; path tricks.
        let org = "org_aaaaaaaaaaaaaaaaaaaaaaaa";
        for (org, s, n) in [
            (
                "default",
                source("canvalytics-staging", Some("other"), None),
                names.clone(),
            ),
            (
                "default",
                source("canvalytics-staging", None, Some("production")),
                names.clone(),
            ),
            (
                org,
                source("canvalytics-staging", None, None),
                names.clone(),
            ),
            (
                "default",
                source("canvalytics-staging", None, None),
                vec!["MISSING".to_string()],
            ),
            ("default", source("..", None, None), names.clone()),
        ] {
            let error = reader.read(org, &s, &n).unwrap_err().to_string();
            assert!(!error.contains("cf-staging-value-1"), "{error}");
        }
        // A delivery may name a tenant org.
        delivery(
            &root,
            "tenant-prod",
            json!({"integration":"tenant-prod","project":"app","target":"production","kind":"env","enginebox_org":org}),
            &[("CLOUDFLARE_API_TOKEN", "tenant-value")],
        );
        assert!(
            reader
                .read(org, &source("tenant-prod", None, None), &names)
                .is_ok()
        );
        assert!(
            reader
                .read("default", &source("tenant-prod", None, None), &names)
                .is_err()
        );
        // A receipt pointing elsewhere is refused.
        let receipt = root.join("tenant-prod/receipt.json");
        std::fs::write(&receipt, json!({"deliveryId":"6f1c2a9e-0000-4000-8000-000000000001","integration":"tenant-prod","status":"staged","directory":"/etc"}).to_string()).unwrap();
        assert!(
            reader
                .read(org, &source("tenant-prod", None, None), &names)
                .is_err()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file = root.join("canvalytics-staging/credentials/6f1c2a9e-0000-4000-8000-000000000001/CLOUDFLARE_API_TOKEN");
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(
                reader
                    .read(
                        "default",
                        &source("canvalytics-staging", None, None),
                        &names
                    )
                    .is_err()
            );
        }
    }

    #[test]
    fn kitbash_sources_feed_runs_and_the_kitbash_mcp_connection() {
        use crate::connections::Connections;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("kitbash");
        delivery(
            &root,
            "ideator-staging",
            json!({"integration":"ideator-staging","project":"ideator","target":"staging","kind":"env"}),
            &[
                ("CLOUDFLARE_API_TOKEN", "cf-value-0001"),
                ("KITBASH_MCP_TOKEN", "kb-mcp-value-0002"),
            ],
        );
        let state = temp.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let c = Connections::open(&state)
            .unwrap()
            .with_reader(std::sync::Arc::new(KitbashReader::new(root).unwrap()));
        c.create("default", "owner", &json!({"name":"kitbash-staging","kind":"secret_source",
            "source":{"provider":"kitbash","integration":"ideator-staging","project":"ideator","environment":"staging"},
            "names":["CLOUDFLARE_API_TOKEN"]})).unwrap();
        c.create("default", "owner", &json!({"name":"kitbash-mcp-token","kind":"secret_source",
            "source":{"provider":"kitbash","integration":"ideator-staging"},"names":["KITBASH_MCP_TOKEN"]})).unwrap();
        c.create("default", "owner", &json!({"name":"kitbash","kind":"mcp_http","url":"https://kitbash.example.com/mcp",
            "headers":{"Authorization":{"secret_source":"kitbash-mcp-token","name":"KITBASH_MCP_TOKEN","prefix":"Bearer "}}})).unwrap();
        let mut block = enginebox_agents::profile::ProviderProfile::default();
        let names = vec!["kitbash".to_string(), "kitbash-staging".to_string()];
        let applied = c
            .apply("default", &names, "codex", None, &mut block)
            .unwrap();
        assert_eq!(block.env["CLOUDFLARE_API_TOKEN"], "cf-value-0001");
        assert_eq!(
            block.env["CONN_KITBASH_AUTHORIZATION"],
            "Bearer kb-mcp-value-0002"
        );
        // The MCP token source is used for the header only, not exported itself.
        assert!(!block.env.contains_key("KITBASH_MCP_TOKEN"));
        assert!(
            !serde_json::to_string(&applied.used)
                .unwrap()
                .contains("value-000")
        );
        let tested = c
            .test(
                "default",
                "kitbash-staging",
                &crate::connections::NetworkProbe,
            )
            .unwrap();
        assert_eq!(tested["test"]["ok"], true);
        assert_eq!(
            c.source_value("default", "kitbash-staging", "CLOUDFLARE_API_TOKEN")
                .unwrap(),
            "cf-value-0001"
        );
        assert!(
            c.source_value("default", "kitbash-staging", "KITBASH_MCP_TOKEN")
                .is_err()
        );
    }
}
