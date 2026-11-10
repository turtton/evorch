//! Providers contracts for Anthropic reasoning, usage, images, observation and streaming wire shapes.
//!
//! One integration-test binary for the former per-file targets below; each
//! test is now named `<old_file>::<test>`.

// Per-file helper modules (`support`, `common`, ...) are intentionally instantiated
// once per old test file, exactly as when each file was its own binary.
#![allow(clippy::duplicate_mod)]

mod anthropic_contract;
mod anthropic_reasoning;
mod claude_extra_usage;
mod image_blocks;
mod images;
mod observation_contract;
mod service_tier;
mod streaming_completion;
mod streaming_retry;
mod structured_output;
