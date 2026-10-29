//! Usage metering boundary (docs/ORGS.md, "Extension points").
//!
//! The core emits one [`UsageEvent`] per metered fact, tagged with the org,
//! and (when known) the credential and run. It never prices, aggregates or
//! bills: a managed deployment supplies its own [`UsageSink`] (for example in
//! a private crate that wraps this controller) through
//! [`crate::Controller::with_usage_sink`]. The default sink discards events, so
//! a self-hosted install has no metering dependency.
use serde::Serialize;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageKind {
    /// A run was admitted (queued or started) in a pool or on a workspace.
    RunAdmitted,
}

#[derive(Clone, Debug, Serialize)]
pub struct UsageEvent {
    pub kind: UsageKind,
    /// The org the usage is charged to.
    pub org_id: String,
    /// The org whose capacity served it (differs under a capacity grant).
    pub capacity_org: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grant_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// The agent credential the run used, once runs select credentials.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    pub started_by: String,
    pub at: String,
}

/// Receives usage events. Implementations must not block for long: events are
/// recorded on the request path after the run was admitted, and a failure to
/// record never fails the run.
pub trait UsageSink: Send + Sync {
    fn record(&self, event: &UsageEvent);
}

/// The default: discard.
pub struct NoopSink;
impl UsageSink for NoopSink {
    fn record(&self, _event: &UsageEvent) {}
}

pub fn noop() -> Arc<dyn UsageSink> {
    Arc::new(NoopSink)
}
