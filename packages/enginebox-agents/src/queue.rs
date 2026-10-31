//! Durable run queue and capacity-aware admission.
//!
//! Accepted runs are persisted as private entries under `agent-run-queue/`
//! until they finish, so a controller restart re-admits queued runs in order.
//! The planner is a pure function: FIFO per principal, round-robin across
//! principals, one writer per workspace+provider, and admission bounded by
//! each host's configured run limit plus measured CPU and memory headroom.
use crate::native::RunRequest;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

const GIB: u64 = 1024 * 1024 * 1024;

/// Queue bounds and admission defaults. Every field is configurable; see
/// `docs/QUEUE.md` for the environment variables that set them.
#[derive(Clone, Debug, PartialEq)]
pub struct QueueLimits {
    /// Hard bound on queued runs across all principals (HTTP 429 beyond it).
    pub max_queued_total: usize,
    /// Hard bound on queued runs of one principal (HTTP 429 beyond it).
    pub max_queued_per_principal: usize,
    /// Concurrent runs per host when the host sets no `max_agent_runs`.
    pub default_host_max_runs: usize,
    /// Free host memory each newly admitted run must leave above the host's
    /// configured memory reserve.
    pub run_memory_headroom_bytes: u64,
    /// How long one host measurement is trusted.
    pub measure_ttl_seconds: u64,
    /// `Retry-After` on a 429.
    pub retry_after_seconds: u64,
    /// Minimum seconds between lease probes of one workspace whose previous
    /// run's execution is uncertain.
    pub uncertain_probe_seconds: u64,
}
impl Default for QueueLimits {
    fn default() -> Self {
        Self {
            max_queued_total: 4096,
            max_queued_per_principal: 1024,
            default_host_max_runs: 8,
            run_memory_headroom_bytes: GIB,
            measure_ttl_seconds: 15,
            retry_after_seconds: 30,
            uncertain_probe_seconds: 60,
        }
    }
}
impl QueueLimits {
    /// Defaults overridden by `ENGINEBOX_RUN_QUEUE_MAX_TOTAL`,
    /// `ENGINEBOX_RUN_QUEUE_MAX_PER_PRINCIPAL`, `ENGINEBOX_HOST_MAX_AGENT_RUNS`,
    /// `ENGINEBOX_AGENT_RUN_MEMORY_MIB`, `ENGINEBOX_HOST_MEASURE_TTL_SECONDS`,
    /// `ENGINEBOX_RUN_QUEUE_RETRY_AFTER_SECONDS` and
    /// `ENGINEBOX_UNCERTAIN_PROBE_SECONDS`. Invalid values are refused.
    pub fn from_env() -> Result<Self> {
        let mut limits = Self::default();
        let read = |key: &str, min: u64, max: u64| -> Result<Option<u64>> {
            match std::env::var(key) {
                Ok(raw) => {
                    let n: u64 = raw
                        .trim()
                        .parse()
                        .map_err(|_| anyhow::anyhow!("{key} must be an integer"))?;
                    ensure!((min..=max).contains(&n), "{key} must be {min}-{max}");
                    Ok(Some(n))
                }
                Err(_) => Ok(None),
            }
        };
        if let Some(n) = read("ENGINEBOX_RUN_QUEUE_MAX_TOTAL", 1, 1_000_000)? {
            limits.max_queued_total = n as usize;
        }
        if let Some(n) = read("ENGINEBOX_RUN_QUEUE_MAX_PER_PRINCIPAL", 1, 1_000_000)? {
            limits.max_queued_per_principal = n as usize;
        }
        if let Some(n) = read("ENGINEBOX_HOST_MAX_AGENT_RUNS", 1, 256)? {
            limits.default_host_max_runs = n as usize;
        }
        if let Some(n) = read("ENGINEBOX_AGENT_RUN_MEMORY_MIB", 0, 1024 * 1024)? {
            limits.run_memory_headroom_bytes = n * 1024 * 1024;
        }
        if let Some(n) = read("ENGINEBOX_HOST_MEASURE_TTL_SECONDS", 1, 3600)? {
            limits.measure_ttl_seconds = n;
        }
        if let Some(n) = read("ENGINEBOX_RUN_QUEUE_RETRY_AFTER_SECONDS", 1, 3600)? {
            limits.retry_after_seconds = n;
        }
        if let Some(n) = read("ENGINEBOX_UNCERTAIN_PROBE_SECONDS", 1, 3600)? {
            limits.uncertain_probe_seconds = n;
        }
        Ok(limits)
    }
}

/// Refusal when a hard queue bound is reached. The controller answers 429.
#[derive(Debug)]
pub struct QueueFull {
    pub scope: &'static str,
    pub retry_after_seconds: u64,
}
impl std::fmt::Display for QueueFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "The {} run queue is full; retry after {} seconds.",
            self.scope, self.retry_after_seconds
        )
    }
}
impl std::error::Error for QueueFull {}

/// What one host measurement reported (from the controller's existing
/// inventory: CPU count, available memory, and the host's reserves).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostLoad {
    pub cpu_total: u64,
    pub reserve_cpu: u64,
    pub memory_available_bytes: u64,
    pub reserve_memory_bytes: u64,
}
impl HostLoad {
    pub fn from_inventory(host: &enginebox_core::Host, facts: &Value) -> Result<Self> {
        let get = |key: &str| {
            facts[key]
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("Host inventory lacks {key}"))
        };
        Ok(Self {
            cpu_total: get("cpu_total")?,
            reserve_cpu: host.reserve_cpu,
            memory_available_bytes: get("memory_available_bytes")?,
            reserve_memory_bytes: host.reserve_memory_gib * GIB,
        })
    }
}

/// Where a run is to execute.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Target {
    /// One named workspace.
    Workspace { host_id: String, name: String },
    /// Any free workspace of a pool; `pinned` when resuming a pool session.
    Pool {
        pool: String,
        #[serde(default)]
        pinned: Option<Placement>,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Placement {
    pub host_id: String,
    pub name: String,
}

/// Where the callback goes and its key. Persisted in the private entry so a
/// restart keeps delivery; never copied into records or sidecars.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredCallback {
    pub url: String,
    pub secret: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryState {
    /// Waiting for admission. The entry holds the full request, env included.
    Queued,
    /// Admitted and running; env has been removed from the entry.
    Admitted,
    /// A pool run that ended before it was placed (cancelled, or its pool or
    /// profile disappeared). It has no workspace record; this is its record.
    Finished,
}

/// One private queue file (`agent-run-queue/<id>.json`, mode 0600).
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub schema: u32,
    pub id: String,
    pub seq: u64,
    pub principal: String,
    pub created_at: String,
    pub target: Target,
    pub state: EntryState,
    pub request: Value,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default)]
    pub callback: Option<StoredCallback>,
    #[serde(default)]
    pub placement: Option<Placement>,
    #[serde(default)]
    pub finished_at: Option<String>,
    #[serde(default)]
    pub outcome: Option<String>,
    /// `<project>/<name>` of the hosted repository the run uses. Only the
    /// name: its token is minted at admission and never stored. Omitted when
    /// unset so older releases still read the entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// The run's stated kind; re-resolves the profile's connection policy
    /// after a restart. Omitted when unset so older releases still read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_kind: Option<String>,
}
impl Entry {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1 && crate::native::validate_run_id(&self.id).is_ok(),
            "Invalid queue entry."
        );
        crate::run_details::validate_principal(&self.principal)?;
        chrono::DateTime::parse_from_rfc3339(&self.created_at)?;
        match &self.target {
            Target::Workspace { host_id, name } => {
                enginebox_core::identifier(host_id, false)?;
                enginebox_core::identifier(name, true)?;
            }
            Target::Pool { pool, pinned } => {
                ensure!(crate::pools::valid_name(pool), "Invalid queue pool.");
                if let Some(p) = pinned {
                    enginebox_core::identifier(&p.host_id, false)?;
                    enginebox_core::identifier(&p.name, true)?;
                }
            }
        }
        if let Some(p) = &self.profile {
            ensure!(crate::profile::valid_name(p), "Invalid queue profile.");
        }
        if let Some(r) = &self.repository {
            crate::repository::parse_ref(r)?;
        }
        if self.state == EntryState::Queued {
            RunRequest::parse(self.request.clone())?;
        }
        Ok(())
    }
    pub fn provider(&self) -> &str {
        self.request["provider"].as_str().unwrap_or("")
    }
    pub fn pool(&self) -> Option<&str> {
        match &self.target {
            Target::Pool { pool, .. } => Some(pool),
            Target::Workspace { .. } => None,
        }
    }
}

/// One host as the planner sees it.
#[derive(Clone, Debug)]
pub struct HostState {
    pub max_runs: usize,
    pub running: usize,
    /// None: never measured, or the last measurement failed.
    pub load: Option<HostLoad>,
    /// Runs admitted since `load` was measured; each reserves headroom.
    pub admitted_since: usize,
}

/// A pool as the planner sees it.
#[derive(Clone, Debug, Default)]
pub struct PoolState {
    pub members: Vec<Placement>,
    /// Workspaces being created for this pool right now.
    pub creating: usize,
    /// Hosts where a new workspace may be created, when the pool can grow.
    pub create_hosts: Vec<String>,
    pub max_workspaces: usize,
    /// True while a failed creation is backing off.
    pub backoff: bool,
    /// The failed creation's message while backing off.
    pub backoff_reason: Option<String>,
    /// Placement preference per member (lower first: running, frozen,
    /// stopped; see `lifecycle::VmState::placement_rank`). Absent: running.
    pub member_rank: BTreeMap<(String, String), u8>,
}

/// A queued run as the planner sees it.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub id: String,
    pub principal: String,
    pub seq: u64,
    pub provider: String,
    pub target: Target,
}

#[derive(Debug, Default)]
pub struct Plan {
    /// (run id, placement) in admission order.
    pub admit: Vec<(String, Placement)>,
    /// (pool, host) for each workspace to create.
    pub create: Vec<(String, String)>,
    /// run id -> (1-based queue position, reason) for runs left queued.
    pub waiting: BTreeMap<String, (usize, String)>,
    /// The principal served last, where the next pass resumes the rotation.
    pub cursor: Option<String>,
}

pub struct PlanInput<'a> {
    pub candidates: &'a [Candidate],
    pub cursor: Option<&'a str>,
    pub hosts: BTreeMap<String, HostState>,
    /// Held (host, workspace, provider) writer leases.
    pub leases: BTreeSet<(String, String, String)>,
    /// Workspaces a pool must not place on: any active run, or an unresolved
    /// lease from an interrupted run.
    pub busy_workspaces: BTreeSet<(String, String)>,
    /// Why a workspace is fenced by an unresolved lease, for waiting reasons.
    pub uncertain: BTreeMap<(String, String), String>,
    pub pools: BTreeMap<String, PoolState>,
    pub headroom_bytes: u64,
    pub admitting: bool,
}

/// Round-robin order: each principal's runs stay FIFO (by `seq`); principals
/// take turns, starting with the first one after `cursor`.
pub fn fair_order<'a>(candidates: &'a [Candidate], cursor: Option<&str>) -> Vec<&'a Candidate> {
    let mut by_principal: BTreeMap<&str, Vec<&Candidate>> = BTreeMap::new();
    let mut sorted: Vec<&Candidate> = candidates.iter().collect();
    sorted.sort_by_key(|c| c.seq);
    for c in sorted {
        by_principal.entry(&c.principal).or_default().push(c);
    }
    let mut principals: Vec<&str> = by_principal.keys().copied().collect();
    if let Some(cursor) = cursor {
        let split = principals.partition_point(|p| *p <= cursor);
        principals.rotate_left(split);
    }
    let mut out = Vec::with_capacity(candidates.len());
    let mut round = 0;
    loop {
        let mut any = false;
        for p in &principals {
            if let Some(c) = by_principal[p].get(round) {
                out.push(*c);
                any = true;
            }
        }
        if !any {
            break;
        }
        round += 1;
    }
    out
}

fn host_room(host_id: &str, state: Option<&HostState>, headroom: u64) -> Result<(), String> {
    let Some(state) = state else {
        return Err(format!("host {host_id} is not registered"));
    };
    if state.running >= state.max_runs {
        return Err(format!(
            "waiting for capacity on host {host_id} ({} of {} runs)",
            state.running, state.max_runs
        ));
    }
    let Some(load) = state.load else {
        return Err(format!(
            "waiting for capacity on host {host_id} (resource measurement unavailable)"
        ));
    };
    let cpu_room = load.cpu_total.saturating_sub(load.reserve_cpu).max(1) as usize;
    if state.running >= cpu_room {
        return Err(format!("waiting for capacity on host {host_id} (CPU)"));
    }
    let needed = load
        .reserve_memory_bytes
        .saturating_add(headroom.saturating_mul(state.admitted_since as u64 + 1));
    if load.memory_available_bytes < needed {
        return Err(format!("waiting for capacity on host {host_id} (memory)"));
    }
    Ok(())
}

/// Decide which queued runs start now, which pool workspaces to create, and
/// why every other run waits.
pub fn plan(mut input: PlanInput) -> Plan {
    let order = fair_order(input.candidates, input.cursor);
    let mut out = Plan {
        cursor: input.cursor.map(str::to_owned),
        ..Default::default()
    };
    // Pool runs that could not be placed this pass, per pool.
    let mut pool_demand: BTreeMap<String, usize> = BTreeMap::new();
    for (index, c) in order.iter().enumerate() {
        let position = index + 1;
        if !input.admitting {
            out.waiting.insert(
                c.id.clone(),
                (
                    position,
                    "admission is paused while the controller drains".into(),
                ),
            );
            continue;
        }
        let decision: Result<Placement, String> = match &c.target {
            Target::Workspace { host_id, name } => {
                if input
                    .leases
                    .contains(&(host_id.clone(), name.clone(), c.provider.clone()))
                {
                    Err(format!("workspace {name} busy"))
                } else {
                    host_room(host_id, input.hosts.get(host_id), input.headroom_bytes).map(|_| {
                        Placement {
                            host_id: host_id.clone(),
                            name: name.clone(),
                        }
                    })
                }
            }
            Target::Pool {
                pinned: Some(p), ..
            } => {
                if input
                    .busy_workspaces
                    .contains(&(p.host_id.clone(), p.name.clone()))
                {
                    Err(
                        match input.uncertain.get(&(p.host_id.clone(), p.name.clone())) {
                            Some(why) => {
                                format!("workspace {} busy (resume is pinned to it; {why})", p.name)
                            }
                            None => format!("workspace {} busy (resume is pinned to it)", p.name),
                        },
                    )
                } else {
                    host_room(
                        &p.host_id,
                        input.hosts.get(&p.host_id),
                        input.headroom_bytes,
                    )
                    .map(|_| p.clone())
                }
            }
            Target::Pool { pool, pinned: None } => match input.pools.get(pool) {
                None => Err(format!("pool {pool} is not defined")),
                Some(state) => {
                    // (member, rank, free run slots on its host)
                    let mut best: Option<(&Placement, u8, usize)> = None;
                    let mut host_reason = None;
                    let mut fenced = Vec::new();
                    for m in &state.members {
                        let key = (m.host_id.clone(), m.name.clone());
                        if input.busy_workspaces.contains(&key) {
                            if let Some(why) = input.uncertain.get(&key) {
                                fenced.push(format!("{}: {why}", m.name));
                            }
                            continue;
                        }
                        let h = input.hosts.get(&m.host_id);
                        match host_room(&m.host_id, h, input.headroom_bytes) {
                            Ok(()) => {
                                let h = h.unwrap();
                                let free = h.max_runs - h.running;
                                let rank = state.member_rank.get(&key).copied().unwrap_or(0);
                                // A running member first, then frozen, then
                                // stopped; among equals, the host with most room.
                                if best.is_none_or(|(_, r, f)| {
                                    (rank, std::cmp::Reverse(free)) < (r, std::cmp::Reverse(f))
                                }) {
                                    best = Some((m, rank, free));
                                }
                            }
                            Err(r) => host_reason = host_reason.or(Some(r)),
                        }
                    }
                    match best {
                        Some((m, _, _)) => Ok(m.clone()),
                        None => {
                            *pool_demand.entry(pool.clone()).or_default() += 1;
                            let mut reason = host_reason.unwrap_or_else(|| {
                                format!("waiting for a free workspace in pool {pool}")
                            });
                            if !fenced.is_empty() {
                                reason = format!("{reason} ({})", fenced.join("; "));
                            }
                            if state.backoff
                                && let Some(why) = &state.backoff_reason
                            {
                                reason = format!("{reason}; last workspace creation failed: {why}");
                            }
                            Err(reason)
                        }
                    }
                }
            },
        };
        match decision {
            Ok(placement) => {
                let host = input.hosts.get_mut(&placement.host_id).unwrap();
                host.running += 1;
                host.admitted_since += 1;
                input.leases.insert((
                    placement.host_id.clone(),
                    placement.name.clone(),
                    c.provider.clone(),
                ));
                input
                    .busy_workspaces
                    .insert((placement.host_id.clone(), placement.name.clone()));
                out.cursor = Some(c.principal.clone());
                out.admit.push((c.id.clone(), placement));
            }
            Err(reason) => {
                out.waiting.insert(c.id.clone(), (position, reason));
            }
        }
    }
    // Grow pools that still have unplaced demand beyond what is being created.
    for (pool, demand) in pool_demand {
        let Some(state) = input.pools.get_mut(&pool) else {
            continue;
        };
        let mut wanted = demand.saturating_sub(state.creating);
        while wanted > 0
            && !state.backoff
            && state.members.len() + state.creating < state.max_workspaces
        {
            let host = state
                .create_hosts
                .iter()
                .find(|h| host_room(h, input.hosts.get(*h), input.headroom_bytes).is_ok());
            let Some(host) = host.cloned() else { break };
            // Reserve the slot so a second creation does not overbook the host.
            if let Some(h) = input.hosts.get_mut(&host) {
                h.running += 1;
                h.admitted_since += 1;
            }
            state.creating += 1;
            out.create.push((pool.clone(), host));
            wanted -= 1;
        }
        if state.creating > 0 {
            for (id, (_, reason)) in out.waiting.iter_mut() {
                let is_pool = input.candidates.iter().any(|c| {
                    &c.id == id
                        && matches!(&c.target, Target::Pool { pool: p, pinned: None } if *p == pool)
                });
                if is_pool {
                    *reason = format!("creating a workspace for pool {pool}");
                }
            }
        }
    }
    out
}

/// Queue fields on a public run view.
pub fn decorate(view: &mut Value, waiting: Option<&(usize, String)>) {
    let queued = view["status"] == "queued";
    view["queue_position"] = json!(waiting.filter(|_| queued).map(|w| w.0));
    view["waiting_reason"] = json!(
        waiting
            .filter(|_| queued)
            .map(|w| w.1.clone())
            .or_else(|| queued.then(|| "waiting for the scheduler".to_owned()))
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ws(id: &str, principal: &str, seq: u64, name: &str) -> Candidate {
        Candidate {
            id: id.into(),
            principal: principal.into(),
            seq,
            provider: "claude".into(),
            target: Target::Workspace {
                host_id: "h1".into(),
                name: name.into(),
            },
        }
    }
    fn roomy() -> HostLoad {
        HostLoad {
            cpu_total: 64,
            reserve_cpu: 2,
            memory_available_bytes: 256 * GIB,
            reserve_memory_bytes: 4 * GIB,
        }
    }
    fn host(max_runs: usize, running: usize, load: Option<HostLoad>) -> HostState {
        HostState {
            max_runs,
            running,
            load,
            admitted_since: 0,
        }
    }
    fn input<'a>(candidates: &'a [Candidate], hosts: Vec<(&str, HostState)>) -> PlanInput<'a> {
        PlanInput {
            candidates,
            cursor: None,
            hosts: hosts.into_iter().map(|(k, v)| (k.into(), v)).collect(),
            leases: BTreeSet::new(),
            busy_workspaces: BTreeSet::new(),
            uncertain: BTreeMap::new(),
            pools: BTreeMap::new(),
            headroom_bytes: GIB,
            admitting: true,
        }
    }

    #[test]
    fn principals_take_turns_and_each_stays_fifo() {
        let mut c = vec![];
        for i in 0..6 {
            c.push(ws(
                &format!("a{i}"),
                "service-token:a",
                i,
                &format!("ebox-a{i}"),
            ));
        }
        c.push(ws("b0", "service-token:b", 10, "ebox-b0"));
        c.push(ws("b1", "service-token:b", 11, "ebox-b1"));
        c.push(ws("o0", "owner", 12, "ebox-o0"));
        let ids = |order: Vec<&Candidate>| -> Vec<String> {
            order.iter().map(|c| c.id.clone()).collect()
        };
        assert_eq!(
            ids(fair_order(&c, None)),
            ["o0", "a0", "b0", "a1", "b1", "a2", "a3", "a4", "a5"]
        );
        // The rotation resumes after the principal served last.
        assert_eq!(ids(fair_order(&c, Some("owner")))[..3], ["a0", "b0", "o0"]);
        assert_eq!(
            ids(fair_order(&c, Some("service-token:a")))[..3],
            ["b0", "o0", "a0"]
        );
        // Three slots: one heavy client cannot take them all.
        let plan = plan(input(&c, vec![("h1", host(3, 0, Some(roomy())))]));
        let admitted: Vec<_> = plan.admit.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(admitted, ["o0", "a0", "b0"]);
        assert_eq!(plan.cursor.as_deref(), Some("service-token:b"));
        assert_eq!(plan.waiting["a1"].0, 4);
        assert!(
            plan.waiting["a1"]
                .1
                .contains("waiting for capacity on host h1")
        );
    }

    #[test]
    fn one_writer_per_workspace_and_provider() {
        let mut second = ws("x2", "owner", 2, "ebox-w");
        let c = vec![ws("x1", "owner", 1, "ebox-w"), second.clone()];
        let plan1 = plan(input(&c, vec![("h1", host(8, 0, Some(roomy())))]));
        assert_eq!(plan1.admit.len(), 1);
        assert_eq!(plan1.admit[0].0, "x1");
        assert_eq!(plan1.waiting["x2"].1, "workspace ebox-w busy");
        // Another provider on the same workspace may run alongside.
        second.provider = "codex".into();
        let c = vec![ws("x1", "owner", 1, "ebox-w"), second];
        assert_eq!(
            plan(input(&c, vec![("h1", host(8, 0, Some(roomy())))]))
                .admit
                .len(),
            2
        );
    }

    #[test]
    fn admission_follows_the_host_limit_cpu_memory_and_measurement() {
        let c: Vec<_> = (0..10)
            .map(|i| ws(&format!("r{i}"), "owner", i, &format!("ebox-r{i}")))
            .collect();
        let admitted = |state: HostState| plan(input(&c, vec![("h1", state)])).admit.len();
        assert_eq!(admitted(host(4, 0, Some(roomy()))), 4);
        assert_eq!(admitted(host(4, 3, Some(roomy()))), 1);
        assert_eq!(admitted(host(8, 0, None)), 0);
        // CPU: 4 CPUs minus a reserve of 2 leaves room for two runs.
        let mut few_cpus = roomy();
        few_cpus.cpu_total = 4;
        assert_eq!(admitted(host(8, 0, Some(few_cpus))), 2);
        // Memory: 4 GiB reserve + 1 GiB per admitted run out of 7 GiB free.
        let mut tight = roomy();
        tight.memory_available_bytes = 7 * GIB;
        assert_eq!(admitted(host(8, 0, Some(tight))), 3);
        let p = plan(input(&c, vec![("h1", host(8, 0, Some(tight)))]));
        assert!(p.waiting["r3"].1.contains("(memory)"));
        // Runs admitted since the last measurement keep their reservation.
        let mut since = host(8, 0, Some(tight));
        since.admitted_since = 3;
        assert_eq!(admitted(since), 0);
        let p = plan(input(&c, vec![("h1", host(8, 0, None))]));
        assert!(p.waiting["r0"].1.contains("measurement unavailable"));
        // Draining pauses admission.
        let mut paused = input(&c, vec![("h1", host(8, 0, Some(roomy())))]);
        paused.admitting = false;
        let p = plan(paused);
        assert!(p.admit.is_empty() && p.waiting["r0"].1.contains("paused"));
    }

    fn pool_run(id: &str, seq: u64, pinned: Option<Placement>) -> Candidate {
        Candidate {
            id: id.into(),
            principal: "owner".into(),
            seq,
            provider: "claude".into(),
            target: Target::Pool {
                pool: "workers".into(),
                pinned,
            },
        }
    }
    fn member(host: &str, name: &str) -> Placement {
        Placement {
            host_id: host.into(),
            name: name.into(),
        }
    }

    #[test]
    fn pool_runs_spread_over_free_members_then_grow_the_pool() {
        let c: Vec<_> = (0..5)
            .map(|i| pool_run(&format!("p{i}"), i, None))
            .collect();
        let mut i = input(
            &c,
            vec![
                ("h1", host(8, 0, Some(roomy()))),
                ("h2", host(8, 0, Some(roomy()))),
            ],
        );
        i.busy_workspaces.insert(("h1".into(), "ebox-w2".into()));
        i.pools.insert(
            "workers".into(),
            PoolState {
                members: vec![
                    member("h1", "ebox-w1"),
                    member("h1", "ebox-w2"),
                    member("h2", "ebox-w3"),
                ],
                creating: 0,
                create_hosts: vec!["h2".into()],
                max_workspaces: 4,
                backoff: false,
                backoff_reason: None,
                member_rank: BTreeMap::new(),
            },
        );
        let p = plan(i);
        let placed: BTreeSet<_> = p.admit.iter().map(|(_, m)| m.name.clone()).collect();
        assert_eq!(
            placed,
            BTreeSet::from(["ebox-w1".to_string(), "ebox-w3".to_string()])
        );
        // Three unplaced runs, room for one more member (max 4).
        assert_eq!(p.create, vec![("workers".to_string(), "h2".to_string())]);
        assert!(
            p.waiting["p2"]
                .1
                .contains("creating a workspace for pool workers")
        );
    }

    #[test]
    fn placement_prefers_a_running_member_then_frozen_then_stopped() {
        let c = vec![pool_run("p", 1, None)];
        let mut i = input(
            &c,
            vec![
                ("h1", host(8, 0, Some(roomy()))),
                ("h2", host(8, 0, Some(roomy()))),
            ],
        );
        i.pools.insert(
            "workers".into(),
            PoolState {
                members: vec![
                    member("h1", "ebox-stopped"),
                    member("h1", "ebox-frozen"),
                    member("h2", "ebox-running"),
                ],
                max_workspaces: 3,
                member_rank: BTreeMap::from([
                    (("h1".into(), "ebox-stopped".into()), 2),
                    (("h1".into(), "ebox-frozen".into()), 1),
                ]),
                ..Default::default()
            },
        );
        assert_eq!(plan(i).admit[0].1, member("h2", "ebox-running"));
        let mut i = input(&c, vec![("h1", host(8, 0, Some(roomy())))]);
        i.pools.insert(
            "workers".into(),
            PoolState {
                members: vec![member("h1", "ebox-stopped"), member("h1", "ebox-frozen")],
                max_workspaces: 2,
                member_rank: BTreeMap::from([
                    (("h1".into(), "ebox-stopped".into()), 2),
                    (("h1".into(), "ebox-frozen".into()), 1),
                ]),
                ..Default::default()
            },
        );
        assert_eq!(plan(i).admit[0].1, member("h1", "ebox-frozen"));
    }

    #[test]
    fn a_pinned_resume_waits_for_its_own_workspace() {
        let c = vec![pool_run("r", 1, Some(member("h1", "ebox-w1")))];
        let mut i = input(&c, vec![("h1", host(8, 0, Some(roomy())))]);
        i.pools.insert(
            "workers".into(),
            PoolState {
                members: vec![member("h1", "ebox-w1"), member("h1", "ebox-w2")],
                max_workspaces: 2,
                ..Default::default()
            },
        );
        i.busy_workspaces.insert(("h1".into(), "ebox-w1".into()));
        let p = plan(i);
        assert!(p.admit.is_empty());
        assert!(p.waiting["r"].1.contains("pinned"));
        let mut i = input(&c, vec![("h1", host(8, 0, Some(roomy())))]);
        i.busy_workspaces.insert(("h1".into(), "ebox-w2".into()));
        assert_eq!(plan(i).admit[0].1, member("h1", "ebox-w1"));
    }

    #[test]
    fn waiting_reasons_name_fenced_workspaces_and_failed_creations() {
        let c = vec![
            pool_run("p", 1, None),
            pool_run("r", 2, Some(member("h1", "ebox-w1"))),
        ];
        let mut i = input(&c, vec![("h1", host(8, 0, Some(roomy())))]);
        i.pools.insert(
            "workers".into(),
            PoolState {
                members: vec![member("h1", "ebox-w1")],
                max_workspaces: 2,
                create_hosts: vec!["h1".into()],
                backoff: true,
                backoff_reason: Some("Insufficient CPU (memory: 4 GiB requested)".into()),
                ..Default::default()
            },
        );
        let key = ("h1".to_string(), "ebox-w1".to_string());
        i.busy_workspaces.insert(key.clone());
        i.uncertain
            .insert(key, "execution uncertain; lease probe failed".into());
        let p = plan(i);
        assert!(p.admit.is_empty() && p.create.is_empty());
        assert_eq!(
            p.waiting["p"].1,
            "waiting for a free workspace in pool workers (ebox-w1: execution uncertain; \
             lease probe failed); last workspace creation failed: Insufficient CPU (memory: \
             4 GiB requested)"
        );
        assert_eq!(
            p.waiting["r"].1,
            "workspace ebox-w1 busy (resume is pinned to it; execution uncertain; lease probe failed)"
        );
    }
}
