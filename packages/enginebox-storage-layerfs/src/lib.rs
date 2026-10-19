//! Linux checkout storage backend. It never owns provider homes or VM RAM.
pub mod cli;
pub mod config;
pub mod incus;
mod runtime;
pub mod source;

pub const REVISION: &str = "968a5d0be0ea3e9456b06f9148b65523138e9490";
