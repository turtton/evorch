//! Storage contracts for threads, lanes, pull requests, memory, retention and host inputs.
//!
//! One integration-test binary for the former per-file targets below; each
//! test is now named `<old_file>::<test>`.

mod diagnostic;
mod diagnostic_cleanup;
mod durable_work_list;
mod host_chat_submissions;
mod host_input_receipts;
mod lanes;
mod lesson_batch;
mod memory;
mod memory_favorite;
mod ownership;
mod pull_requests;
mod thread_index_contracts;
mod thread_retention;
mod thread_runs;
mod thread_start;
mod threads;
