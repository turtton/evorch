//! 永続化エンティティごとのリポジトリを集約します。
//!
//! 各 repo 関数は single-writer 管理下の `Connection` でのみ使用すること。
//! 同一 DB ファイルを storage 外から直接開くと ADR 0012 の single-writer 規約が破綻します。

pub mod agent_run;
pub(crate) mod assistant_reference;
pub mod catalog;
pub(crate) mod eval_sample;
pub mod event;
pub(crate) mod host_input_receipt;
pub(crate) mod memory;
pub mod message;
pub mod metrics;
pub(crate) mod review_evidence;
pub(crate) mod review_lineage;
pub mod run_context;
pub mod run_ledger;
pub mod session;
pub mod task;
pub(crate) mod thread_index;
#[cfg(test)]
mod thread_index_tests;
pub(crate) mod thread_retention;
pub(crate) mod thread_start;
pub(crate) mod threads;
pub(crate) mod usage;
pub(crate) mod user_questions;

#[cfg(test)]
mod crud_tests;
