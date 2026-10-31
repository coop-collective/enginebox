//! Pool member lifecycle decisions, kept pure so they are tested with an
//! injected clock (docs/QUEUE.md, "Idle members and reclamation").
//!
//! - An idle member (no run, no unresolved lease) is frozen after the pool's
//!   `idle_freeze_seconds` (default 2 minutes) unless its pool has queued runs
//!   it could take, and stopped after `idle_stop_seconds` (default 1 hour).
//! - Placement prefers a running member, then a frozen one, then a stopped
//!   one; a warm claim and a fresh creation come after (the planner only
//!   creates when no member is free).
//! - Memory pressure stops frozen idle members, lowest reuse score first.
//! - Instance slots count live (running and frozen) instances: slot
//!   pressure is relieved like memory pressure, by stopping frozen members.
//! - Disk pressure deletes stale bases first, then stopped created members
//!   beyond the pool's `min_workspaces`, longest idle first.
use crate::queue::Placement;
use std::time::Duration;

/// A member's Incus state as the scheduler last saw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum VmState {
    #[default]
    Running,
    Frozen,
    Stopped,
    /// Starting, stopping, missing or unreadable: never acted on.
    Other,
}
impl VmState {
    pub fn from_status(status: &str) -> Self {
        match status {
            "Running" => Self::Running,
            "Frozen" => Self::Frozen,
            "Stopped" => Self::Stopped,
            _ => Self::Other,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Frozen => "frozen",
            Self::Stopped => "stopped",
            Self::Other => "unknown",
        }
    }
    /// Placement preference: lower is better (running, frozen, stopped).
    pub fn placement_rank(self) -> u8 {
        match self {
            Self::Running => 0,
            Self::Frozen => 1,
            Self::Stopped => 2,
            Self::Other => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdleAction {
    Freeze,
    Stop,
}

/// The pool's idle policy (`None`: that step is disabled).
#[derive(Clone, Copy, Debug)]
pub struct IdlePolicy {
    pub freeze_after: Option<Duration>,
    pub stop_after: Option<Duration>,
}

/// What to do with a free member idle for `idle`. A pool with queued runs this
/// member could take keeps it running: the run is about to land on it.
pub fn idle_action(
    state: VmState,
    idle: Duration,
    policy: IdlePolicy,
    demand: bool,
) -> Option<IdleAction> {
    if demand {
        return None;
    }
    let stop = policy.stop_after.is_some_and(|after| idle >= after);
    match state {
        VmState::Running if stop => Some(IdleAction::Stop),
        VmState::Running if policy.freeze_after.is_some_and(|after| idle >= after) => {
            Some(IdleAction::Freeze)
        }
        VmState::Frozen if stop => Some(IdleAction::Stop),
        _ => None,
    }
}

/// A native session whose last run ended this recently is likely resumed soon.
pub const SESSION_AFFINITY: Duration = Duration::from_secs(600);

/// How valuable it is to keep a frozen member's RAM: lower is stopped first
/// under memory pressure. Longer idle lowers it; a session that ran within
/// [`SESSION_AFFINITY`] raises it above every member without one. Demand
/// forecasts belong here too.
pub fn reuse_score(idle: Duration, session_age: Option<Duration>) -> i64 {
    let mut score = -(idle.as_secs().min(i64::MAX as u64 / 2) as i64);
    if session_age.is_some_and(|age| age < SESSION_AFFINITY) {
        score += 1 << 40;
    }
    score
}

/// One pool member as the reclaimers see it.
#[derive(Clone, Debug)]
pub struct MemberView {
    pub pool: String,
    pub placement: Placement,
    pub state: VmState,
    /// Idle time; `None` while a run or lease holds it.
    pub idle: Option<Duration>,
    /// Age of the newest native session started on it, when known.
    pub session_age: Option<Duration>,
    /// Created by autoscale (not listed by the owner).
    pub created: bool,
}

/// Frozen idle members on `host` to stop for memory, in order.
pub fn memory_victims(members: &[MemberView], host: &str) -> Vec<Placement> {
    let mut frozen: Vec<&MemberView> = members
        .iter()
        .filter(|m| m.placement.host_id == host && m.state == VmState::Frozen && m.idle.is_some())
        .collect();
    frozen.sort_by_key(|m| reuse_score(m.idle.unwrap_or_default(), m.session_age));
    frozen.into_iter().map(|m| m.placement.clone()).collect()
}

/// Something disk reclamation may delete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reclaim {
    /// A stopped base no template deployment references.
    Base { host_id: String, name: String },
    /// A stopped created member beyond its pool's minimum.
    Member { pool: String, placement: Placement },
}

/// What to delete on `host` to free disk, in order: stale bases,
/// then stopped created members beyond each pool's `min_workspaces`, longest
/// idle first. Listed members, busy members and running or frozen ones are
/// never chosen.
pub fn delete_victims(
    host: &str,
    stale_bases: &[String],
    members: &[MemberView],
    min_workspaces: &dyn Fn(&str) -> usize,
    created_count: &dyn Fn(&str) -> usize,
) -> Vec<Reclaim> {
    let mut out: Vec<Reclaim> = stale_bases
        .iter()
        .map(|name| Reclaim::Base {
            host_id: host.to_owned(),
            name: name.clone(),
        })
        .collect();
    let mut stopped: Vec<&MemberView> = members
        .iter()
        .filter(|m| {
            m.placement.host_id == host
                && m.created
                && m.state == VmState::Stopped
                && m.idle.is_some()
        })
        .collect();
    stopped.sort_by_key(|m| std::cmp::Reverse(m.idle.unwrap_or_default()));
    let mut removed: std::collections::BTreeMap<&str, usize> = Default::default();
    for m in stopped {
        let gone = removed.entry(&m.pool).or_default();
        if created_count(&m.pool).saturating_sub(*gone) > min_workspaces(&m.pool) {
            *gone += 1;
            out.push(Reclaim::Member {
                pool: m.pool.clone(),
                placement: m.placement.clone(),
            });
        }
    }
    out
}

/// How a run's workspace was brought up, recorded on the run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartPath {
    Hot,
    Frozen,
    Stopped,
    Warm,
    Created,
}
impl StartPath {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hot => "hot",
            Self::Frozen => "frozen",
            Self::Stopped => "stopped",
            Self::Warm => "warm",
            Self::Created => "created",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const MIN: Duration = Duration::from_secs(60);
    fn policy() -> IdlePolicy {
        IdlePolicy {
            freeze_after: Some(2 * MIN),
            stop_after: Some(60 * MIN),
        }
    }
    fn at(name: &str) -> Placement {
        Placement {
            host_id: "h".into(),
            name: name.into(),
        }
    }
    fn member(name: &str, state: VmState, idle: Option<u64>, session: Option<u64>) -> MemberView {
        MemberView {
            pool: "p".into(),
            placement: at(name),
            state,
            idle: idle.map(|m| MIN * m as u32),
            session_age: session.map(|m| MIN * m as u32),
            created: true,
        }
    }

    #[test]
    fn idle_members_freeze_at_two_minutes_and_stop_at_an_hour() {
        let p = policy();
        assert_eq!(idle_action(VmState::Running, MIN, p, false), None);
        assert_eq!(
            idle_action(VmState::Running, 2 * MIN, p, false),
            Some(IdleAction::Freeze)
        );
        assert_eq!(idle_action(VmState::Frozen, 59 * MIN, p, false), None);
        assert_eq!(
            idle_action(VmState::Frozen, 60 * MIN, p, false),
            Some(IdleAction::Stop)
        );
        // a running member past the stop time (freezing disabled or missed) stops
        assert_eq!(
            idle_action(VmState::Running, 61 * MIN, p, false),
            Some(IdleAction::Stop)
        );
        assert_eq!(idle_action(VmState::Stopped, 600 * MIN, p, false), None);
        assert_eq!(idle_action(VmState::Other, 600 * MIN, p, false), None);
    }

    #[test]
    fn demand_and_disabled_steps_keep_members_up() {
        assert_eq!(
            idle_action(VmState::Running, 10 * MIN, policy(), true),
            None
        );
        let never = IdlePolicy {
            freeze_after: None,
            stop_after: None,
        };
        assert_eq!(idle_action(VmState::Running, 600 * MIN, never, false), None);
        let stop_only = IdlePolicy {
            freeze_after: None,
            stop_after: Some(60 * MIN),
        };
        assert_eq!(
            idle_action(VmState::Running, 30 * MIN, stop_only, false),
            None
        );
        assert_eq!(
            idle_action(VmState::Running, 60 * MIN, stop_only, false),
            Some(IdleAction::Stop)
        );
    }

    #[test]
    fn reuse_score_prefers_recent_sessions_then_short_idle() {
        assert!(reuse_score(5 * MIN, None) > reuse_score(50 * MIN, None));
        assert!(reuse_score(50 * MIN, Some(5 * MIN)) > reuse_score(3 * MIN, None));
        assert!(reuse_score(50 * MIN, Some(20 * MIN)) < reuse_score(3 * MIN, None));
    }

    #[test]
    fn memory_pressure_stops_frozen_idle_members_lowest_score_first() {
        let members = vec![
            member("a", VmState::Frozen, Some(10), None),
            member("b", VmState::Frozen, Some(40), None),
            member("c", VmState::Frozen, Some(50), Some(3)), // a session ran 3 min ago
            member("d", VmState::Frozen, None, None),        // busy
            member("e", VmState::Running, Some(30), None),
            member("f", VmState::Stopped, Some(90), None),
        ];
        let mut other = member("g", VmState::Frozen, Some(99), None);
        other.placement.host_id = "elsewhere".into();
        let mut all = members.clone();
        all.push(other);
        assert_eq!(memory_victims(&all, "h"), vec![at("b"), at("a"), at("c")]);
    }

    #[test]
    fn disk_pressure_deletes_stale_bases_then_stopped_created_members_beyond_the_minimum() {
        let mut listed = member("listed", VmState::Stopped, Some(500), None);
        listed.created = false;
        let members = vec![
            member("old", VmState::Stopped, Some(300), None),
            member("newer", VmState::Stopped, Some(100), None),
            member("frozen", VmState::Frozen, Some(400), None),
            member("busy", VmState::Stopped, None, None),
            listed,
        ];
        let victims = delete_victims("h", &["ebox-base".into()], &members, &|_| 1, &|_| 4);
        assert_eq!(
            victims,
            vec![
                Reclaim::Base {
                    host_id: "h".into(),
                    name: "ebox-base".into()
                },
                Reclaim::Member {
                    pool: "p".into(),
                    placement: at("old")
                },
                Reclaim::Member {
                    pool: "p".into(),
                    placement: at("newer")
                },
            ]
        );
        // the pool keeps its minimum: with 2 created and a minimum of 1, one goes
        let victims = delete_victims("h", &[], &members, &|_| 1, &|_| 2);
        assert_eq!(
            victims,
            vec![Reclaim::Member {
                pool: "p".into(),
                placement: at("old")
            }]
        );
    }

    #[test]
    fn placement_prefers_running_then_frozen_then_stopped() {
        let mut states = [VmState::Stopped, VmState::Running, VmState::Frozen];
        states.sort_by_key(|s| s.placement_rank());
        assert_eq!(
            states,
            [VmState::Running, VmState::Frozen, VmState::Stopped]
        );
    }
}
