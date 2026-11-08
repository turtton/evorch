//! ストレージ層で返すエラーを定義します。

use std::fmt;

use crate::LimitKind;

pub use secret_guard::SecretRule;

#[derive(Debug)]
struct IoSource;

impl fmt::Display for IoSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "I/O error")
    }
}

impl std::error::Error for IoSource {}

static IO_SOURCE: IoSource = IoSource;

/// Why a body could not be resolved or stored. No variant carries body text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyUnresolvedReason {
    /// A string in a token position is not a well-formed token.
    InvalidToken,
    /// A well-formed token of a version this build does not know.
    UnknownTokenVersion,
    /// No blob has the token's hash.
    BlobMissing,
    /// The blob (or the token) declares a length that the other does not match.
    LengthMismatch { expected: u64, actual: u64 },
    /// The blob declares more bytes than a blob may hold.
    DeclaredLengthTooLarge { declared: u64 },
    /// The `codec` column holds a value this build does not know.
    UnknownCodec(i64),
    /// The stored bytes could not be decompressed to the declared length.
    DecompressFailed,
    /// The stored bytes are not UTF-8.
    InvalidUtf8,
    /// The row's `data` is not a blob (another SQL type): the row was altered.
    MalformedRow,
    /// The stored bytes do not hash to the blob's `hash` (found by verification).
    HashMismatch,
    /// A different text with the same hash but another length is already stored: a
    /// collision or a corrupt row. The storing transaction must be rolled back.
    HashLengthConflict { stored: u64, new: u64 },
}

impl fmt::Display for BodyUnresolvedReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidToken => write!(formatter, "malformed body token"),
            Self::UnknownTokenVersion => write!(formatter, "body token of an unknown version"),
            Self::BlobMissing => write!(formatter, "blob is missing"),
            Self::LengthMismatch { expected, actual } => {
                write!(
                    formatter,
                    "length mismatch (expected {expected}, found {actual})"
                )
            }
            Self::DeclaredLengthTooLarge { declared } => {
                write!(formatter, "declared length {declared} is too large")
            }
            Self::UnknownCodec(codec) => write!(formatter, "unknown codec {codec}"),
            Self::DecompressFailed => write!(formatter, "decompression failed"),
            Self::InvalidUtf8 => write!(formatter, "stored bytes are not UTF-8"),
            Self::MalformedRow => write!(formatter, "blob row holds a value of the wrong type"),
            Self::HashMismatch => write!(formatter, "stored bytes do not match the hash"),
            Self::HashLengthConflict { stored, new } => write!(
                formatter,
                "hash is already stored with length {stored}, not {new}"
            ),
        }
    }
}

/// Why a lane request collides with what the ledger already holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneConflictReason {
    /// The creation key was used before for a different request.
    CreationKeyReused,
    /// The lane ID or its thread ID belongs to another lane.
    IdentityTaken,
    /// The thread already exists with a lineage that is not a lane.
    ThreadNotLane,
    /// The creation intent was cancelled or failed before its first root started.
    StartBlocked,
}

/// Why a `lane_list` cursor cannot continue a listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneCursorError {
    /// The snapshot the cursor names is no longer kept. List again from the start.
    Stale,
    /// The cursor was issued for another filter.
    FilterMismatch,
    /// Not a cursor this ledger issued.
    Malformed,
}

/// ストレージ操作で発生するエラーです。
// `rusqlite::Error` が `Eq` を実装しないため、ワークスペースの慣例に反して `Eq` は導出しません。
#[derive(Debug, PartialEq)]
pub enum StorageError {
    QuestionCapacity(crate::lane_delivery::QuestionCapacity),
    LaneDeliveryConflict(&'static str),
    LaneOperation(event_bus::lane::LaneOperationError),
    LaneManagement(crate::lane_management::LaneManagementError),
    StaleMutation,
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
    /// The database still has the format that stored a run's messages as one
    /// column of `run_contexts` (or a conversion stopped half way). This build never
    /// opens it: every evorch process must be stopped and the conversion command
    /// run. Never retryable; the database is left untouched.
    ConversionRequired {
        /// `PRAGMA user_version` found, or 0 when the shape (not the version) is old.
        found: u32,
        /// What is old.
        detail: &'static str,
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
    /// Run note revisions must contain at most 8192 UTF-8 bytes.
    InvalidRunLedgerBody {
        bytes: usize,
    },
    /// The reference ID is already bound to different immutable content.
    AssistantReferenceConflict,
    /// The reference does not exist or its approval was already consumed.
    AssistantReferenceUnavailable,
    /// The question was already answered by someone else. Only reported to a caller that
    /// asked to be the first to answer ([`crate::AnswerMode::FirstOnly`]).
    QuestionAlreadyAnswered,
    /// The run or thread was deleted by a confirmed retention (tombstoned), so
    /// nothing may be recorded for it any more. Not retryable.
    Tombstoned {
        run_id: Option<String>,
        thread_id: Option<String>,
    },
    /// The lesson is a favorite (the user's approval mark), so it cannot be rejected
    /// until the mark is cleared.
    LessonFavorite {
        /// The protected lesson.
        id: String,
    },
    /// A body token could not be turned back into its text, or a body could not be
    /// stored consistently. Never a soft failure: the caller must not substitute an
    /// empty or placeholder text, nor show the raw token as if it were the body.
    BodyUnresolved {
        /// Where the token was found, for example `message run=r1 ordinal=3`.
        owner: String,
        /// Lowercase hex SHA-256 of the body, when the token (or blob) names one.
        hash: Option<String>,
        reason: BodyUnresolvedReason,
    },
    /// The lane request was refused: empty or oversized input. Never silently cut.
    LaneInvalid(event_bus::LaneValidationError),
    /// The lane does not exist (or its thread was deleted by retention).
    LaneNotFound {
        lane_id: String,
    },
    /// The caller worked from an older state: re-read and retry. `kind` names the
    /// counter (`management`, `brief` or `generation`).
    StaleRevision {
        kind: &'static str,
        expected: u64,
        current: u64,
    },
    /// The lane request collides with existing lane data.
    LaneConflict(LaneConflictReason),
    /// The `lane_list` cursor cannot be used (`stale_cursor` and its relatives).
    LaneCursor(LaneCursorError),
    /// The pull request request was refused (ADR 0043): malformed input or a state the
    /// call cannot apply to. A stale revision is [`Self::StaleRevision`].
    PullRequest(crate::pull_requests::PullRequestRejection),
    /// Durable PR notice/policy transition was refused.
    PrAutoFix(crate::pull_request_auto_fix::PrAutoFixRejection),
    /// 入出力操作が失敗しました。
    Io(String),
    /// 永続化 ingress の heuristic secret guard が credential らしき値を検出し、
    /// 保存を拒否しました。
    ///
    /// 診断には entity 種別・field 名・検出規則のみを含めます。値本体・前後
    /// コンテキストはもちろん、決定的ハッシュのような「拒否された値が何か」を
    /// 外部から照合確認できる表現も一切含みません。
    SecretDetected {
        /// 検出対象の entity 種別（`message` / `event`）です。
        entity: &'static str,
        /// 検出対象の field（`content` / `reasoning` / `MessageDelta.delta` 等）です。
        field: &'static str,
        /// 一致した検出規則です。
        rule: SecretRule,
    },
}

impl fmt::Display for StorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QuestionCapacity(capacity) => write!(
                formatter,
                "question capacity {capacity:?} reached ({})",
                capacity.limit()
            ),
            Self::LaneManagement(reason) => write!(formatter, "{}", reason.as_str()),
            Self::LaneOperation(reason) => write!(formatter, "{}", reason.as_str()),
            Self::LaneDeliveryConflict(reason) => {
                write!(formatter, "lane delivery conflict: {reason}")
            }
            Self::StaleMutation => write!(formatter, "mutation owner generation is stale"),
            Self::Sqlite(error) => write!(formatter, "SQLite error: {error}"),
            Self::Migration { version, message } => {
                write!(formatter, "migration {version} failed: {message}")
            }
            Self::SchemaTooNew { found, supported } => write!(
                formatter,
                "schema version {found} is newer than supported version {supported}"
            ),
            Self::ConversionRequired { found, detail } => write!(
                formatter,
                "the database (schema version {found}) uses the old conversation storage \
                 ({detail}); stop every evorch process and run the conversion command \
                 (`evorch maintenance convert-bodies`, shipped with the release that \
                 includes this storage format) before starting this build. Nothing was changed"
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
            Self::InvalidRunLedgerBody { bytes } => write!(
                formatter,
                "run ledger body must contain 0..=8192 bytes: actual={bytes}"
            ),
            Self::AssistantReferenceConflict => write!(
                formatter,
                "assistant reference content conflicts with its immutable ID"
            ),
            Self::AssistantReferenceUnavailable => write!(
                formatter,
                "assistant reference is missing or already consumed"
            ),
            Self::QuestionAlreadyAnswered => write!(formatter, "question already answered"),
            Self::Tombstoned { run_id, thread_id } => write!(
                formatter,
                "run {} / thread {} was deleted by retention",
                run_id.as_deref().unwrap_or("-"),
                thread_id.as_deref().unwrap_or("-")
            ),
            Self::LessonFavorite { id } => write!(
                formatter,
                "lesson {id} is a favorite; clear the approval mark before rejecting it"
            ),
            Self::BodyUnresolved {
                owner,
                hash,
                reason,
            } => write!(
                formatter,
                "body unresolved at {owner} (hash {}): {reason}",
                hash.as_deref().unwrap_or("unknown")
            ),
            Self::LaneInvalid(error) => write!(formatter, "invalid lane input: {error}"),
            Self::LaneNotFound { lane_id } => write!(formatter, "lane {lane_id} was not found"),
            Self::StaleRevision {
                kind,
                expected,
                current,
            } => write!(
                formatter,
                "stale_revision: {kind} revision {expected} was expected but the lane is at {current}"
            ),
            Self::LaneConflict(reason) => match reason {
                LaneConflictReason::CreationKeyReused => write!(
                    formatter,
                    "the creation key was already used for a different lane request"
                ),
                LaneConflictReason::IdentityTaken => {
                    write!(formatter, "the lane or thread ID belongs to another lane")
                }
                LaneConflictReason::StartBlocked => {
                    write!(formatter, "the lane creation intent is blocked")
                }
                LaneConflictReason::ThreadNotLane => {
                    write!(formatter, "the thread already exists and is not a lane")
                }
            },
            Self::LaneCursor(reason) => match reason {
                LaneCursorError::Stale => write!(
                    formatter,
                    "stale_cursor: the listing it continues is no longer kept; list again"
                ),
                LaneCursorError::FilterMismatch => {
                    write!(formatter, "the cursor was issued for a different filter")
                }
                LaneCursorError::Malformed => write!(formatter, "the cursor is not valid"),
            },
            Self::PullRequest(reason) => write!(formatter, "{reason}"),
            Self::PrAutoFix(reason) => write!(formatter, "{reason}"),
            Self::Io(message) => write!(formatter, "I/O error: {message}"),
            Self::SecretDetected {
                entity,
                field,
                rule,
            } => write!(
                formatter,
                "secret guard rejected {entity} field {field}: rule={rule} \
                 (heuristic defense-in-depth; not a complete secret-leak guarantee; ADR 0008)"
            ),
        }
    }
}

/// What kind of failure a [`StorageError`] is, for callers that report it without
/// depending on SQLite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// The file is not a database, or is damaged.
    Corrupt,
    /// The database's schema is newer or older than this build understands.
    SchemaUnsupported,
    /// The operating system or SQLite refused access.
    Permission,
    /// Another connection holds the database; trying again later can succeed.
    Busy,
    /// A size or row limit was exceeded.
    Limit,
    /// The database could not be opened, for a reason SQLite does not say.
    CannotOpen,
    Other,
}

impl StorageError {
    pub fn class(&self) -> FailureClass {
        use rusqlite::ErrorCode;
        match self {
            // A database that needs the conversion command is older than this build
            // reads, like any other older schema.
            Self::SchemaTooNew { .. } | Self::ConversionRequired { .. } => {
                FailureClass::SchemaUnsupported
            }
            Self::Serialization(message) if message.contains(crate::inspect::OLDER_SCHEMA) => {
                FailureClass::SchemaUnsupported
            }
            Self::LimitExceeded { .. } | Self::QuestionCapacity(_) => FailureClass::Limit,
            Self::Sqlite(error) => match error.sqlite_error_code() {
                Some(ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt) => FailureClass::Corrupt,
                Some(
                    ErrorCode::PermissionDenied
                    | ErrorCode::ReadOnly
                    | ErrorCode::AuthorizationForStatementDenied,
                ) => FailureClass::Permission,
                Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => FailureClass::Busy,
                Some(ErrorCode::CannotOpen) => FailureClass::CannotOpen,
                _ => FailureClass::Other,
            },
            _ => FailureClass::Other,
        }
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Io(_) => Some(&IO_SOURCE),
            Self::QuestionCapacity(_)
            | Self::LaneManagement(_)
            | Self::LaneOperation(_)
            | Self::LaneDeliveryConflict(_)
            | Self::Migration { .. }
            | Self::StaleMutation
            | Self::SchemaTooNew { .. }
            | Self::ConversionRequired { .. }
            | Self::LimitExceeded { .. }
            | Self::WriterClosed
            | Self::RawUsageEventNotPersisted
            | Self::Serialization(_)
            | Self::OutOfRange(_)
            | Self::InvalidRunLedgerBody { .. }
            | Self::AssistantReferenceConflict
            | Self::AssistantReferenceUnavailable
            | Self::QuestionAlreadyAnswered
            | Self::Tombstoned { .. }
            | Self::LessonFavorite { .. }
            | Self::BodyUnresolved { .. }
            | Self::LaneInvalid(_)
            | Self::LaneNotFound { .. }
            | Self::StaleRevision { .. }
            | Self::LaneConflict(_)
            | Self::LaneCursor(_)
            | Self::PrAutoFix(_)
            | Self::PullRequest(_)
            | Self::SecretDetected { .. } => None,
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
