//! Tenancy: node ownership and capacity grants (docs/ORGS.md).
//!
//! Every host or enrolled node belongs to exactly one org. Hosts not listed in
//! `host_orgs` belong to the platform org (`default`: the install's own org,
//! "Default Org" on a self-hosted install), so state written before orgs needs
//! no migration. Orgs bring their own machines by enrolling them; the dashboard
//! records the enrolling org here.
//!
//! A capacity grant lets the platform org's capacity serve a tenant org: one
//! pool or capacity class, with limits (concurrent runs, CPU, memory, monthly
//! hours). Limits are recorded and returned by [`Tenancy::admit_pool_run`] for
//! a metering/quota implementation to enforce; this core admits within a
//! grant and refuses without one. On a `managed` install grants are the only
//! way a tenant reaches platform capacity; on a `self_hosted` install the
//! instance owner uses them to share Default Org pools with other orgs.
//!
//! The mode is `ENGINEBOX_TENANCY_MODE` (`self_hosted`, the default, or
//! `managed`). Records live in `tenancy.json` (private, atomic, schema 1).
use anyhow::{Context, Result, ensure};
use enginebox_agents::pools::{self, DEFAULT_ORG, valid_org};
use enginebox_core::state;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub const SCHEMA: u32 = 1;
pub const MAX_HOSTS: usize = 4096;
pub const MAX_GRANTS: usize = 4096;
/// Header the dashboard sends with the owner token to say which org it acts for.
pub const ORG_HEADER: &str = "x-enginebox-org";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    SelfHosted,
    Managed,
}
impl Mode {
    pub fn from_env() -> Self {
        match std::env::var("ENGINEBOX_TENANCY_MODE").as_deref() {
            Ok("managed") => Self::Managed,
            _ => Self::SelfHosted,
        }
    }
}

/// Per-grant limits. `None` means unlimited within the grant.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent_runs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_gib: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monthly_hours: Option<u32>,
}

/// Platform org capacity offered to a tenant org.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityGrant {
    pub id: String,
    /// Always the platform org in this release.
    pub provider_org: String,
    pub tenant_org: String,
    /// One platform pool, or…
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
    /// …a capacity class (an opaque label a managed implementation maps to nodes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity_class: Option<String>,
    #[serde(default)]
    pub limits: Limits,
    pub created_at: String,
    #[serde(default)]
    pub revoked_at: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    schema_version: u32,
    #[serde(default)]
    host_orgs: BTreeMap<String, String>,
    #[serde(default)]
    grants: Vec<CapacityGrant>,
}

/// The outcome of asking whether an org may run in a pool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admission {
    /// The pool belongs to the acting org.
    Own,
    /// Platform capacity under a grant; the limits are for the quota layer.
    Granted {
        grant_id: String,
        limits: Limits,
    },
    Denied,
}

#[derive(Clone)]
pub struct Tenancy {
    path: PathBuf,
    mode: Mode,
    state: Arc<Mutex<Stored>>,
}

fn valid_class(class: &str) -> bool {
    (1..=48).contains(&class.len())
        && class.as_bytes()[0].is_ascii_lowercase()
        && class
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

impl Tenancy {
    pub fn open(root: &Path) -> Result<Self> {
        Self::open_with_mode(root, Mode::from_env())
    }
    pub fn open_with_mode(root: &Path, mode: Mode) -> Result<Self> {
        let path = root.join("tenancy.json");
        let stored = if path.exists() {
            let stored: Stored = serde_json::from_value(state::read_json(&path)?)?;
            ensure!(stored.schema_version == SCHEMA, "Unsupported tenancy state");
            ensure!(
                stored.host_orgs.len() <= MAX_HOSTS
                    && stored.grants.len() <= MAX_GRANTS
                    && stored
                        .host_orgs
                        .iter()
                        .all(|(h, o)| enginebox_core::identifier(h, false).is_ok() && valid_org(o)),
                "Invalid tenancy state"
            );
            stored
        } else {
            Stored {
                schema_version: SCHEMA,
                ..Default::default()
            }
        };
        Ok(Self {
            path,
            mode,
            state: Arc::new(Mutex::new(stored)),
        })
    }
    pub fn mode(&self) -> Mode {
        self.mode
    }
    fn save(&self, stored: &Stored) -> Result<()> {
        crate::jobs::write_private(&self.path, &serde_json::to_value(stored)?)
    }
    /// The org that owns `host` (the platform org unless assigned).
    pub fn host_org(&self, host: &str) -> String {
        self.state
            .lock()
            .unwrap()
            .host_orgs
            .get(host)
            .cloned()
            .unwrap_or_else(|| DEFAULT_ORG.into())
    }
    pub fn view(&self) -> Value {
        let stored = self.state.lock().unwrap();
        json!({
            "mode": self.mode,
            "platform_org": DEFAULT_ORG,
            "host_orgs": stored.host_orgs,
            "grants": stored.grants,
        })
    }
    /// Assign a host or enrolled node to an org (`default` returns it to the platform org).
    pub fn assign_host(
        &self,
        host: &str,
        body: &Value,
        known: &dyn Fn(&str) -> bool,
    ) -> Result<Value> {
        crate::keys(body, &["org_id"], &["org_id"])?;
        enginebox_core::identifier(host, false)?;
        ensure!(known(host), "Unknown host or node");
        let org = body["org_id"].as_str().context("org_id must be a string")?;
        ensure!(valid_org(org), "org_id must be an org ID");
        let mut stored = self.state.lock().unwrap();
        let mut next = stored.clone();
        if org == DEFAULT_ORG {
            next.host_orgs.remove(host);
        } else {
            ensure!(
                next.host_orgs.len() < MAX_HOSTS || next.host_orgs.contains_key(host),
                "Host assignment limit reached"
            );
            next.host_orgs.insert(host.into(), org.into());
        }
        self.save(&next)?;
        *stored = next;
        Ok(json!({"host_id":host,"org_id":org}))
    }
    /// Create a capacity grant from the platform org to a tenant org.
    pub fn grant(&self, body: &Value, pool_org: &dyn Fn(&str) -> Option<String>) -> Result<Value> {
        crate::keys(
            body,
            &["tenant_org", "pool", "capacity_class", "limits"],
            &["tenant_org"],
        )?;
        let tenant = body["tenant_org"]
            .as_str()
            .context("tenant_org must be a string")?;
        ensure!(
            valid_org(tenant) && tenant != DEFAULT_ORG,
            "tenant_org must be an org other than the platform org"
        );
        let pool = body.get("pool").and_then(Value::as_str).map(str::to_owned);
        let class = body
            .get("capacity_class")
            .and_then(Value::as_str)
            .map(str::to_owned);
        ensure!(
            pool.is_some() != class.is_some(),
            "Name exactly one of pool or capacity_class"
        );
        if let Some(p) = &pool {
            ensure!(pools::valid_name(p), "Invalid pool name");
            ensure!(
                pool_org(p).as_deref() == Some(DEFAULT_ORG),
                "Only an existing platform org pool can be granted"
            );
        }
        if let Some(c) = &class {
            ensure!(
                valid_class(c),
                "capacity_class must match ^[a-z][a-z0-9-]{{0,47}}$"
            );
        }
        let limits: Limits = match body.get("limits") {
            None | Some(Value::Null) => Limits::default(),
            Some(v) => serde_json::from_value(v.clone()).context("Invalid limits")?,
        };
        let grant = CapacityGrant {
            id: uuid::Uuid::new_v4().simple().to_string(),
            provider_org: DEFAULT_ORG.into(),
            tenant_org: tenant.into(),
            pool,
            capacity_class: class,
            limits,
            created_at: chrono::Utc::now().to_rfc3339(),
            revoked_at: None,
        };
        let mut stored = self.state.lock().unwrap();
        ensure!(
            stored.grants.len() < MAX_GRANTS,
            "Capacity grant limit reached"
        );
        let mut next = stored.clone();
        next.grants.push(grant.clone());
        self.save(&next)?;
        *stored = next;
        Ok(json!({"grant":grant}))
    }
    pub fn revoke_grant(&self, id: &str) -> Result<Value> {
        let mut stored = self.state.lock().unwrap();
        let mut next = stored.clone();
        let grant = next
            .grants
            .iter_mut()
            .find(|g| g.id == id && g.revoked_at.is_none())
            .context("Unknown capacity grant")?;
        grant.revoked_at = Some(chrono::Utc::now().to_rfc3339());
        let view = grant.clone();
        self.save(&next)?;
        *stored = next;
        Ok(json!({"grant":view}))
    }
    /// May `org` start a run in `pool` (owned by `pool_org`)? Its own pools,
    /// or platform pools under an active grant naming the pool. Capacity-class
    /// grants are resolved by a managed implementation and do not admit here.
    pub fn admit_pool_run(&self, org: &str, pool: &str, pool_org: &str) -> Admission {
        if org == pool_org {
            return Admission::Own;
        }
        if pool_org != DEFAULT_ORG {
            // Nobody, including the platform org, reaches another tenant's pools.
            return Admission::Denied;
        }
        let stored = self.state.lock().unwrap();
        stored
            .grants
            .iter()
            .find(|g| {
                g.revoked_at.is_none() && g.tenant_org == org && g.pool.as_deref() == Some(pool)
            })
            .map_or(Admission::Denied, |g| Admission::Granted {
                grant_id: g.id.clone(),
                limits: g.limits.clone(),
            })
    }
    /// May `org` start a run on a workspace of `host`? Only on its own hosts.
    pub fn admit_host_run(&self, org: &str, host: &str) -> bool {
        self.host_org(host) == org
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TENANT: &str = "org_0123456789abcdef01234567";
    const OTHER: &str = "org_fedcba9876543210fedcba98";

    #[test]
    fn unassigned_hosts_belong_to_the_platform_org_and_assignment_persists() {
        let temp = tempfile::tempdir().unwrap();
        let t = Tenancy::open_with_mode(temp.path(), Mode::SelfHosted).unwrap();
        assert_eq!(t.host_org("worker"), DEFAULT_ORG);
        assert!(t.admit_host_run(DEFAULT_ORG, "worker"));
        t.assign_host("worker", &json!({"org_id":TENANT}), &|_| true)
            .unwrap();
        assert!(!t.admit_host_run(DEFAULT_ORG, "worker"));
        let reopened = Tenancy::open_with_mode(temp.path(), Mode::SelfHosted).unwrap();
        assert_eq!(reopened.host_org("worker"), TENANT);
        assert!(
            t.assign_host("ghost", &json!({"org_id":TENANT}), &|_| false)
                .is_err()
        );
        assert!(
            t.assign_host("worker", &json!({"org_id":"evil"}), &|_| true)
                .is_err()
        );
        t.assign_host("worker", &json!({"org_id":DEFAULT_ORG}), &|_| true)
            .unwrap();
        assert_eq!(t.host_org("worker"), DEFAULT_ORG);
    }

    #[test]
    fn pool_runs_need_ownership_or_a_live_grant_and_never_cross_tenants() {
        let temp = tempfile::tempdir().unwrap();
        let t = Tenancy::open_with_mode(temp.path(), Mode::Managed).unwrap();
        let platform = |p: &str| (p == "shared").then(|| DEFAULT_ORG.to_string());
        assert_eq!(t.admit_pool_run(TENANT, "mine", TENANT), Admission::Own);
        assert_eq!(
            t.admit_pool_run(TENANT, "shared", DEFAULT_ORG),
            Admission::Denied
        );
        // The platform org never reaches a tenant's pool.
        assert_eq!(
            t.admit_pool_run(DEFAULT_ORG, "mine", TENANT),
            Admission::Denied
        );
        let granted = t
            .grant(
                &json!({"tenant_org":TENANT,"pool":"shared","limits":{"max_concurrent_runs":2}}),
                &platform,
            )
            .unwrap();
        let id = granted["grant"]["id"].as_str().unwrap().to_owned();
        match t.admit_pool_run(TENANT, "shared", DEFAULT_ORG) {
            Admission::Granted { limits, .. } => assert_eq!(limits.max_concurrent_runs, Some(2)),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            t.admit_pool_run(OTHER, "shared", DEFAULT_ORG),
            Admission::Denied
        );
        t.revoke_grant(&id).unwrap();
        assert_eq!(
            t.admit_pool_run(TENANT, "shared", DEFAULT_ORG),
            Admission::Denied
        );
        // Only platform pools can be granted, to orgs other than the platform org.
        assert!(
            t.grant(&json!({"tenant_org":TENANT,"pool":"theirs"}), &platform)
                .is_err()
        );
        assert!(
            t.grant(
                &json!({"tenant_org":DEFAULT_ORG,"pool":"shared"}),
                &platform
            )
            .is_err()
        );
        assert!(
            t.grant(
                &json!({"tenant_org":TENANT,"pool":"shared","capacity_class":"gpu"}),
                &platform
            )
            .is_err()
        );
        assert!(
            t.grant(
                &json!({"tenant_org":TENANT,"capacity_class":"gpu-large"}),
                &platform
            )
            .is_ok()
        );
    }
}
