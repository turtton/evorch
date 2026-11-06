//! Read-only protocol services. Cursors are server-issued capabilities tied to
//! a query and collection revision; none contain storage paths or SQL offsets.
pub(crate) mod approval;
mod history;
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
pub(crate) struct Pager(Mutex<VecDeque<(String, String, Revision, usize)>>);
impl Pager {
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
                if query != key || *revision != rev || *offset > items.len() {
                    return Err(error(
                        ErrorCode::Conflict,
                        "page cursor belongs to another query or snapshot",
                    ));
                }
                *offset
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
            tokens.push_back((token.clone(), key.to_owned(), rev, start));
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
                *offset
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
            tokens.push_back((token.clone(), key.to_owned(), rev, end));
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
