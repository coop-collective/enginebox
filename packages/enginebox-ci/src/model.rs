use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub fn identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
pub fn sha(s: &str) -> bool {
    [40, 64].contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}
pub fn repository(s: &str) -> bool {
    let parts: Vec<_> = s.split('/').collect();
    parts.len() == 2
        && parts.iter().all(|p| {
            !p.is_empty()
                && *p != "."
                && *p != ".."
                && p.len() <= 100
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        })
}
fn label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b))
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Os {
    Linux,
    Macos,
    Windows,
}
impl Os {
    pub fn current() -> Result<Self> {
        Ok(match std::env::consts::OS {
            "linux" => Self::Linux,
            "macos" => Self::Macos,
            "windows" => Self::Windows,
            _ => anyhow::bail!("CI requires Linux, macOS or Windows"),
        })
    }
    pub fn label(&self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Macos => "macos",
            Self::Windows => "windows",
        }
    }
}
pub fn architecture() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("x64"),
        "aarch64" => Ok("arm64"),
        _ => anyhow::bail!("Unsupported CI architecture"),
    }
}

/// An already provisioned native OS worker, including a worker inside a VM.
/// Image is an operator-owned revision label, never an instruction to boot a VM.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    pub schema_version: u32,
    pub id: String,
    pub target: String,
    pub os: Os,
    pub architecture: String,
    pub image: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub disabled: bool,
}
impl Node {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1 && identifier(&self.id),
            "Invalid CI node schema or ID"
        );
        ensure!(
            self.target == "local"
                || (!self.target.starts_with('-')
                    && !self.target.is_empty()
                    && self.target.len() <= 255
                    && self
                        .target
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"@._-:".contains(&b))),
            "Invalid SSH target"
        );
        ensure!(
            ["x64", "arm64"].contains(&self.architecture.as_str()) && label(&self.image),
            "Invalid CI architecture or image"
        );
        let reserved = [
            "self-hosted",
            "enginebox",
            "linux",
            "macos",
            "windows",
            "x64",
            "arm64",
        ];
        ensure!(
            self.capabilities.len() <= 32
                && self.capabilities.iter().all(|s| label(s)
                    && !reserved.contains(&s.to_ascii_lowercase().as_str())
                    && !s.to_ascii_lowercase().starts_with("image:")),
            "Invalid or reserved capability label"
        );
        Ok(())
    }
    pub fn labels(&self) -> BTreeSet<String> {
        [
            "self-hosted".to_owned(),
            "enginebox".to_owned(),
            self.os.label().to_owned(),
            self.architecture.clone(),
            format!("image:{}", self.image),
        ]
        .into_iter()
        .chain(self.capabilities.iter().cloned())
        .map(|s| s.to_ascii_lowercase())
        .collect()
    }
    pub fn matches(&self, labels: &[String]) -> bool {
        !self.disabled
            && labels
                .iter()
                .all(|l| self.labels().contains(&l.to_ascii_lowercase()))
    }
    pub fn require_local(&self) -> Result<()> {
        self.validate()?;
        ensure!(
            self.os == Os::current()? && self.architecture == architecture()?,
            "Worker OS/architecture does not match its registration"
        );
        ensure!(!self.disabled, "CI node is disabled");
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    Merge,
    WorkflowDispatch,
    Schedule,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Shell {
    Bash,
    Pwsh,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkout {
    pub url: String,
}
impl Checkout {
    fn validate(&self) -> Result<()> {
        let url = url::Url::parse(&self.url)?;
        ensure!(
            url.scheme() == "https"
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "Checkout requires an HTTPS URL without credentials/query/fragment"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "runner", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Job {
    Automation {
        #[serde(rename = "runs-on")]
        runs_on: Vec<String>,
        #[serde(default)]
        node: Option<String>,
        #[serde(default)]
        checkout: Option<Checkout>,
        shell: Shell,
        run: String,
        #[serde(default)]
        artifacts: Vec<String>,
        #[serde(rename = "timeout-minutes")]
        timeout_minutes: u64,
    },
    GithubActions {
        repository: String,
        workflow: String,
        #[serde(rename = "ref")]
        git_ref: String,
        #[serde(default)]
        inputs: BTreeMap<String, String>,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub schema_version: u32,
    /// Stable merge/event + job identity. A retry with different content is refused.
    pub request_id: String,
    pub event: Event,
    /// Exact source commit, passed to automation checkout and workflow input enginebox_sha.
    pub sha: String,
    pub job: Job,
}
impl Request {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1 && identifier(&self.request_id) && sha(&self.sha),
            "Invalid CI request identity or commit"
        );
        match &self.job {
            Job::Automation {
                runs_on,
                node,
                checkout,
                run,
                timeout_minutes,
                artifacts,
                ..
            } => {
                ensure!(
                    !runs_on.is_empty() && runs_on.len() <= 32 && runs_on.iter().all(|s| label(s)),
                    "runs-on needs explicit capability labels"
                );
                ensure!(node.as_deref().is_none_or(identifier), "Invalid node ID");
                ensure!(
                    !run.trim().is_empty()
                        && run.len() <= 8192
                        && !run.contains('\0')
                        && (1..=360).contains(timeout_minutes),
                    "Invalid automation script or timeout"
                );
                if let Some(checkout) = checkout {
                    checkout.validate()?;
                }
                ensure!(
                    artifacts.len() <= 16
                        && artifacts.iter().all(|s| !s.is_empty()
                            && s.len() <= 255
                            && !s.contains(['\\', ':'])
                            && s.split('/').all(|p| !p.is_empty()
                                && p != "."
                                && p != ".."
                                && p != ".git"
                                && p.bytes()
                                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)))),
                    "Artifacts must be literal relative paths without traversal or .git"
                );
            }
            Job::GithubActions {
                repository: repo,
                workflow,
                git_ref,
                inputs,
            } => {
                ensure!(repository(repo), "Use owner/repository");
                ensure!(
                    !workflow.is_empty()
                        && workflow.len() <= 100
                        && workflow
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
                        && (workflow.ends_with(".yml") || workflow.ends_with(".yaml")),
                    "Use a workflow filename"
                );
                ensure!(
                    !git_ref.is_empty()
                        && git_ref.len() <= 255
                        && !git_ref.chars().any(char::is_control),
                    "Explicit workflow branch or tag required"
                );
                ensure!(
                    inputs.len() <= 24
                        && !inputs.contains_key("enginebox_sha")
                        && inputs
                            .iter()
                            .all(|(k, v)| identifier(k) && v.len() <= 4096 && !v.contains('\0')),
                    "Invalid workflow inputs; enginebox_sha is supplied by Engine Box"
                );
            }
        }
        Ok(())
    }
}
