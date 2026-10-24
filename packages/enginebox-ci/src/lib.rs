//! Optional CI execution. No agent loop and no dependency from the core runtime.
pub mod executor;
pub mod github;
pub mod model;
pub mod provision;
pub mod templates;
pub use model::{Job, Node, Request};
