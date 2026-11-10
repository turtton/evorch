//! Storage contracts for runs, events, messages, run contexts and run-scoped read models.
//!
//! One integration-test binary for the former per-file targets below; each
//! test is now named `<old_file>::<test>`.

mod accounting_aggregates;
mod agent_messages;
mod answer_review_evidence;
mod artifact_refs;
mod catalog_history;
mod compressed_run_contexts;
mod deferred_run_reader;
mod eval_sample;
mod events_after;
mod findings;
mod handoffs;
mod immutable_read;
mod read_api;
mod resume;
mod review_evidence;
mod run_context_redaction;
mod run_contexts;
mod run_ledger;
mod run_messages;
mod stopped;
mod task_progress_retention;
mod task_queue;
mod undecodable_events;
mod user_questions;
mod writer_commands;
