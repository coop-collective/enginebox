//! Outbound contributor contract, shared by the native controller and installer.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub cpu: u32,
    pub memory_gib: u32,
    pub disk_gib: u32,
}
impl Budget {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=256).contains(&self.cpu)
                && (2..=4096).contains(&self.memory_gib)
                && (10..=65536).contains(&self.disk_gib),
            "Invalid machine resource budget"
        );
        Ok(())
    }
    pub fn fits(&self, ceiling: &Self) -> bool {
        self.cpu <= ceiling.cpu
            && self.memory_gib <= ceiling.memory_gib
            && self.disk_gib <= ceiling.disk_gib
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Workloads {
    pub ci: bool,
    pub agents: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Invitation {
    pub label: String,
    #[serde(default = "installer_ceiling")]
    pub budget: Budget,
    pub workloads: Workloads,
}
// New registrations choose their actual allowance locally. Retain an optional
// owner ceiling for older API clients; it is never the installed allocation.
pub fn installer_ceiling() -> Budget {
    Budget {
        cpu: 256,
        memory_gib: 4096,
        disk_gib: 65536,
    }
}
impl Invitation {
    pub fn validate(&self) -> Result<()> {
        self.budget.validate()?;
        ensure!(
            !self.label.trim().is_empty()
                && self.label.len() <= 80
                && !self.label.chars().any(char::is_control),
            "Invalid node label"
        );
        ensure!(
            self.workloads.ci || self.workloads.agents,
            "Select CI or agents"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Enrollment {
    pub secret_hash: String,
    pub budget: Budget,
    pub os: String,
    pub architecture: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Run {
    pub request_id: String,
    pub kind: String,
    pub request: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRequest {
    pub operation: String,
    pub workspace: String,
    #[serde(default = "default_project")]
    pub project: String,
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}
fn default_project() -> String {
    "welcome".into()
}
fn default_timeout() -> u64 {
    120
}
impl WorkspaceRequest {
    pub fn validate(&self) -> Result<()> {
        let identifier = |v: &str| {
            !v.is_empty()
                && v.len() <= 48
                && v.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && !v.starts_with('-')
                && !v.ends_with('-')
        };
        ensure!(
            identifier(&self.workspace) && identifier(&self.project),
            "Invalid workspace or project"
        );
        ensure!(
            ["fork", "exec", "checkpoint", "remount", "close"].contains(&self.operation.as_str()),
            "Unknown workspace operation"
        );
        ensure!(
            (1..=3600).contains(&self.timeout_seconds),
            "Invalid workspace timeout"
        );
        ensure!(
            self.argv.len() <= 128
                && self
                    .argv
                    .iter()
                    .all(|a| a.len() <= 16384 && !a.contains('\0'))
                && self.argv.iter().map(String::len).sum::<usize>() <= 24000,
            "Invalid workspace command"
        );
        ensure!(
            (self.operation == "exec") != self.argv.is_empty(),
            "Only exec accepts a nonempty command"
        );
        Ok(())
    }
}
pub fn id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 80
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
/// Scoped service token wire format (`ebst_<32 hex id>.<secret>`), shared by
/// the controller, which mints tokens and enforces their scopes, and the
/// supervisor, which only admits active tokens to its ownership queue.
pub mod service_token {
    use sha2::{Digest, Sha256};

    pub const PREFIX: &str = "ebst_";
    pub const MAX_SECRET: usize = 128;

    pub fn valid_id(id: &str) -> bool {
        id.len() == 32
            && id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }
    /// Split a presented bearer into `(id, secret)` when it is well formed.
    pub fn parse(presented: &str) -> Option<(&str, &str)> {
        let (id, secret) = presented.strip_prefix(PREFIX)?.split_once('.')?;
        (valid_id(id) && !secret.is_empty() && secret.len() <= MAX_SECRET).then_some((id, secret))
    }
    /// The stored digest: SHA-256 of the secret's JSON string encoding.
    pub fn secret_hash(secret: &str) -> String {
        let encoded = serde_json::to_vec(&serde_json::Value::String(secret.to_owned()))
            .expect("JSON string serializes");
        format!("{:x}", Sha256::digest(encoded))
    }
}
pub fn hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}
pub const HEARTBEAT_SECONDS: u64 = 3;
pub const LEASE_SECONDS: u64 = 20;
