//! Providers contracts for OpenAI-compatible, Kimi and model-listing transports.
//!
//! One integration-test binary for the former per-file targets below; each
//! test is now named `<old_file>::<test>`.

// Per-file helper modules (`support`, `common`, ...) are intentionally instantiated
// once per old test file, exactly as when each file was its own binary.
#![allow(clippy::duplicate_mod)]

mod kimi_quota;
mod kimi_reasoning;
mod list_models_contract;
mod openai_compatible_contract;
mod openai_contract;
mod openai_output_limits;
mod openai_sse_failures;
mod openai_timeouts;
mod verify_connectivity;
