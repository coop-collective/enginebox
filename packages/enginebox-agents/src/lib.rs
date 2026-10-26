//! Native CLI credentials and session execution. No inference proxy or agent loop.
pub mod auth;
pub mod browser;
pub mod clients;
pub mod delegation;
pub mod native;
pub mod pools;
pub mod process;
pub mod profile;
pub mod queue;
pub mod run_details;
pub mod security;
pub mod service;
pub mod usage;
pub use service::AgentService;
pub const PROVIDERS: [&str; 3] = ["codex", "claude", "cursor"];
pub fn provider(value: &str) -> anyhow::Result<&str> {
    anyhow::ensure!(PROVIDERS.contains(&value), "Unknown native agent provider.");
    Ok(value)
}
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}
