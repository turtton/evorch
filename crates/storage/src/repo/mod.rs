//! 永続化エンティティごとのリポジトリを集約します。
//!
//! 各 repo 関数は single-writer 管理下の `Connection` でのみ使用すること。
//! 同一 DB ファイルを storage 外から直接開くと ADR 0012 の single-writer 規約が破綻します。

pub mod agent_run;
pub mod catalog;
pub mod event;
pub mod message;
pub mod metrics;
pub mod session;
pub mod task;

#[cfg(test)]
mod credential_tests;
#[cfg(test)]
mod crud_tests;
