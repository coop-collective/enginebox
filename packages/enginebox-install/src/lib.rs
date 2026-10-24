//! Versioned, integrity-checked installs and a stable controller front door.
pub mod bootstrap;
pub mod native_node;
pub mod release;
pub mod supervisor;
#[cfg(windows)]
mod windows_acl;
