//! Tools contracts for MCP client sessions, protocol, HTTP transport, secrets and LSP.
//!
//! One integration-test binary for the former per-file targets below; each
//! test is now named `<old_file>::<test>`.

// Per-file helper modules (`support`, `common`, ...) are intentionally instantiated
// once per old test file, exactly as when each file was its own binary.
#![allow(clippy::duplicate_mod)]

mod lsp_client;
mod mcp_client;
mod mcp_contracts;
mod mcp_http;
mod mcp_protocol;
mod mcp_secrets;
mod mcp_session;
mod search_mcp;
