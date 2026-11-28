//! Project MCP server settings: list, add, edit, disable, delete and override
//! removal for `.evorch/mcp.toml` (with `.mcp.json` as a read-only import),
//! plus the privileged actions: approval, revocation, connection tests and
//! storing secrets in the OS keychain.
//!
//! All loading, saving and privileged calls go through [`McpGuiService`] on a
//! background executor; the render thread only reads finished results from a
//! channel. Opening or saving settings never launches or connects to a server,
//! and `.mcp.json` is never written: editing an imported server saves a
//! complete override in `mcp.toml`, and disabling it saves a tombstone.
//!
//! Approving a server never changes the tools of a conversation that is
//! already running; it is used from the next new run.

mod draft;
mod review;

pub use draft::{
    DraftPair, DraftTransport, DraftValue, DraftValueKind, McpServerDraft, SecretTarget,
};
