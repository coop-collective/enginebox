use crate::transport::EngineBox;
use anyhow::{Result, bail, ensure};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::HashSet, path::Path};

pub fn identifier(value: &str, instance: bool) -> Result<&str> {
    let pattern = if instance {
        r"^ebox-[a-z0-9][a-z0-9-]{0,48}$"
    } else {
        r"^[a-z0-9][a-z0-9-]{0,48}$"
    };
    ensure!(
        Regex::new(pattern)?.is_match(value),
        "Use ebox-prefixed instance names and lowercase letters, digits or hyphens"
    );
    Ok(value)
}
pub fn safe_path(value: &str) -> Result<()> {
    ensure!(
        value.starts_with('/')
            && !value.split('/').any(|p| p == "..")
            && !value.chars().any(|c| c.is_control()),
        "Guest paths must be absolute without '..' or control characters"
    );
    Ok(())
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct GuestConfig {
    pub uid: u32,
    pub gid: u32,
    pub user: String,
    pub home: String,
    pub workspace: String,
    pub display: String,
}
impl Default for GuestConfig {
    fn default() -> Self {
        Self {
            uid: 1001,
            gid: 1001,
            user: "agent".into(),
            home: "/home/agent".into(),
            workspace: "/workspace".into(),
            display: ":99".into(),
        }
    }
}
impl GuestConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.uid > 0 && self.uid < u32::MAX && self.gid > 0 && self.gid < u32::MAX,
            "Guest UID/GID must be non-root"
        );
        ensure!(
            Regex::new(r"^[a-z_][a-z0-9_-]{0,31}$")?.is_match(&self.user) && self.user != "root",
            "Guest user must be a non-root Linux username"
        );
        ensure!(
            Regex::new(r"^:[0-9]{1,5}(\.[0-9]{1,5})?$")?.is_match(&self.display),
            "Guest display must be a local X11 display"
        );
        safe_path(&self.home)?;
        safe_path(&self.workspace)?;
        Ok(())
    }
    pub fn from_env() -> Result<Self> {
        let mut result = Self::default();
        for (key, slot) in [
            ("USER", &mut result.user),
            ("HOME", &mut result.home),
            ("WORKSPACE", &mut result.workspace),
            ("DISPLAY", &mut result.display),
        ] {
            if let Ok(v) = std::env::var(format!("ENGINEBOX_GUEST_{key}")) {
                *slot = v;
            }
        }
        if let Ok(v) = std::env::var("ENGINEBOX_GUEST_UID") {
            result.uid = v.parse()?;
        }
        if let Ok(v) = std::env::var("ENGINEBOX_GUEST_GID") {
            result.gid = v.parse()?;
        }
        result.validate()?;
        Ok(result)
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    pub cpu: u64,
    pub memory_gib: u64,
    pub disk_gib: u64,
}
impl Resources {
    pub fn validate(self, minimum: Option<Self>) -> Result<Self> {
        ensure!((1..=512).contains(&self.cpu), "cpu must be 1–512");
        ensure!(
            (1..=512).contains(&self.memory_gib),
            "memory_gib must be 1–512"
        );
        ensure!(
            (12..=16384).contains(&self.disk_gib),
            "disk_gib must be 12–16384"
        );
        if let Some(m) = minimum {
            m.validate(None)?;
            ensure!(
                self.cpu >= m.cpu && self.memory_gib >= m.memory_gib && self.disk_gib >= m.disk_gib,
                "Resources must meet every template minimum"
            );
        }
        Ok(self)
    }
    pub fn from_value(v: &Value, minimum: Option<Self>) -> Result<Self> {
        serde_json::from_value::<Self>(v.clone())?.validate(minimum)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticTemplate {
    pub id: String,
    #[serde(default)]
    pub label: String,
    pub source: String,
    pub snapshot: String,
    pub minimum: Option<Resources>,
    pub recommended: Option<Resources>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WarmPolicy {
    pub template: String,
    pub size: String,
    pub ready: u32,
}
/// Default vCPU overcommit for agent workloads, which are mostly idle while a
/// native client waits on inference.
pub const DEFAULT_CPU_OVERCOMMIT: f64 = 4.0;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Host {
    pub id: String,
    pub label: String,
    pub target: String,
    pub project: String,
    pub pool: String,
    pub reserve_cpu: u64,
    pub reserve_memory_gib: u64,
    pub reserve_disk_gib: u64,
    pub disk_gib: u64,
    pub max_instances: u64,
    pub templates: Vec<StaticTemplate>,
    pub guest: GuestConfig,
    pub network: Option<String>,
    pub warm_pools: Vec<WarmPolicy>,
    pub contribution_cpu: Option<u64>,
    pub contribution_memory_gib: Option<u64>,
    /// vCPU overcommit ratio (1.0-64.0). Instance `limits.cpu` values are
    /// ceilings, not reservations, so admission allows
    /// `(cpu_total - reserve_cpu) * cpu_overcommit` running vCPUs. Unset uses
    /// [`DEFAULT_CPU_OVERCOMMIT`]; omitted from serialization when unset so
    /// older releases still read generated configuration. Memory is never
    /// overcommitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_overcommit: Option<f64>,
    /// Concurrent native agent runs admitted on this host (1-256). Unset uses
    /// the controller default; omitted from serialization when unset so older
    /// releases still read generated configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_agent_runs: Option<u64>,
    /// Fail closed unless the controller has a live owner-bound node credential.
    /// Older strict Host decoders reject this field, preventing unsafe rollback.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub managed_registration: bool,
}
impl Default for Host {
    fn default() -> Self {
        Self {
            id: String::new(),
            label: String::new(),
            target: String::new(),
            project: "enginebox".into(),
            pool: "enginebox".into(),
            reserve_cpu: 2,
            reserve_memory_gib: 4,
            reserve_disk_gib: 10,
            disk_gib: 30,
            max_instances: 16,
            templates: vec![],
            guest: GuestConfig::default(),
            network: None,
            warm_pools: vec![],
            contribution_cpu: None,
            contribution_memory_gib: None,
            cpu_overcommit: None,
            max_agent_runs: None,
            managed_registration: false,
        }
    }
}
impl Host {
    pub fn validate(&self) -> Result<()> {
        identifier(&self.id, false)?;
        identifier(&self.pool, false)?;
        if let Some(n) = &self.network {
            identifier(n, false)?;
        }
        ensure!(
            Regex::new(r"^(?:[a-zA-Z0-9_][a-zA-Z0-9_.-]*@)?[a-zA-Z0-9][a-zA-Z0-9_.-]*$")?
                .is_match(&self.target),
            "Host target must be an explicit SSH alias or local"
        );
        ensure!(
            Regex::new(r"^[a-zA-Z0-9][a-zA-Z0-9_-]{0,62}$")?.is_match(&self.project),
            "Invalid Incus project"
        );
        self.guest.validate()?;
        ensure!(
            self.reserve_cpu <= 4096
                && self.reserve_memory_gib <= 4096
                && self.reserve_disk_gib <= 4096
                && (8..=4096).contains(&self.disk_gib)
                && (1..=256).contains(&self.max_instances)
                && self.max_agent_runs.is_none_or(|n| (1..=256).contains(&n))
                && self
                    .cpu_overcommit
                    .is_none_or(|r| r.is_finite() && (1.0..=64.0).contains(&r)),
            "Invalid host resource bounds"
        );
        match (self.contribution_cpu, self.contribution_memory_gib) {
            (Some(c), Some(m)) => ensure!(
                (1..=4096).contains(&c) && (1..=16384).contains(&m),
                "Invalid contribution bounds"
            ),
            (None, None) => {}
            _ => bail!("Contribution CPU and memory must be supplied together"),
        }
        let mut ids = HashSet::new();
        for t in &self.templates {
            identifier(&t.id, false)?;
            identifier(&t.source, true)?;
            identifier(&t.snapshot, false)?;
            ensure!(ids.insert(&t.id), "Duplicate host template ID");
            match (t.minimum, t.recommended) {
                (Some(m), Some(r)) => {
                    m.validate(None)?;
                    r.validate(Some(m))?;
                }
                (None, None) => {}
                _ => bail!("Template minimum and recommended must be supplied together"),
            }
        }
        let mut policies = HashSet::new();
        for p in &self.warm_pools {
            identifier(&p.template, false)?;
            ensure!(
                ["minimum", "recommended", "small", "standard", "large"].contains(&p.size.as_str())
                    && p.ready <= 32,
                "Invalid warm policy"
            );
            ensure!(
                policies.insert((&p.template, &p.size)),
                "Duplicate warm policy"
            );
        }
        Ok(())
    }
    pub fn engine_box(&self) -> EngineBox {
        EngineBox {
            host: self.target.clone(),
            project: self.project.clone(),
            guest: self.guest.clone(),
        }
    }
    pub fn box_(&self) -> EngineBox {
        self.engine_box()
    }
    /// Effective vCPU overcommit ratio for admission.
    pub fn cpu_overcommit(&self) -> f64 {
        self.cpu_overcommit.unwrap_or(DEFAULT_CPU_OVERCOMMIT)
    }
    pub fn binding(&self) -> Value {
        json!({"target":self.target,"project":self.project,"pool":self.pool,"guest":self.guest,"network":self.network.as_deref().unwrap_or("")})
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FleetConfig {
    schema_version: u32,
    hosts: Vec<Host>,
}
pub fn load_hosts(path: &Path) -> Result<Vec<Host>> {
    let config: FleetConfig = toml::from_str(&std::fs::read_to_string(path)?)?;
    ensure!(
        config.schema_version == 1 && !config.hosts.is_empty(),
        "Fleet config requires schema_version 1 and at least one host"
    );
    let mut ids = HashSet::new();
    let mut targets = HashSet::new();
    let mut hosts = config.hosts;
    for h in &mut hosts {
        h.validate()?;
        ensure!(
            ids.insert(h.id.clone()) && targets.insert(h.target.clone()),
            "Host IDs and targets must be unique"
        );
        if h.label.is_empty() {
            h.label = h.id.clone();
        }
    }
    Ok(hosts)
}
pub fn sizes() -> Value {
    json!({"small":{"cpu":2,"memory_gib":2},"standard":{"cpu":4,"memory_gib":4},"large":{"cpu":8,"memory_gib":8}})
}
pub fn preset(
    host: &Host,
    template: &Value,
    size: Option<&str>,
    custom: Option<&Value>,
) -> Result<Resources> {
    let min = Resources::from_value(&template["minimum"], None)?;
    if let Some(c) = custom {
        ensure!(
            size.is_none(),
            "Choose a preset or custom resources, not both"
        );
        return Resources::from_value(c, Some(min));
    }
    let name = size.unwrap_or("recommended");
    if name == "minimum" || name == "recommended" {
        return Resources::from_value(&template[name], Some(min));
    }
    let legacy = sizes();
    ensure!(
        !legacy[name].is_null(),
        "Choose minimum, recommended or custom resources"
    );
    Resources {
        cpu: legacy[name]["cpu"].as_u64().unwrap(),
        memory_gib: legacy[name]["memory_gib"].as_u64().unwrap(),
        disk_gib: host.disk_gib,
    }
    .validate(Some(min))
}
