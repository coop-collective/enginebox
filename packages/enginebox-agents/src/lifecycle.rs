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
use std::time::{Duration, Instant};

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

/// Why a member must stay as it is: any of these keeps it from being frozen,
/// stopped or reclaimed, and clears its idle clock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hold {
    /// A run is placed on it (from admission, which reserves the member, to
    /// the run's end, including the wake/start before the run).
    pub run: bool,
    /// A writer lease (a run's provider lease, or an owner's attach).
    pub lease: bool,
    /// An unresolved (execution-uncertain) lease.
    pub uncertain: bool,
    /// A lifecycle action on it is in flight.
    pub action: bool,
}
impl Hold {
    pub fn busy(self) -> bool {
        self.run || self.lease || self.uncertain || self.action
    }
}

/// One member's lifecycle step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decision {
    /// Freeze or stop it.
    pub action: Option<IdleAction>,
    /// It is busy but frozen (a run would hang): resume it now.
    pub resume: bool,
    /// Its idle clock after this pass (`None` while busy).
    pub idle_since: Option<Instant>,
}

/// Decide one member's step. Only the controller that owns the fleet and is
/// admitting work (`active`) acts: a draining or drained controller does not
/// know the new owner's runs and its idle clocks are stale. A busy member
/// never gets an idle action and its idle clock restarts when it is free
/// again, so a member woken for a run waits a full policy period after that
/// run before it is frozen or stopped. `wanted`: queued runs could take it.
pub fn decide(
    active: bool,
    state: VmState,
    hold: Hold,
    wanted: bool,
    idle_since: Option<Instant>,
    policy: IdlePolicy,
    now: Instant,
) -> Decision {
    if !active {
        return Decision {
            action: None,
            resume: false,
            idle_since,
        };
    }
    if hold.busy() {
        return Decision {
            action: None,
            resume: state == VmState::Frozen && (hold.run || hold.lease) && !hold.action,
            idle_since: None,
        };
    }
    let since = idle_since.unwrap_or(now);
    Decision {
        action: idle_action(state, now.saturating_duration_since(since), policy, wanted),
        resume: false,
        idle_since: Some(since),
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

    fn run() -> Hold {
        Hold {
            run: true,
            ..Hold::default()
        }
    }

    #[test]
    fn a_member_being_placed_is_never_frozen_even_when_its_idle_clock_elapsed() {
        // Admission reserves the member (a run is placed on it) before its
        // wake and launch; a long-elapsed idle clock must not freeze it.
        let t0 = Instant::now();
        let d = decide(
            true,
            VmState::Running,
            run(),
            false,
            Some(t0),
            policy(),
            t0 + 90 * MIN,
        );
        assert_eq!(
            d,
            Decision {
                action: None,
                resume: false,
                idle_since: None
            }
        );
        for hold in [
            Hold {
                lease: true,
                ..Hold::default()
            },
            Hold {
                uncertain: true,
                ..Hold::default()
            },
            Hold {
                action: true,
                ..Hold::default()
            },
        ] {
            let d = decide(
                true,
                VmState::Running,
                hold,
                false,
                Some(t0),
                policy(),
                t0 + 90 * MIN,
            );
            assert_eq!(d.action, None, "{hold:?}");
            assert_eq!(d.idle_since, None);
        }
    }

    #[test]
    fn only_an_admitting_controller_acts() {
        // A drained or restored controller keeps its scheduler thread but
        // does not know the new owner's runs: it must not freeze or stop.
        let t0 = Instant::now();
        let d = decide(
            false,
            VmState::Running,
            Hold::default(),
            false,
            Some(t0),
            policy(),
            t0 + 90 * MIN,
        );
        assert_eq!(
            d,
            Decision {
                action: None,
                resume: false,
                idle_since: Some(t0)
            }
        );
        let d = decide(false, VmState::Frozen, run(), false, Some(t0), policy(), t0);
        assert!(!d.resume);
    }

    #[test]
    fn stop_counts_from_the_end_of_the_run_not_a_stale_idle_clock() {
        // ebox-ideator-95: idle since yesterday, woken for a run, stopped three
        // seconds after the run ended. The stale clock is dropped while busy
        // and a fresh one starts when the run ends.
        let stale = Instant::now();
        let woke = stale + 600 * MIN;
        let busy = decide(
            true,
            VmState::Running,
            run(),
            false,
            Some(stale),
            policy(),
            woke,
        );
        assert_eq!(busy.idle_since, None);
        let ended = woke + 5 * MIN;
        let free = decide(
            true,
            VmState::Running,
            Hold::default(),
            false,
            busy.idle_since,
            policy(),
            ended,
        );
        assert_eq!(
            free,
            Decision {
                action: None,
                resume: false,
                idle_since: Some(ended)
            }
        );
        let later = |m: u32, state| {
            decide(
                true,
                state,
                Hold::default(),
                false,
                Some(ended),
                policy(),
                ended + MIN * m,
            )
        };
        assert_eq!(later(1, VmState::Running).action, None);
        assert_eq!(later(2, VmState::Running).action, Some(IdleAction::Freeze));
        assert_eq!(later(59, VmState::Frozen).action, None);
        assert_eq!(later(60, VmState::Frozen).action, Some(IdleAction::Stop));
    }

    #[test]
    fn a_busy_frozen_member_is_resumed() {
        // After a restart or a stray freeze, a member whose run is still
        // going must not stay frozen. A lifecycle action in flight is left
        // alone (it will settle the state itself).
        let t0 = Instant::now();
        assert!(decide(true, VmState::Frozen, run(), false, None, policy(), t0).resume);
        let lease = Hold {
            lease: true,
            ..Hold::default()
        };
        assert!(decide(true, VmState::Frozen, lease, false, None, policy(), t0).resume);
        let acting = Hold {
            run: true,
            action: true,
            ..Hold::default()
        };
        assert!(!decide(true, VmState::Frozen, acting, false, None, policy(), t0).resume);
        assert!(!decide(true, VmState::Running, run(), false, None, policy(), t0).resume);
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
