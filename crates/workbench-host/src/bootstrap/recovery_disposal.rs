//! Owned-graph management. Read models are not execution or disposal authority.
mod view;
pub use view::{binding_for, collect_undisposed, detail};
mod facade;
pub use facade::{DiagnosticsPort, OwnedGraphsFacade};
mod snapshot;
