//! Owner-managed workspace pools. A pool is a named set of workspaces on one
//! or more hosts sharing a profile and/or template. Runs started on a pool are
//! placed on any free member; when `autoscale` allows, the scheduler creates a
//! fresh member from the pool's template, up to `max_workspaces`.
//! Records live under `agent-pools/<name>.json`; the pool session index (which
//! workspace holds each native session started in a pool) lives in
//! `agent-pool-sessions.json` so a resume lands where its session is.
use crate::{queue::Placement, security};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub const MAX_POOLS: usize = 256;
pub const MAX_MEMBERS: usize = 256;
const MAX_SESSIONS: usize = 4096;
const SESSIONS_FILE: &str = "agent-pool-sessions.json";
const SESSIONS_LIMIT: usize = 2 * 1024 * 1024;
const SIZES: [&str; 5] = ["minimum", "recommended", "small", "standard", "large"];

/// Project of pools created before pools carried one, and of new pools whose
/// request names none.
pub const DEFAULT_PROJECT: &str = "default";

/// A project ID, the same shape as the fleet Worker's project IDs
/// (`^[a-z0-9](?:[a-z0-9-]{0,46}[a-z0-9])?$`).
pub fn valid_project(project: &str) -> bool {
    let b = project.as_bytes();
    (1..=48).contains(&b.len())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && b[0] != b'-'
        && b[b.len() - 1] != b'-'
}
fn default_project() -> String {
    DEFAULT_PROJECT.into()
}

/// The platform org: owner of every record written before orgs existed and of
/// records that name none. Matches the dashboard's identity store.
pub const DEFAULT_ORG: &str = "default";

/// An org ID: `default` (the platform org) or `org_` + 24 lowercase hex.
pub fn valid_org(org: &str) -> bool {
    org == DEFAULT_ORG
        || (org.len() == 28
            && org.starts_with("org_")
            && org[4..]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

/// `^[a-z][a-z0-9-]{0,31}$`: short enough that `ebox-<pool>-<n>` stays a
/// valid instance name.
pub fn valid_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Autoscale {
    /// Hosts where new members may be created, in preference order.
    pub hosts: Vec<String>,
    /// Upper bound on all members, listed and created.
    pub max_workspaces: usize,
    /// Fleet size preset for created members (template default when unset).
    #[serde(default)]
    pub size: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pool {
    pub name: String,
    /// The project the pool belongs to. Records written before projects
    /// existed load as [`DEFAULT_PROJECT`].
    #[serde(default = "default_project")]
    pub project: String,
    /// The org that owns the pool. Absent (and never written) for the
    /// platform org, so records stay readable by releases before orgs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_id: Option<String>,
    #[serde(default)]
    pub description: String,
    /// Profile every run in the pool uses. A run may omit it or name it.
    #[serde(default)]
    pub profile: Option<String>,
    /// Template every member must come from; required for `autoscale`.
    #[serde(default)]
    pub template: Option<String>,
    /// Owner-listed members.
    #[serde(default)]
    pub workspaces: Vec<Placement>,
    #[serde(default)]
    pub autoscale: Option<Autoscale>,
    // Controller-maintained fields; a request may not supply them.
    #[serde(default)]
    pub created: Vec<Placement>,
    #[serde(default)]
    pub next_index: u32,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
}
impl Pool {
    /// Every member: listed first, then created, without duplicates.
    pub fn members(&self) -> Vec<Placement> {
        let mut seen = BTreeSet::new();
        self.workspaces
            .iter()
            .chain(self.created.iter())
            .filter(|m| seen.insert((*m).clone()))
            .cloned()
            .collect()
    }
    /// The owning org (the platform org when unset).
    pub fn org(&self) -> &str {
        self.org_id.as_deref().unwrap_or(DEFAULT_ORG)
    }
    pub fn max_workspaces(&self) -> usize {
        self.autoscale
            .as_ref()
            .map_or(self.members().len(), |a| a.max_workspaces)
    }
    pub fn validate(&self, known_host: &dyn Fn(&str) -> bool) -> Result<()> {
        ensure!(
            valid_name(&self.name),
            "Pool names match ^[a-z][a-z0-9-]{{0,31}}$"
        );
        ensure!(
            valid_project(&self.project),
            "project must match ^[a-z0-9](?:[a-z0-9-]{{0,46}}[a-z0-9])?$"
        );
        if let Some(org) = &self.org_id {
            ensure!(
                valid_org(org) && org != DEFAULT_ORG,
                "org_id must be an org ID other than the platform org (omit it for the platform org)"
            );
        }
        ensure!(
            self.description.len() <= 512 && !self.description.chars().any(char::is_control),
            "description must be at most 512 printable bytes"
        );
        if let Some(p) = &self.profile {
            ensure!(crate::profile::valid_name(p), "Invalid profile name");
        }
        if let Some(t) = &self.template {
            enginebox_core::identifier(t, false)?;
        }
        ensure!(
            self.workspaces.len() <= MAX_MEMBERS && self.members().len() <= MAX_MEMBERS,
            "A pool has at most 256 workspaces"
        );
        for m in self.workspaces.iter().chain(self.created.iter()) {
            enginebox_core::identifier(&m.host_id, false)?;
            enginebox_core::identifier(&m.name, true)?;
            ensure!(known_host(&m.host_id), "Unknown host in pool");
        }
        if let Some(a) = &self.autoscale {
            ensure!(
                self.template.is_some(),
                "autoscale needs a template to create workspaces from"
            );
            ensure!(
                !a.hosts.is_empty()
                    && a.hosts.len() <= 64
                    && a.hosts.iter().collect::<BTreeSet<_>>().len() == a.hosts.len(),
                "autoscale.hosts must name 1-64 distinct hosts"
            );
            for h in &a.hosts {
                enginebox_core::identifier(h, false)?;
                ensure!(known_host(h), "Unknown host in autoscale");
            }
            ensure!(
                (1..=MAX_MEMBERS).contains(&a.max_workspaces),
                "autoscale.max_workspaces must be 1-256"
            );
            if let Some(size) = &a.size {
                ensure!(SIZES.contains(&size.as_str()), "Unknown autoscale size");
            }
        } else {
            ensure!(
                !self.workspaces.is_empty() || !self.created.is_empty(),
                "A pool needs workspaces or autoscale"
            );
        }
        Ok(())
    }
    pub fn public(&self) -> Value {
        json!({
            "name":self.name,
            "project":self.project,
            "org_id":self.org(),
            "description":self.description,
            "profile":self.profile,
            "template":self.template,
            "workspaces":self.workspaces,
            "created_workspaces":self.created,
            "members":self.members(),
            "autoscale":self.autoscale,
            "max_workspaces":self.max_workspaces(),
            "revision":self.revision,
            "created_at":self.created_at,
            "updated_at":self.updated_at,
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct SessionHome {
    pool: String,
    host_id: String,
    name: String,
    at: String,
}

#[derive(Clone)]
pub struct Pools {
    dir: PathBuf,
    root: PathBuf,
    pools: Arc<Mutex<BTreeMap<String, Pool>>>,
    sessions: Arc<Mutex<BTreeMap<String, SessionHome>>>,
}
impl Pools {
    pub fn open(root: &Path) -> Result<Self> {
        let dir = root.join("agent-pools");
        security::private_dir(&dir)?;
        let mut pools = BTreeMap::new();
        for item in std::fs::read_dir(&dir)? {
            let filename = item?.file_name().to_string_lossy().into_owned();
            let Some(name) = filename.strip_suffix(".json") else {
                continue;
            };
            ensure!(valid_name(name), "Invalid pool record name.");
            let value = security::read_json(&dir, &filename)?.context("Pool record vanished")?;
            let pool: Pool = serde_json::from_value(value)?;
            ensure!(pool.name == name, "Invalid pool record.");
            pool.validate(&|_| true)?;
            pools.insert(pool.name.clone(), pool);
        }
        let sessions = security::read_bytes(root, SESSIONS_FILE, SESSIONS_LIMIT, false)?
            .map(|raw| -> Result<BTreeMap<String, SessionHome>> {
                let v: Value = serde_json::from_slice(&raw)?;
                Ok(serde_json::from_value(v["sessions"].clone())?)
            })
            .transpose()?
            .unwrap_or_default();
        Ok(Self {
            dir,
            root: root.to_path_buf(),
            pools: Arc::new(Mutex::new(pools)),
            sessions: Arc::new(Mutex::new(sessions)),
        })
    }
    fn save(&self, pool: &Pool) -> Result<()> {
        security::write_json(
            &self.dir,
            &format!("{}.json", pool.name),
            &serde_json::to_value(pool)?,
        )
    }
    /// Create or replace a pool from an owner request. Created members and
    /// the name counter are kept across replacement.
    pub fn put(&self, body: Value, known_host: &dyn Fn(&str) -> bool) -> Result<Value> {
        let object = body.as_object().context("Expected object")?;
        ensure!(
            object.keys().all(|k| [
                "name",
                "project",
                "org_id",
                "description",
                "profile",
                "template",
                "workspaces",
                "autoscale"
            ]
            .contains(&k.as_str())),
            "Unsupported pool fields"
        );
        let named_project = object.contains_key("project");
        let named_org = object.contains_key("org_id");
        let mut pool: Pool = serde_json::from_value(body)?;
        let mut pools = self.pools.lock().unwrap();
        let now = crate::now();
        match pools.get(&pool.name) {
            Some(existing) => {
                // Replacing a pool without naming a project keeps its project.
                if !named_project {
                    pool.project = existing.project.clone();
                }
                // Likewise its org: a pool changes org only when a request names one.
                if !named_org {
                    pool.org_id = existing.org_id.clone();
                }
                pool.created = existing.created.clone();
                pool.next_index = existing.next_index;
                pool.revision = existing.revision + 1;
                pool.created_at = existing.created_at.clone();
            }
            None => {
                ensure!(pools.len() < MAX_POOLS, "Pool limit reached");
                pool.revision = 1;
                pool.created_at = now.clone();
            }
        }
        pool.updated_at = now;
        pool.validate(known_host)?;
        self.save(&pool)?;
        let public = pool.public();
        pools.insert(pool.name.clone(), pool);
        Ok(json!({"pool":public}))
    }
    pub fn get(&self, name: &str) -> Option<Pool> {
        self.pools.lock().unwrap().get(name).cloned()
    }
    pub fn all(&self) -> Vec<Pool> {
        self.pools.lock().unwrap().values().cloned().collect()
    }
    /// Pools of one project, by name.
    pub fn in_project(&self, project: &str) -> Vec<Pool> {
        self.pools
            .lock()
            .unwrap()
            .values()
            .filter(|p| p.project == project)
            .cloned()
            .collect()
    }
    /// The project a pool belongs to.
    pub fn project_of(&self, name: &str) -> Option<String> {
        self.pools
            .lock()
            .unwrap()
            .get(name)
            .map(|p| p.project.clone())
    }
    pub fn exists(&self, name: &str) -> bool {
        self.pools.lock().unwrap().contains_key(name)
    }
    pub fn delete(&self, name: &str) -> Result<()> {
        ensure!(valid_name(name), "Invalid pool name");
        let mut pools = self.pools.lock().unwrap();
        ensure!(pools.contains_key(name), "Unknown pool");
        security::remove(&self.dir, &format!("{name}.json"))?;
        pools.remove(name);
        Ok(())
    }
    /// Reserve the next member name for a workspace the pool will create.
    pub fn reserve_name(&self, pool: &str) -> Result<String> {
        let mut pools = self.pools.lock().unwrap();
        let p = pools.get_mut(pool).context("Unknown pool")?;
        let mut next = p.clone();
        loop {
            next.next_index += 1;
            let name = format!("ebox-{}-{}", next.name, next.next_index);
            if !next.members().iter().any(|m| m.name == name) {
                enginebox_core::identifier(&name, true)?;
                self.save(&next)?;
                *p = next;
                return Ok(name);
            }
        }
    }
    pub fn add_created(&self, pool: &str, member: Placement) -> Result<()> {
        let mut pools = self.pools.lock().unwrap();
        let p = pools.get_mut(pool).context("Unknown pool")?;
        let mut next = p.clone();
        if !next.created.contains(&member) {
            next.created.push(member);
        }
        ensure!(
            next.members().len() <= MAX_MEMBERS,
            "A pool has at most 256 workspaces"
        );
        self.save(&next)?;
        *p = next;
        Ok(())
    }
    /// Remember which workspace holds a native session started in a pool.
    pub fn remember_session(&self, session: &str, pool: &str, at: &Placement) -> Result<()> {
        ensure!(security::exact_uuid(session), "Invalid native session ID.");
        let mut sessions = self.sessions.lock().unwrap();
        sessions.insert(
            session.to_owned(),
            SessionHome {
                pool: pool.to_owned(),
                host_id: at.host_id.clone(),
                name: at.name.clone(),
                at: crate::now(),
            },
        );
        while sessions.len() > MAX_SESSIONS {
            let oldest = sessions
                .iter()
                .min_by(|a, b| a.1.at.cmp(&b.1.at))
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => sessions.remove(&k),
                None => break,
            };
        }
        security::write_bytes(
            &self.root,
            SESSIONS_FILE,
            &serde_json::to_vec(&json!({"sessions":*sessions}))?,
            SESSIONS_LIMIT,
        )
    }
    /// The workspace holding `session`, when it was started in `pool`.
    pub fn session_home(&self, session: &str, pool: &str) -> Option<Placement> {
        self.sessions
            .lock()
            .unwrap()
            .get(session)
            .filter(|s| s.pool == pool)
            .map(|s| Placement {
                host_id: s.host_id.clone(),
                name: s.name.clone(),
            })
    }
}
