//! Reading events without their long bodies: the API of a reader that shows a body only
//! when it is on screen.
//!
//! [`for_each_event_ordered_deferred`] hands over every event of the log in `id` order, as
//! a [`DeferredStoredEvent`] or, when its row cannot be read, as a [`StoredEventFailure`]
//! in the same sequence. A tokenized event keeps the **token strings** where its long
//! strings were; [`resolve_bodies`] turns tokens into text, a batch at a time. The two
//! types here are not [`StoredEvent`]: a token must never reach code that treats a string
//! as the text it holds.
//!
//! A caller takes a token position only from a [`DeferredStoredEvent`] whose
//! [`format`](DeferredStoredEvent::format) is [`StoredFormat::Tokenized`]. In an inline
//! event (and in any live event) a string that looks like a token is only text.

use std::collections::BTreeMap;
use std::time::SystemTime;

use event_bus::Event;
use rusqlite::{Connection, ErrorCode, Row};

use super::{BODY_FORMAT_COLUMN, StoredFormat, row_meta};
use crate::StorageError;
use crate::body_ref::BodyToken;
use crate::db::ns_to_system_time;
use crate::error::BodyUnresolvedReason;
use crate::thread_retention::TombstoneSet;

/// The most distinct bodies one [`resolve_bodies`] call resolves.
pub const MAX_RESOLVE_REFS: usize = 256;

/// The identity of a stored body: the SHA-256 of its UTF-8 bytes and their count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BodyRef {
    pub hash: [u8; 32],
    /// Bytes of the text the reference stands for.
    pub byte_len: u64,
}

impl BodyRef {
    /// Reads a string of a [`StoredFormat::Tokenized`] event's token position. `None` when
    /// it is not a well-formed token of a version this build knows.
    #[must_use]
    pub fn from_token(value: &str) -> Option<Self> {
        BodyToken::parse(value).ok().map(Self::from)
    }

    /// The hash as lowercase hex, as `evorch inspect blob` takes it.
    #[must_use]
    pub fn hash_hex(&self) -> String {
        crate::body_ref::hex(&self.hash)
    }
}

impl From<BodyToken> for BodyRef {
    fn from(token: BodyToken) -> Self {
        Self {
            hash: token.hash,
            byte_len: token.byte_len,
        }
    }
}

/// An event whose long strings are still tokens (when `format` says it holds any).
///
/// Deliberately not a [`StoredEvent`](super::StoredEvent): it cannot be passed to code that
/// reads `event` as text.
#[derive(Debug, Clone, PartialEq)]
pub struct DeferredStoredEvent {
    pub id: i64,
    pub session_id: Option<String>,
    /// The `run_id` column.
    pub run_id: Option<String>,
    /// The `kind` column.
    pub kind: String,
    /// The `subkind` column (`None` before the event backfill reaches the row).
    pub subkind: Option<String>,
    /// The event with its long strings as tokens when `format` is
    /// [`StoredFormat::Tokenized`].
    pub event: Event,
    /// The stored form, from the `body_format` column: the only thing that says whether a
    /// string of `event` is a token.
    pub format: StoredFormat,
}

/// Why a row of the log is not an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredEventFailureReason {
    /// A column around the payload is unusable (a time, the schema version, the format).
    Row(String),
    /// The payload cannot be read: not text, not a zstd frame, not UTF-8.
    Payload(String),
    /// The payload is not JSON, or not an event of this build (a variant removed since).
    Variant(String),
}

/// A row that could not be turned into an event, with what its columns still say. It takes
/// the place of the event in the sequence.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredEventFailure {
    pub id: i64,
    pub run_id: Option<String>,
    pub wall_clock: Option<SystemTime>,
    pub kind: Option<String>,
    pub subkind: Option<String>,
    pub reason: StoredEventFailureReason,
}

/// One row of the log, in `id` order.
#[derive(Debug, Clone, PartialEq)]
pub enum DeferredRow {
    Event(Box<DeferredStoredEvent>),
    Failed(StoredEventFailure),
}

/// Every event with `id <= through_id` in `id` order, successes and failures in one
/// sequence. The rows of a deleted run or thread (tombstoned) are not handed over, as in
/// the other readers. No blob is read.
///
/// # Errors
/// Fails only when the query itself fails.
pub fn for_each_event_ordered_deferred(
    conn: &Connection,
    through_id: i64,
    mut visit: impl FnMut(DeferredRow),
) -> Result<(), StorageError> {
    let tombstones = TombstoneSet::load(conn)?;
    let mut statement = conn.prepare(
        "SELECT id, session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload, \
         body_format, subkind, run_id FROM events WHERE id <= ?1 ORDER BY id ASC",
    )?;
    let mut rows = statement.query([through_id])?;
    while let Some(row) = rows.next()? {
        let run_id: Option<String> = row.get(9).ok().flatten();
        match deferred_row(row) {
            Ok(event) if tombstones.rejects(&event.event.kind).is_some() => {}
            Ok(event) => visit(DeferredRow::Event(Box::new(event))),
            Err(reason) => {
                if run_id
                    .as_deref()
                    .is_some_and(|run| tombstones.contains_run(run))
                {
                    continue;
                }
                visit(DeferredRow::Failed(StoredEventFailure {
                    id: row.get(0)?,
                    run_id,
                    wall_clock: row.get(4).ok().map(ns_to_system_time),
                    kind: row.get(5).ok(),
                    subkind: row.get(8).ok().flatten(),
                    reason,
                }));
            }
        }
    }
    Ok(())
}

fn deferred_row(row: &Row<'_>) -> Result<DeferredStoredEvent, StoredEventFailureReason> {
    use StoredEventFailureReason::{Payload, Row as RowReason, Variant};
    let meta = row_meta(row).map_err(|error| RowReason(error.to_string()))?;
    let format = row
        .get::<_, i64>(BODY_FORMAT_COLUMN)
        .map_err(|error| RowReason(error.to_string()))
        .and_then(|value| {
            StoredFormat::from_column(value).map_err(|error| RowReason(error.to_string()))
        })?;
    let payload = row
        .get_ref(6)
        .map_err(|error| Payload(error.to_string()))
        .and_then(|value| {
            crate::codec::decode(value).map_err(|error| Payload(error.to_string()))
        })?;
    let kind = serde_json::from_str(&payload).map_err(|error| Variant(error.to_string()))?;
    Ok(DeferredStoredEvent {
        id: row.get(0).map_err(|error| RowReason(error.to_string()))?,
        session_id: row.get(1).map_err(|error| RowReason(error.to_string()))?,
        run_id: row.get(9).map_err(|error| RowReason(error.to_string()))?,
        kind: row.get(5).map_err(|error| RowReason(error.to_string()))?,
        subkind: row.get(8).map_err(|error| RowReason(error.to_string()))?,
        event: Event { meta, kind },
        format,
    })
}

/// What went wrong with one body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyFailureClass {
    /// No blob has the hash (or the length differs): it will not come back.
    Missing,
    /// The blob is there and cannot be read back as the text it declares.
    Corrupt,
    /// The database was busy or locked: trying again may work.
    Busy,
    /// Another failure of the database: trying again may work.
    Io,
}

/// A body that could not be resolved. It never carries body text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyFailure {
    pub class: BodyFailureClass,
    pub reason: String,
}

impl BodyFailure {
    /// Whether a later attempt can succeed ([`Busy`](BodyFailureClass::Busy) and
    /// [`Io`](BodyFailureClass::Io)).
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(self.class, BodyFailureClass::Busy | BodyFailureClass::Io)
    }
}

/// The text of each of `refs`, in the order given, in one read snapshot. A body that
/// cannot be resolved is a [`BodyFailure`] of its own; the others of the batch are still
/// returned. A reference repeated in `refs` is read once.
///
/// # Errors
/// Fails, for the whole call, when `refs` holds more than [`MAX_RESOLVE_REFS`] distinct
/// references or the read snapshot cannot be opened.
pub fn resolve_bodies(
    conn: &Connection,
    refs: &[BodyRef],
) -> Result<Vec<Result<String, BodyFailure>>, StorageError> {
    let mut distinct: BTreeMap<BodyRef, Option<Result<String, BodyFailure>>> = BTreeMap::new();
    for reference in refs {
        distinct.insert(*reference, None);
    }
    if distinct.len() > MAX_RESOLVE_REFS {
        return Err(StorageError::OutOfRange("body references per call"));
    }
    let transaction = conn.unchecked_transaction()?;
    for (reference, slot) in &mut distinct {
        let token = BodyToken {
            hash: reference.hash,
            byte_len: reference.byte_len,
        };
        *slot =
            Some(crate::body_store::load_blob(&transaction, &token, "event body").map_err(failure));
    }
    Ok(refs
        .iter()
        .map(|reference| {
            distinct
                .get(reference)
                .cloned()
                .flatten()
                .expect("every reference was resolved")
        })
        .collect())
}

fn failure(error: StorageError) -> BodyFailure {
    match error {
        StorageError::BodyUnresolved { reason, .. } => BodyFailure {
            class: if matches!(reason, BodyUnresolvedReason::BlobMissing) {
                BodyFailureClass::Missing
            } else {
                BodyFailureClass::Corrupt
            },
            reason: reason.to_string(),
        },
        StorageError::Sqlite(error) => BodyFailure {
            class: match error.sqlite_error_code() {
                Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => BodyFailureClass::Busy,
                _ => BodyFailureClass::Io,
            },
            reason: error.to_string(),
        },
        other => BodyFailure {
            class: BodyFailureClass::Io,
            reason: other.to_string(),
        },
    }
}
