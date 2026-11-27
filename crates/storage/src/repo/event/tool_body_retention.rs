//! Bounded skeleton-only rewrite. Blob data and provider history are never read.
use crate::thread_retention::tool_bodies::*;
use crate::{
    StorageError,
    body_ref::{BodyToken, collect_tokens},
    body_store,
};
use rusqlite::{
    Connection, OptionalExtension, params,
    types::{Value as SqlValue, ValueRef},
};
use serde_json::Value;
use std::{collections::BTreeSet, io::Read};

fn invalid() -> StorageError {
    StorageError::Serialization("invalid tool retention skeleton".into())
}

fn skeleton(value: ValueRef<'_>, budget: usize) -> Result<String, StorageError> {
    match value {
        ValueRef::Text(b) if b.len() <= budget => {
            String::from_utf8(b.to_vec()).map_err(|_| invalid())
        }
        ValueRef::Blob(b) if b.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) => {
            let decoder = zstd::stream::read::Decoder::new(b).map_err(|_| invalid())?;
            let mut bytes = Vec::new();
            decoder
                .take((budget + 1) as u64)
                .read_to_end(&mut bytes)
                .map_err(|_| invalid())?;
            if bytes.len() > budget {
                return Err(invalid());
            }
            String::from_utf8(bytes).map_err(|_| invalid())
        }
        _ => Err(invalid()),
    }
}
fn replace(value: &mut Value) -> usize {
    let mut count = 0;
    let mut stack = vec![value];
    while let Some(value) = stack.pop() {
        match value {
            Value::String(s) if BodyToken::parse(s).is_ok_and(|t| t.byte_len >= 2048) => {
                *s = REMOVED.into();
                count += 1;
            }
            Value::Array(a) => stack.extend(a.iter_mut()),
            Value::Object(o) => stack.extend(o.values_mut()),
            _ => {}
        }
    }
    count
}
/// Validate shape without reserializing the typed event: unknown metadata, JSON keys
/// and omitted fields are retained byte-for-value, rather than dropped by serde.
fn rewrite(doc: &mut Value, run: &str, subkind: &str) -> Result<bool, StorageError> {
    let kind: event_bus::EventKind = serde_json::from_value(doc.clone()).map_err(|_| invalid())?;
    let valid = match kind {
        event_bus::EventKind::Tool(event_bus::ToolEvent::ToolStarted { run_id, .. })
            if subkind == "ToolStarted" =>
        {
            run_id
        }
        event_bus::EventKind::Tool(event_bus::ToolEvent::ToolCompleted { run_id, .. })
            if subkind == "ToolCompleted" =>
        {
            run_id
        }
        _ => return Err(invalid()),
    };
    if valid.as_deref() != Some(run) {
        return Err(invalid());
    }
    // EventKind and ToolEvent both use the adjacent kind/payload representation.
    let payload = doc
        .get_mut("payload")
        .and_then(|v| v.get_mut("payload"))
        .ok_or_else(invalid)?;
    let mut changed = 0;
    for field in if subkind == "ToolStarted" {
        &["input"][..]
    } else {
        &["output", "detail"][..]
    } {
        if let Some(value) = payload.get_mut(*field) {
            changed += replace(value);
        }
    }
    Ok(changed > 0)
}
fn token_ids(conn: &Connection, doc: &Value) -> Result<BTreeSet<i64>, StorageError> {
    let tokens = collect_tokens(doc, "event")?;
    if tokens.len() > REFERENCE_WINDOW {
        return Err(invalid());
    }
    tokens
        .into_iter()
        .map(|t| {
            conn.query_row(
                "SELECT id FROM content_blobs WHERE hash=?1 AND byte_len=?2",
                params![&t.hash[..], t.byte_len],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(invalid)
        })
        .collect()
}

pub(crate) fn apply(conn: &Connection, request: &Request) -> Result<Progress, StorageError> {
    body_store::require_write_transaction(conn)?;
    let mut out = Progress {
        after_event: request.after_event,
        ..Default::default()
    };
    if let Some(h) = check(conn, request)? {
        out.hold = Some(h);
        return Ok(out);
    }
    // LIMIT precedes all Tool/body filtering. No OFFSET and no decompression by SQL.
    let headers: Vec<(i64,String,Option<String>,i64,i64)> = conn.prepare_cached("SELECT id,kind,subkind,body_format,octet_length(payload) FROM events INDEXED BY idx_events_run_id WHERE run_id=?1 AND id>?2 ORDER BY id LIMIT ?3")?.query_map(params![request.run_id,request.after_event,EVENT_ROWS],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?.collect::<Result<_,_>>()?;
    let count = headers.len();
    for (id, kind, sub, format, stored) in headers {
        if kind != "Tool"
            || format != 1
            || !matches!(sub.as_deref(), Some("ToolStarted" | "ToolCompleted"))
        {
            out.inspected += 1;
            out.after_event = id;
            continue;
        }
        let stored = usize::try_from(stored).unwrap_or(usize::MAX);
        if stored > SKELETON_BYTES {
            tracing::warn!(
                event_id = id,
                reason = "StoredSkeletonBudget",
                "tool body retention skipped"
            );
            out.inspected += 1;
            out.after_event = id;
            continue;
        }
        if out.stored_bytes.saturating_add(stored) > SKELETON_BYTES {
            break;
        }
        let refs: Vec<i64> = conn
            .prepare_cached(
                "SELECT blob_id FROM content_blob_refs_event WHERE event_id=?1 LIMIT ?2",
            )?
            .query_map(params![id, REFERENCE_WINDOW - out.old_refs + 1], |r| {
                r.get(0)
            })?
            .collect::<Result<_, _>>()?;
        if refs.len() > REFERENCE_WINDOW - out.old_refs {
            if out.old_refs != 0 {
                break;
            }
            tracing::warn!(
                event_id = id,
                reason = "ReferenceBudget",
                "tool body retention skipped"
            );
            out.inspected += 1;
            out.after_event = id;
            continue;
        }
        out.stored_bytes += stored;
        out.old_refs += refs.len();
        let decoded_before = out.decoded_bytes;
        let result = (|| -> Result<(Value, bool), StorageError> {
            let text = conn.query_row("SELECT payload FROM events WHERE id=?1", [id], |r| {
                Ok(skeleton(r.get_ref(0)?, SKELETON_BYTES - out.decoded_bytes))
            })??;
            out.decoded_bytes += text.len();
            let mut doc: Value = serde_json::from_str(&text).map_err(|_| invalid())?;
            if token_ids(conn, &doc)? != refs.iter().copied().collect() {
                return Err(invalid());
            }
            let changed = rewrite(
                &mut doc,
                &request.run_id,
                sub.as_deref().unwrap_or_default(),
            )?;
            Ok((doc, changed))
        })();
        match result {
            Ok((doc, true)) => {
                let new = collect_tokens(&doc, "event")?.len();
                if out.new_refs + new > REFERENCE_WINDOW {
                    break;
                }
                let text = serde_json::to_string(&doc).map_err(|_| invalid())?;
                let encoded =
                    super::encode_payload("Tool", sub.as_deref().unwrap_or_default(), &text);
                body_store::put_owner_with_stored(
                    conn,
                    &body_store::EVENT_OWNER,
                    &[SqlValue::Integer(id)],
                    &mut [],
                    &[&doc],
                    |c, _| {
                        c.execute(
                            "UPDATE events SET payload=?1 WHERE id=?2",
                            params![encoded, id],
                        )?;
                        Ok(())
                    },
                )?;
                out.changed += 1;
                out.new_refs += new;
            }
            Ok((_, false)) => {}
            Err(_) => {
                // Error strings may contain body text; log only the bounded id/reason.
                tracing::warn!(
                    event_id = id,
                    reason = "InvalidOrOversizedSkeleton",
                    "tool body retention skipped"
                );
                // A decode which exceeded the remaining allowance must not consume
                // another row this unit. Retry this row with a full budget next time.
                if decoded_before != 0 {
                    break;
                }
                out.decoded_bytes = SKELETON_BYTES;
            }
        }
        out.inspected += 1;
        out.after_event = id;
    }
    out.done = count < EVENT_ROWS && out.inspected == count;
    Ok(out)
}

#[cfg(test)]
mod tests;
