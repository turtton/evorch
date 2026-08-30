//! ストレージ層で返すエラーを定義します。

use std::fmt;

use crate::LimitKind;

#[derive(Debug)]
struct IoSource;

impl fmt::Display for IoSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "I/O error")
    }
}

impl std::error::Error for IoSource {}

static IO_SOURCE: IoSource = IoSource;

/// ストレージ操作で発生するエラーです。
// `rusqlite::Error` が `Eq` を実装しないため、ワークスペースの慣例に反して `Eq` は導出しません。
#[derive(Debug, PartialEq)]
pub enum StorageError {
    /// SQLite 操作が失敗しました。
    Sqlite(rusqlite::Error),
    /// スキーマ移行が失敗しました。
    Migration {
        /// 失敗した移行バージョンです。
        version: u32,
        /// 失敗理由です。
        message: String,
    },
    /// データベースのスキーマがサポート範囲より新しい状態です。
    SchemaTooNew {
        /// データベースから検出したスキーマバージョンです。
        found: u32,
        /// このクレートがサポートする最大バージョンです。
        supported: u32,
    },
    /// 容量上限を超過しました。
    LimitExceeded {
        /// 超過した上限の種別です。
        limit: LimitKind,
        /// 実測した値です。
        actual: u64,
        /// 許容する最大値です。
        max: u64,
    },
    /// 非同期書き込み担当が終了しました。
    WriterClosed,
    /// raw usage event はイベントログへ永続化できません。
    RawUsageEventNotPersisted,
    /// シリアライズまたはデシリアライズが失敗しました。
    Serialization(String),
    /// 値が許容範囲外でした。
    OutOfRange(&'static str),
    /// 入出力操作が失敗しました。
    Io(String),
}

impl fmt::Display for StorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "SQLite error: {error}"),
            Self::Migration { version, message } => {
                write!(formatter, "migration {version} failed: {message}")
            }
            Self::SchemaTooNew { found, supported } => write!(
                formatter,
                "schema version {found} is newer than supported version {supported}"
            ),
            Self::LimitExceeded { limit, actual, max } => {
                write!(
                    formatter,
                    "limit {limit:?} exceeded: actual={actual}, max={max}"
                )
            }
            Self::WriterClosed => write!(formatter, "storage writer is closed"),
            Self::RawUsageEventNotPersisted => write!(
                formatter,
                "raw usage events are not persisted; submit downsampled usage through UsageSink (ADR 0012)"
            ),
            Self::Serialization(message) => write!(formatter, "serialization failed: {message}"),
            Self::OutOfRange(name) => write!(formatter, "value out of range: {name}"),
            Self::Io(message) => write!(formatter, "I/O error: {message}"),
        }
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Io(_) => Some(&IO_SOURCE),
            Self::Migration { .. }
            | Self::SchemaTooNew { .. }
            | Self::LimitExceeded { .. }
            | Self::WriterClosed
            | Self::RawUsageEventNotPersisted
            | Self::Serialization(_)
            | Self::OutOfRange(_) => None,
        }
    }
}

impl From<rusqlite::Error> for StorageError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LimitKind;

    #[test]
    fn display_includes_key_context_for_domain_errors() {
        // Given: 移行、スキーマ、容量上限の各エラー
        let cases = [
            (
                StorageError::Migration {
                    version: 3,
                    message: "failed".into(),
                },
                "migration 3",
            ),
            (
                StorageError::SchemaTooNew {
                    found: 4,
                    supported: 3,
                },
                "schema version 4",
            ),
            (
                StorageError::LimitExceeded {
                    limit: LimitKind::EventSize,
                    actual: 11,
                    max: 10,
                },
                "EventSize",
            ),
        ];

        // When: 各エラーを表示形式へ変換する
        // Then: 原因を識別できる主要情報を含む
        for (error, expected) in cases {
            assert!(error.to_string().contains(expected));
        }
    }

    #[test]
    fn from_rusqlite_error_wraps_sqlite_variant() {
        // Given: rusqlite が返すエラー
        let sqlite_error = rusqlite::Error::InvalidQuery;

        // When: ストレージエラーへ変換する
        let error = StorageError::from(sqlite_error);

        // Then: SQLite エラーのバリアントへ保持される
        assert!(matches!(
            error,
            StorageError::Sqlite(rusqlite::Error::InvalidQuery)
        ));
    }

    #[test]
    fn limit_exceeded_compares_all_fields() {
        // Given: 同じ容量上限超過を表す二つのエラー
        let first = StorageError::LimitExceeded {
            limit: LimitKind::DbSize,
            actual: 101,
            max: 100,
        };
        let second = StorageError::LimitExceeded {
            limit: LimitKind::DbSize,
            actual: 101,
            max: 100,
        };

        // When: エラー値を比較する
        // Then: 全フィールドが等しい値として扱われる
        assert_eq!(first, second);
    }
}
