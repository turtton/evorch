//! `rusqlite` ベースの single-writer 永続化層です。
//! event-sourced な `events` テーブルを正として projection からセッションを復元します。
//! ADR 0012 に従い、provider / model ごとの usage を 1 分バケットへ downsample します。
//! SQLite は WAL、`synchronous=NORMAL`、`wal_autocheckpoint` で運用します。
//! writer は定期的な PASSIVE checkpoint とハード上限の検査を行います。
//! 起動時にも DB / WAL サイズを検査し、上限到達時の書き込みを制御します。
//! `watch_exclusions` は DB 副ファイルを監視対象から除き、自己参照を防止します。
//! ADR 0008 の credential 非永続化は、公開書き込み API の型付きレコードで保証します。
//!
//! # 公開境界
//!
//! 読み取りは [`Database`]、書き込みは [`StorageHandle`] を使用します。
//!
//! ```
//! let db = storage::Database::open_in_memory()?;
//! let sessions = db.restore_sessions()?;
//! assert!(sessions.is_empty());
//! # Ok::<(), storage::StorageError>(())
//! ```
//!
//! ```compile_fail,E0603
//! use storage::repo;
//! ```
//!
//! ```
//! let db = storage::Database::open_in_memory()?;
//! assert!(db.session("missing")?.is_none());
//! # Ok::<(), storage::StorageError>(())
//! ```
//!
//! ```compile_fail,E0603
//! use storage::repo::session::create;
//! ```
//!
//! ```
//! let db = storage::Database::open_in_memory()?;
//! assert!(db.events_all_ordered()?.is_empty());
//! # Ok::<(), storage::StorageError>(())
//! ```
//!
//! ```compile_fail,E0603
//! use storage::projection::reconcile;
//! ```
//!
//! ```compile_fail,E0616
//! let db = storage::Database::open_in_memory().unwrap();
//! let _ = db.conn;
//! ```
//!
//! ```compile_fail,E0532
//! fn unwrap_handle(handle: storage::StorageHandle) {
//!     let storage::StorageHandle(_) = handle;
//! }
//! ```

pub mod config;
mod db;
pub mod entity;
pub mod error;
mod migrations;
mod projection;
mod read;
mod repo;
mod writer;

pub use config::{HardLimits, LimitKind, StorageConfig};
pub use db::{Database, ns_to_system_time, system_time_to_ns, watch_exclusions};
pub use entity::CatalogUpdateRecord;
pub use error::StorageError;
pub use projection::{ReconcileSummary, SessionSnapshot};
pub use repo::event::StoredEvent;
pub use writer::{Storage, StorageHandle};
