//! Public restart adapter. Capability/production installation belongs to 5a-V.
#[cfg(target_os = "linux")]
use super::{RestartCoordinator, RestartId, RestartPhase, State};
#[cfg(target_os = "linux")]
#[path = "public/implementation.rs"]
mod implementation;
#[cfg(target_os = "linux")]
pub use implementation::*;
#[cfg(not(target_os = "linux"))]
#[path = "public/unsupported.rs"]
mod unsupported;
#[cfg(not(target_os = "linux"))]
pub use unsupported::*;
