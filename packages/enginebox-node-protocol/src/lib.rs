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
    pub budget: Budget,
    pub workloads: Workloads,
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
pub fn id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 80
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
pub fn hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}
pub const HEARTBEAT_SECONDS: u64 = 3;
pub const LEASE_SECONDS: u64 = 20;
