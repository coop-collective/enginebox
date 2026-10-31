//! Native CLI credentials and session execution. No inference proxy or agent loop.
pub mod auth;
pub mod browser;
pub mod clients;
pub mod delegation;
pub mod detached;
pub mod launch_events;
pub mod lifecycle;
pub mod native;
pub mod pools;
pub mod process;
pub mod profile;
pub mod queue;
pub mod repository;
pub mod run_details;
pub mod security;
pub mod service;
pub mod usage;
pub use service::AgentService;

/// A refusal of a request's own fields, worded by a validation rule. Rule
/// messages name the rule, never a value, so the controller shows them to the
/// caller; every other error stays behind its generic reply.
#[derive(Debug)]
pub struct Rule(pub String);
impl std::fmt::Display for Rule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Rule {}
/// Mark a value-free validation error as a [`Rule`].
pub fn rule(error: anyhow::Error) -> anyhow::Error {
    anyhow::Error::new(Rule(error.to_string()))
}
/// `ensure!` for a [`Rule`].
pub fn require(condition: bool, message: &str) -> anyhow::Result<()> {
    if condition {
        Ok(())
    } else {
        Err(anyhow::Error::new(Rule(message.to_owned())))
    }
}
pub const PROVIDERS: [&str; 3] = ["codex", "claude", "cursor"];
pub fn provider(value: &str) -> anyhow::Result<&str> {
    anyhow::ensure!(PROVIDERS.contains(&value), "Unknown native agent provider.");
    Ok(value)
}
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}
