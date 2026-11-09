//! Project MCP configuration contracts. Loading/editing never launches or connects.
//! Shared types and exports are owned by Worker E; implementation modules by A.
pub mod expand;
pub mod load;
pub mod save;
mod types;
pub mod validate;
pub use load::load_project;
pub use save::save_override;
pub use types::*;
