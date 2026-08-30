//! `rusqlite` ベースの single-writer 永続化層です。
//! event-sourced な `events` テーブルを正として projection からセッションを復元します。
//! ADR 0012 に従い、provider / model ごとの usage を 1 分バケットへ downsample します。
//! SQLite は WAL、`synchronous=NORMAL`、`wal_autocheckpoint` で運用します。
//! writer は定期的な PASSIVE checkpoint とハード上限の検査を行います。
//! 起動時にも DB / WAL サイズを検査し、上限到達時の書き込みを制御します。
//! `watch_exclusions` は DB 副ファイルを監視対象から除き、自己参照を防止します。
//! ADR 0008 の credential 非永続化は、公開書き込み API の型付きレコードで保証します。

pub mod config;
pub mod db;
pub mod entity;
pub mod error;
pub mod migrations;
pub mod projection;
pub mod repo;
mod writer;

pub use config::{HardLimits, LimitKind, StorageConfig};
pub use db::{Database, watch_exclusions};
pub use entity::CatalogUpdateRecord;
pub use error::StorageError;
pub use projection::ReconcileSummary;
pub use writer::{Storage, StorageHandle};
