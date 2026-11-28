//! Read-only protocol services. Cursors are server-issued capabilities tied to
//! a query and collection revision; none contain storage paths or SQL offsets.
pub(crate) mod approval;
mod history;
pub(crate) mod input_draft;
mod models;

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{collections::VecDeque, sync::Mutex};
use workbench_protocol::{
    errors::{ErrorCode, RetryAdvice, WorkbenchError},
    ids::PageCursor,
    queries::PageRequest,
    views::Page,
    wire::Revision,
};

pub(crate) use history::{Bodies, CommitScanner};
pub(crate) use models::*;

#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryRoute {
    Indexed,
    Legacy(&'static str),
}

/// Leave room for the enclosing reply and JSON escaping already counted below.
pub const PAGE_BYTES: usize = 768 * 1024;
pub(crate) fn error(code: ErrorCode, text: &str) -> WorkbenchError {
    WorkbenchError::new(
        code,
        text,
        if matches!(
            code,
            ErrorCode::Conflict
                | ErrorCode::ResyncRequired
                | ErrorCode::StaleEpoch
                | ErrorCode::StaleRevision
        ) {
            RetryAdvice::AfterResync
        } else {
            RetryAdvice::Never
        },
    )
}
pub(crate) fn storage_error(_: storage::StorageError) -> WorkbenchError {
    // Storage diagnostics can include secret-bearing paths.
    error(
        ErrorCode::StorageUnavailable,
        "projection storage is unavailable",
    )
}
pub(crate) fn revision<T: Serialize>(value: &T) -> Revision {
    let hash = Sha256::digest(serde_json::to_vec(value).expect("serializable view"));
    Revision(u64::from_be_bytes(hash[..8].try_into().unwrap()))
}
pub(crate) fn masked(value: &str) -> String {
    static REDACTOR: std::sync::OnceLock<secret_guard::SecretRedactor> = std::sync::OnceLock::new();
    REDACTOR
        .get_or_init(secret_guard::SecretRedactor::from_env)
        .redact(value)
        .text
}
pub(crate) fn text(value: &str, max: usize) -> String {
    let masked = masked(value);
    // Agent-authored question text permits control characters. Count their
    // JSON escapes too so one valid view cannot consume the whole frame.
    let mut encoded = 2usize;
    let mut end = 0;
    for (offset, ch) in masked.char_indices() {
        let cost = match ch {
            '"' | '\\' | '\n' | '\r' | '\t' | '\x08' | '\x0c' => 2,
            c if c < ' ' => 6,
            c => c.len_utf8(),
        };
        if offset + ch.len_utf8() > max || encoded.saturating_add(cost) > max.saturating_mul(2) {
            break;
        }
        encoded += cost;
        end = offset + ch.len_utf8();
    }
    masked[..end].to_owned()
}

#[derive(Default)]
pub(crate) struct Pager(Mutex<VecDeque<(String, String, Revision, CursorPosition)>>);
enum CursorPosition {
    Offset(usize),
    History {
        boundary: storage::history::read::Order,
        freeze: [u8; 32],
    },
}
impl CursorPosition {
    fn offset(&self) -> Result<usize, WorkbenchError> {
        match self {
            Self::Offset(offset) => Ok(*offset),
            Self::History { .. } => Err(error(ErrorCode::Conflict, "wrong page cursor type")),
        }
    }
}
pub(crate) enum HistoryCursor {
    Legacy(Revision),
    Indexed {
        high_water: i64,
        boundary: storage::history::read::Order,
        freeze: [u8; 32],
    },
}
impl Pager {
    pub fn history_cursor(
        &self,
        key: &str,
        request: &PageRequest,
    ) -> Result<Option<HistoryCursor>, WorkbenchError> {
        let Some(cursor) = &request.cursor else {
            return Ok(None);
        };
        let tokens = self.0.lock().unwrap();
        let (_, query, revision, position) = tokens
            .iter()
            .find(|e| e.0 == cursor.as_str())
            .ok_or_else(|| error(ErrorCode::Conflict, "unknown page cursor or wrong query"))?;
        let same_scope = match position {
            CursorPosition::History { .. } => query == key,
            CursorPosition::Offset(_) => query
                .strip_prefix(key)
                .is_some_and(|tail| tail.starts_with(':')),
        };
        if !same_scope {
            return Err(error(
                ErrorCode::Conflict,
                "unknown page cursor or wrong query",
            ));
        }
        Ok(Some(match position {
            CursorPosition::Offset(_) => HistoryCursor::Legacy(*revision),
            CursorPosition::History { boundary, freeze } => HistoryCursor::Indexed {
                high_water: revision.get() as i64,
                boundary: boundary.clone(),
                freeze: *freeze,
            },
        }))
    }
    pub fn history_next(
        &self,
        key: &str,
        high_water: i64,
        boundary: storage::history::read::Order,
        freeze: [u8; 32],
    ) -> PageCursor {
        let token = uuid::Uuid::new_v4().to_string();
        let mut tokens = self.0.lock().unwrap();
        tokens.push_back((
            token.clone(),
            key.to_owned(),
            Revision(high_water as u64),
            CursorPosition::History { boundary, freeze },
        ));
        while tokens.len() > 4096 {
            tokens.pop_front();
        }
        PageCursor::new(token).unwrap()
    }
    pub fn cursor_revision(
        &self,
        key: &str,
        request: &PageRequest,
    ) -> Result<Option<Revision>, WorkbenchError> {
        let Some(cursor) = &request.cursor else {
            return Ok(None);
        };
        let tokens = self.0.lock().unwrap();
        tokens
            .iter()
            .find(|(token, query, _, _)| {
                token == cursor.as_str()
                    && (query == key
                        || query
                            .strip_prefix(key)
                            .is_some_and(|tail| tail.starts_with('#')))
            })
            .map(|(_, _, revision, _)| Some(*revision))
            .ok_or_else(|| error(ErrorCode::Conflict, "unknown page cursor or wrong query"))
    }
    /// Backward cursors hold an exclusive end offset; items within each page
    /// stay chronological so a client can prepend the older page unchanged.
    pub fn page_backward<T: Clone + Serialize>(
        &self,
        key: &str,
        items: &[T],
        request: &PageRequest,
        rev: Revision,
    ) -> Result<Page<T>, WorkbenchError> {
        request.validate()?;
        let mut tokens = self.0.lock().unwrap();
        let end = match &request.cursor {
            None => items.len(),
            Some(cursor) => {
                let (_, query, revision, offset) = tokens
                    .iter()
                    .find(|e| e.0 == cursor.as_str())
                    .ok_or_else(|| {
                    error(ErrorCode::Conflict, "unknown or expired page cursor")
                })?;
                let offset = offset.offset()?;
                if query != key || *revision != rev || offset > items.len() {
                    return Err(error(
                        ErrorCode::Conflict,
                        "page cursor belongs to another query or snapshot",
                    ));
                }
                offset
            }
        };
        let mut start = end;
        let mut bytes = 256;
        while start > 0 && end - start < request.limit as usize {
            let size = serde_json::to_vec(&items[start - 1])
                .expect("serializable view")
                .len()
                + 1;
            if bytes + size > PAGE_BYTES {
                if start == end {
                    return Err(error(
                        ErrorCode::InvalidRequest,
                        "one view exceeds page byte limit",
                    ));
                }
                break;
            }
            bytes += size;
            start -= 1;
        }
        let next_cursor = if start > 0 {
            let token = uuid::Uuid::new_v4().to_string();
            tokens.push_back((
                token.clone(),
                key.to_owned(),
                rev,
                CursorPosition::Offset(start),
            ));
            while tokens.len() > 4096 {
                tokens.pop_front();
            }
            Some(PageCursor::new(token).unwrap())
        } else {
            None
        };
        Ok(Page {
            items: items[start..end].to_vec(),
            next_cursor,
            snapshot_revision: rev,
        })
    }
    pub fn page<T: Clone + Serialize>(
        &self,
        key: &str,
        items: &[T],
        request: &PageRequest,
        rev: Revision,
    ) -> Result<Page<T>, WorkbenchError> {
        request.validate()?;
        let mut tokens = self.0.lock().unwrap();
        let start = match &request.cursor {
            None => 0,
            Some(cursor) => {
                let (_, query, revision, offset) = tokens
                    .iter()
                    .find(|entry| entry.0 == cursor.as_str())
                    .ok_or_else(|| error(ErrorCode::Conflict, "unknown or expired page cursor"))?;
                if query != key || *revision != rev {
                    return Err(error(
                        ErrorCode::Conflict,
                        "page cursor belongs to another query or snapshot",
                    ));
                }
                offset.offset()?
            }
        };
        // History has a fixed event high-water, but retention/tombstones can
        // still remove rows below it. Never slice using a revoked offset.
        if start > items.len() {
            return Err(error(ErrorCode::Conflict, "page source was removed"));
        }
        let mut end = start;
        let mut bytes = 256;
        while end < items.len() && end - start < request.limit as usize {
            let size = serde_json::to_vec(&items[end])
                .expect("serializable view")
                .len()
                + 1;
            if bytes + size > PAGE_BYTES {
                if end == start {
                    return Err(error(
                        ErrorCode::InvalidRequest,
                        "one view exceeds page byte limit",
                    ));
                }
                break;
            }
            bytes += size;
            end += 1;
        }
        let next_cursor = if end < items.len() {
            let token = uuid::Uuid::new_v4().to_string();
            tokens.push_back((
                token.clone(),
                key.to_owned(),
                rev,
                CursorPosition::Offset(end),
            ));
            while tokens.len() > 4096 {
                tokens.pop_front();
            }
            Some(PageCursor::new(token).unwrap())
        } else {
            None
        };
        Ok(Page {
            items: items[start..end].to_vec(),
            next_cursor,
            snapshot_revision: rev,
        })
    }
}

#[cfg(test)]
mod cursor_tests {
    use super::*;
    use storage::history::read::Order;

    #[test]
    fn history_and_offset_cursors_share_one_retention_cap_but_not_their_state_types() {
        let pager = Pager::default();
        let first =
            pager.history_next("history:test", 7, Order(2, true, 0, String::new()), [1; 32]);
        for id in 0..4094 {
            pager.history_next(
                "history:test",
                7,
                Order(id, true, 0, String::new()),
                [1; 32],
            );
        }
        let request = PageRequest {
            cursor: None,
            limit: 1,
        };
        let page = pager
            .page("other", &[1, 2, 3], &request, Revision(9))
            .unwrap();
        assert_eq!(pager.0.lock().unwrap().len(), 4096);
        let newest =
            pager.history_next("history:test", 7, Order(6, true, 0, String::new()), [2; 32]);
        assert_eq!(pager.0.lock().unwrap().len(), 4096);
        assert_eq!(
            pager
                .history_cursor(
                    "history:test",
                    &PageRequest {
                        cursor: Some(first),
                        limit: 1
                    }
                )
                .err()
                .unwrap()
                .code,
            ErrorCode::Conflict
        );
        assert!(
            matches!(pager.history_cursor("history:test", &PageRequest { cursor: Some(newest.clone()), limit: 1 }).unwrap(),
            Some(HistoryCursor::Indexed { high_water: 7, freeze, .. }) if freeze == [2; 32])
        );
        assert_eq!(
            pager
                .page(
                    "history:test",
                    &[1, 2, 3],
                    &PageRequest {
                        cursor: Some(newest),
                        limit: 1
                    },
                    Revision(7)
                )
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        let next = pager
            .page(
                "other",
                &[1, 2, 3],
                &PageRequest {
                    cursor: page.next_cursor,
                    limit: 1,
                },
                Revision(9),
            )
            .unwrap();
        assert_eq!(next.items, vec![2]);
        let other = pager.history_next(
            "history:test:other",
            7,
            Order(0, true, 0, String::new()),
            [0; 32],
        );
        assert_eq!(
            pager
                .history_cursor(
                    "history:test",
                    &PageRequest {
                        cursor: Some(other),
                        limit: 0
                    }
                )
                .err()
                .unwrap()
                .code,
            ErrorCode::Conflict
        );
    }
}
