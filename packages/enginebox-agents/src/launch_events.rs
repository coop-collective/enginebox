//! Workspace launch telemetry: every admission or placement failure of a native
//! run and every pool member creation (success or failure) becomes one
//! structured log line and one event in a bounded history, so operators can
//! learn what makes a workspace launch succeed.
//!
//! Events never hold env values, prompts, tokens or callback URLs: only
//! identifiers (run, pool, project, workspace, host, template, profile,
//! provider), a stage, a reason code, a clipped and redacted message, and
//! durations. The newest [`MAX_EVENTS`] are kept in memory and in the private
//! file `launch-events.json` (mode 0600) under the controller root.
use crate::security;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// Events retained (memory and disk).
pub const MAX_EVENTS: usize = 500;
/// Longest message kept on an event or a log line.
pub const MAX_MESSAGE: usize = 240;
const FILE: &str = "launch-events.json";
const FILE_LIMIT: usize = 4 * 1024 * 1024;

/// Where a launch stopped. `resolve`: the profile or pool could not be resolved
/// at admission. `place`: the chosen workspace cannot take the run (missing,
/// stopped, wrong template, an earlier run's unresolved lease, or admission
/// bookkeeping failed). `create`: a pool member could not be created.
/// `bootstrap`: the workspace could not be prepared for the run (its operation
/// lock), or a created member could not be recorded. `delegate`: the provider
/// credential could not be delegated to the workspace. `start`: the guest
/// helper failed to launch the run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Resolve,
    Place,
    Create,
    Bootstrap,
    Delegate,
    Start,
}
impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Resolve => "resolve",
            Self::Place => "place",
            Self::Create => "create",
            Self::Bootstrap => "bootstrap",
            Self::Delegate => "delegate",
            Self::Start => "start",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "resolve" => Self::Resolve,
            "place" => Self::Place,
            "create" => Self::Create,
            "bootstrap" => Self::Bootstrap,
            "delegate" => Self::Delegate,
            "start" => Self::Start,
            _ => return None,
        })
    }
}

/// Stable reason codes. Clients may branch on them; new codes are additive.
pub const REASONS: &[&str] = &[
    "ok",
    "profile_unavailable",
    "pool_unavailable",
    "workspace_unavailable",
    "template_mismatch",
    "lease_unresolved",
    "admission_error",
    "pool_has_no_template",
    "name_unavailable",
    "create_failed",
    "member_record_failed",
    "workspace_locked",
    "delegation_failed",
    "provider_not_delegable",
    "guest_launch_failed",
    "repository_unavailable",
    "guest_outdated",
];
pub fn valid_reason(reason: &str) -> bool {
    REASONS.contains(&reason)
}

/// A launch that stopped before (or while) the guest started the run. Returned
/// by workspace preparation, and by a test or development executor to simulate
/// one stage failing.
#[derive(Debug, Clone)]
pub struct LaunchFailure {
    pub stage: Stage,
    pub reason: &'static str,
    pub message: String,
}
impl LaunchFailure {
    pub fn new(stage: Stage, reason: &'static str, message: impl std::fmt::Display) -> Self {
        debug_assert!(valid_reason(reason));
        Self {
            stage,
            reason,
            message: message.to_string(),
        }
    }
    /// Wrap an error from one step; the message is clipped and redacted later.
    pub fn from_error(stage: Stage, reason: &'static str, error: &anyhow::Error) -> Self {
        Self::new(stage, reason, format!("{error:#}"))
    }
    /// The operator-facing run error: fixed text per stage, never error detail.
    pub fn run_error(&self) -> &'static str {
        match self.reason {
            "repository_unavailable" => {
                return "Run could not start: its repository is unavailable or a repository token could not be issued.";
            }
            "guest_outdated" => {
                return "Run could not start: the workspace's guest helper does not support repository runs; update enginebox-guest in its template.";
            }
            _ => {}
        }
        match self.stage {
            Stage::Resolve => "Run could not start: its profile is no longer available.",
            Stage::Place => {
                "Run could not start: the workspace is unavailable or not running, was not created from the required template, or still holds an earlier run's lease."
            }
            Stage::Create => "Run could not start: a pool workspace could not be created.",
            Stage::Bootstrap => "Run could not start: the workspace could not be prepared.",
            Stage::Delegate => {
                "Run could not start: the provider could not be delegated to the workspace."
            }
            Stage::Start => {
                "Native launch failed; inspect installation and the workspace session lease."
            }
        }
    }
}
impl std::fmt::Display for LaunchFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "launch failed at {} ({})",
            self.stage.as_str(),
            self.reason
        )
    }
}
impl std::error::Error for LaunchFailure {}

/// Printable, single line, at most [`MAX_MESSAGE`] characters, with known
/// secret values and anything token-shaped replaced by `[redacted]`.
pub fn clip_message(message: &str, secrets: &BTreeMap<String, String>) -> String {
    let mut text: String = message
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    for value in secrets.values().filter(|v| v.len() >= 4) {
        text = text.replace(value.as_str(), "[redacted]");
    }
    let redacted: Vec<String> = text
        .split(' ')
        .map(|word| {
            let core = word.trim_matches(|c: char| "\"'`,;:()[]{}<>".contains(c));
            let tokenish = core.len() >= 24
                && core.chars().filter(char::is_ascii_digit).count() >= 2
                && core
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_-.+/=".contains(c))
                && !core.contains("://");
            let prefixed = ["ebst_", "ebrt_", "ebac_", "ebcs_", "sk-", "ghp_", "gho_"]
                .iter()
                .any(|p| core.starts_with(p));
            if tokenish || prefixed || core.to_ascii_lowercase().starts_with("bearer") {
                word.replace(core, "[redacted]")
            } else {
                word.to_owned()
            }
        })
        .collect();
    let joined = redacted
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut out: String = joined.chars().take(MAX_MESSAGE).collect();
    if joined.chars().count() > MAX_MESSAGE {
        out.pop();
        out.push('…');
    }
    out
}

/// One launch event. `kind` is `run_admission_failed`, `pool_member_created`
/// or `pool_member_create_failed`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LaunchEvent {
    pub at: String,
    pub kind: String,
    pub outcome: String,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub pool: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub host_id: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default)]
    pub template_revision: Option<Value>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub profile_revision: Option<u64>,
    #[serde(default)]
    pub provider: Option<String>,
    pub stage: String,
    pub reason: String,
    #[serde(default)]
    pub message: String,
    /// Time spent in the step that produced the event.
    #[serde(default)]
    pub duration_ms: u64,
    /// Time the run waited in the queue before admission (runs only).
    #[serde(default)]
    pub queue_wait_ms: Option<u64>,
}
impl LaunchEvent {
    /// The structured log line: one JSON object on stderr.
    pub fn log_line(&self) -> String {
        let mut value = serde_json::to_value(self).unwrap_or_else(|_| json!({}));
        value["event"] = json!("enginebox.launch");
        value.to_string()
    }
}

#[derive(Clone)]
pub struct LaunchEvents {
    root: PathBuf,
    events: Arc<Mutex<VecDeque<LaunchEvent>>>,
}
impl LaunchEvents {
    /// Load the retained history; an unreadable file starts a fresh one.
    pub fn open(root: &Path) -> Result<Self> {
        let events = security::read_bytes(root, FILE, FILE_LIMIT, false)
            .ok()
            .flatten()
            .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
            .and_then(|v| serde_json::from_value::<VecDeque<LaunchEvent>>(v["events"].clone()).ok())
            .unwrap_or_default();
        let mut events = events;
        while events.len() > MAX_EVENTS {
            events.pop_front();
        }
        Ok(Self {
            root: root.to_path_buf(),
            events: Arc::new(Mutex::new(events)),
        })
    }
    /// Log, retain and persist one event. Persistence failures are logged and
    /// never fail the launch path.
    pub fn record(&self, mut event: LaunchEvent) {
        if event.at.is_empty() {
            event.at = crate::now();
        }
        if !valid_reason(&event.reason) {
            event.reason = "admission_error".into();
        }
        event.message = clip_message(&event.message, &BTreeMap::new());
        eprintln!("{}", event.log_line());
        let mut events = self.events.lock().unwrap();
        events.push_back(event);
        while events.len() > MAX_EVENTS {
            events.pop_front();
        }
        let body = json!({"schema":1,"events":*events});
        if let Err(error) = serde_json::to_vec(&body)
            .map_err(anyhow::Error::from)
            .and_then(|raw| security::write_bytes(&self.root, FILE, &raw, FILE_LIMIT))
        {
            eprintln!(
                "{}",
                json!({"event":"enginebox.launch_events_write_failed","error":clip_message(&format!("{error:#}"), &BTreeMap::new())})
            );
        }
    }
    /// Newest first, optionally filtered.
    pub fn list(&self, filter: &Filter) -> Value {
        let events = self.events.lock().unwrap();
        let matched: Vec<&LaunchEvent> = events
            .iter()
            .rev()
            .filter(|e| filter.matches(e))
            .take(filter.limit.unwrap_or(MAX_EVENTS).min(MAX_EVENTS))
            .collect();
        json!({"events":matched,"retained":events.len(),"max_retained":MAX_EVENTS})
    }
    pub fn len(&self) -> usize {
        self.events.lock().unwrap().len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Owner listing filters (`GET /v1/launch-events`, `/v1/pools/{pool}/launch-events`,
/// `/v1/projects/{project}/launch-events`).
#[derive(Default, Debug)]
pub struct Filter {
    pub pool: Option<String>,
    pub project: Option<String>,
    pub stage: Option<String>,
    pub outcome: Option<String>,
    pub run_id: Option<String>,
    pub limit: Option<usize>,
}
impl Filter {
    fn matches(&self, e: &LaunchEvent) -> bool {
        let same = |want: &Option<String>, have: &Option<String>| {
            want.as_ref().is_none_or(|w| have.as_ref() == Some(w))
        };
        same(&self.pool, &e.pool)
            && same(&self.project, &e.project)
            && same(&self.run_id, &e.run_id)
            && self.stage.as_ref().is_none_or(|s| *s == e.stage)
            && self.outcome.as_ref().is_none_or(|o| *o == e.outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_clipped_single_line_and_redacted() {
        let mut secrets = BTreeMap::new();
        secrets.insert("API_TOKEN".to_owned(), "hunter2-secret".to_owned());
        let raw = "incus failed\nAuthorization: Bearer abcdefghijklmnopqrstuvwxyz0123456789 \
                   token ebst_0123.abcdef value hunter2-secret ok";
        let clipped = clip_message(raw, &secrets);
        assert!(!clipped.contains('\n'));
        assert!(!clipped.contains("hunter2-secret"));
        assert!(!clipped.contains("abcdefghijklmnopqrstuvwxyz0123456789"));
        assert!(!clipped.contains("ebst_0123"));
        assert!(clipped.contains("incus failed"));
        let long = "x ".repeat(400);
        assert!(clip_message(&long, &secrets).chars().count() <= MAX_MESSAGE);
    }

    #[test]
    fn history_is_bounded_persisted_and_filtered() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let events = LaunchEvents::open(&root).unwrap();
        for i in 0..(MAX_EVENTS + 20) {
            events.record(LaunchEvent {
                kind: "pool_member_created".into(),
                outcome: "succeeded".into(),
                pool: Some(if i % 2 == 0 { "even" } else { "odd" }.into()),
                stage: "create".into(),
                reason: "ok".into(),
                ..Default::default()
            });
        }
        assert_eq!(events.len(), MAX_EVENTS);
        let reopened = LaunchEvents::open(&root).unwrap();
        assert_eq!(reopened.len(), MAX_EVENTS);
        let filter = Filter {
            pool: Some("even".into()),
            limit: Some(5),
            ..Default::default()
        };
        let listed = reopened.list(&filter);
        assert_eq!(listed["events"].as_array().unwrap().len(), 5);
        assert!(
            listed["events"]
                .as_array()
                .unwrap()
                .iter()
                .all(|e| e["pool"] == "even")
        );
        let event = LaunchEvent {
            reason: "made-up".into(),
            ..Default::default()
        };
        reopened.record(event);
        assert_eq!(
            reopened.list(&Filter::default())["events"][0]["reason"],
            "admission_error"
        );
    }
}
