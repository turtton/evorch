//! Providers contracts for Codex OAuth, model catalogs, compaction, refresh and quota.
//!
//! One integration-test binary for the former per-file targets below; each
//! test is now named `<old_file>::<test>`.

// Per-file helper modules (`support`, `common`, ...) are intentionally instantiated
// once per old test file, exactly as when each file was its own binary.
#![allow(clippy::duplicate_mod)]

mod codex_browser_oauth_contract;
mod codex_compaction;
mod codex_contract;
mod codex_empty_deltas;
mod codex_models_contract;
mod codex_oauth_contract;
mod codex_quota;
mod codex_refresh_contract;
mod codex_usage_missing;
