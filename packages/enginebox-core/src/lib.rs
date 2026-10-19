//! Native fleet operations. No provider loop, credentials, or Python runtime lives here.
pub mod bootstrap;
pub mod capabilities;
pub mod capacity;
pub mod fleet;
pub mod guest;
pub mod model;
pub mod state;
pub mod templates;
pub mod transport;
pub mod warm;
pub use capacity::{capacity, resource_fits};
pub use fleet::Fleet;
pub use model::{GuestConfig, Host, Resources, identifier, load_hosts};
pub use transport::{EngineBox, HostOperationLock, host_run, operation_lock};
pub const GIB: u64 = 1024 * 1024 * 1024;
