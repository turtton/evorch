//! Storage contracts for credentials, maintenance, migrations and usage accounting.
//!
//! One integration-test binary for the former per-file targets below; each
//! test is now named `<old_file>::<test>`.

// Per-file helper modules (`support`, `common`, ...) are intentionally instantiated
// once per old test file, exactly as when each file was its own binary.
#![allow(clippy::duplicate_mod)]

mod credential_payload;
mod credential_schema;
mod credential_surface;
mod maintenance;
mod migration;
mod raw_usage_guard;
mod statistics;
mod usage_flush;
mod usage_ledger;
