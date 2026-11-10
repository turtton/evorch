//! Tools contracts for web fetch, web search and network guarding.
//!
//! One integration-test binary for the former per-file targets below; each
//! test is now named `<old_file>::<test>`.

// Per-file helper modules (`support`, `common`, ...) are intentionally instantiated
// once per old test file, exactly as when each file was its own binary.
#![allow(clippy::duplicate_mod)]

mod hosted_web_search;
mod network_guard;
mod search_providers;
mod web_fetch_e2e;
mod web_fetch_executor;
mod web_fetch_tool;
mod web_search_flow;
mod web_search_tool;
