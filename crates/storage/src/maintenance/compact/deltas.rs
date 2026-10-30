//! Keep the first row's ID and metadata: they are the transcript's display anchor.
//! Chunk only at original row boundaries; never renumber a historical event.

use std::collections::BTreeMap;

use event_bus::{EventKind, MessageEvent};
use rusqlite::{Connection, params};

use super::{CompactOptions, DeltaMode, Stage, checkpoint, statistics};
use crate::{StorageError, entity::SecretGuard, repo::event::StoredEvent};

const MAX_PAYLOAD: usize = 64 * 1024;
const DAY: i64 = 86_400_000_000_000;
// Absolute ID windows stay stable across interruption/replay and bound a group
// of even millions of empty deltas without allocating an ID list.
const GROUP_ID_WINDOW: i64 = 100_000;

struct Pending {
    first: StoredEvent,
    last_id: i64,
    count: u64,
    stored_bytes: u64,
    json_bytes: usize,
}

fn text(kind: &EventKind, reasoning: bool) -> Option<(Option<&str>, &str)> {
    match (kind, reasoning) {
        (EventKind::Message(MessageEvent::MessageDelta { run_id, delta }), false)
        | (EventKind::Message(MessageEvent::ReasoningDelta { run_id, delta }), true) => {
            Some((run_id.as_deref(), delta))
        }
        _ => None,
    }
}

fn compatible(first: &StoredEvent, next: &StoredEvent) -> bool {
    first.session_id == next.session_id
        && first.id.div_euclid(GROUP_ID_WINDOW) == next.id.div_euclid(GROUP_ID_WINDOW)
        && first.event.meta.schema_version == next.event.meta.schema_version
        && crate::system_time_to_ns(first.event.meta.wall_clock)
            .ok()
            .zip(crate::system_time_to_ns(next.event.meta.wall_clock).ok())
            .is_some_and(|(first, next)| first / DAY == next / DAY)
}

fn ignored(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Fault(event_bus::FaultEvent::SubscriberLagged { .. })
            | EventKind::Lifecycle(event_bus::LifecycleEvent::RunProgress { .. })
    )
}

#[cfg(test)]
pub(super) fn compact(
    conn: &Connection,
    options: &CompactOptions,
    reasoning: bool,
    cutoff: Option<i64>,
) -> Result<(Stage, u64), StorageError> {
    compact_preview(conn, options, reasoning, cutoff, None, Some(&[]))
}

pub(super) fn compact_preview(
    conn: &Connection,
    options: &CompactOptions,
    reasoning: bool,
    cutoff: Option<i64>,
    preview: Option<&mut statistics::EventSample>,
    user_times: Option<&[std::time::SystemTime]>,
) -> Result<(Stage, u64), StorageError> {
    compact_with_guard(
        conn,
        options,
        reasoning,
        cutoff,
        &SecretGuard::from_env(),
        preview,
        user_times,
    )
}

fn compact_with_guard(
    conn: &Connection,
    options: &CompactOptions,
    reasoning: bool,
    cutoff: Option<i64>,
    guard: &SecretGuard,
    mut preview: Option<&mut statistics::EventSample>,
    user_times: Option<&[std::time::SystemTime]>,
) -> Result<(Stage, u64), StorageError> {
    let mut pending = BTreeMap::<String, Pending>::new();
    let mut stage = Stage::default();
    let mut oversized = 0;
    let mut after = i64::MIN;
    let mut next_user = 0;
    loop {
        let batch = {
            let mut stmt = conn.prepare(
                "SELECT id,session_id,schema_version,monotonic_ns,wall_clock_ns,kind,payload,
                 OCTET_LENGTH(payload) FROM events WHERE id>?1 ORDER BY id LIMIT ?2",
            )?;
            let mut rows = stmt.query(params![after, options.batch_rows as i64])?;
            let mut batch = Vec::new();
            let mut batch_bytes = 0;
            while let Some(row) = rows.next()? {
                let id: i64 = row.get(0)?;
                let decoded = match crate::repo::event::row_to_event(row) {
                    Ok(event) => Some(event),
                    Err(StorageError::Serialization(_)) => None,
                    Err(error) => return Err(error),
                };
                let bytes = row.get::<_, u64>(7)?;
                batch_bytes += bytes;
                batch.push((id, decoded, bytes));
                if batch_bytes >= 16 * 1024 * 1024 {
                    break;
                }
            }
            batch
        };
        if batch.is_empty() {
            break;
        }
        let mut delete_ids = Vec::new();
        for (id, stored, bytes) in batch {
            after = id;
            let Some(stored) = stored else {
                // An unknown historical variant may be a response boundary.
                flush_all(conn, options, &mut pending, &mut stage, guard, &mut preview)?;
                continue;
            };
            // Match restore_history's global replay cursor, not just the two
            // deltas' clocks: another run can trigger insertion of user input.
            // With no readable sidebar, even a sub-second guess is unsafe.
            let mut user_boundary = user_times.is_none();
            if let Some(times) = user_times {
                while times
                    .get(next_user)
                    .is_some_and(|at| *at <= stored.event.meta.wall_clock)
                {
                    next_user += 1;
                    user_boundary = true;
                }
            }
            if user_boundary {
                flush_all(conn, options, &mut pending, &mut stage, guard, &mut preview)?;
            }
            if ignored(&stored.event.kind) {
                continue;
            }
            // These rows disappear in stage five; treating them as boundaries
            // would create merge opportunities only on a second invocation.
            if cutoff.is_some_and(|cutoff| {
                matches!(
                    &stored.event.kind,
                    EventKind::Provider(
                        event_bus::ProviderEvent::RequestCompleted { .. }
                            | event_bus::ProviderEvent::FirstTokenObserved { .. }
                            | event_bus::ProviderEvent::CacheReuseObserved { .. }
                    )
                ) && crate::system_time_to_ns(stored.event.meta.wall_clock)
                    .is_ok_and(|ns| ns < cutoff)
            }) {
                continue;
            }
            let delta = text(&stored.event.kind, reasoning);
            if let Some((run, delta)) = delta {
                if options.delta_mode == DeltaMode::Delete {
                    let subkind = stored.event.kind.subkind_name();
                    stage.deleted("Message", subkind, bytes);
                    if options.apply {
                        delete_ids.push((id, stored.session_id.clone(), bytes));
                    }
                    continue;
                }
                let json_bytes = serde_json::to_vec(&stored.event.kind)
                    .map_err(|error| StorageError::Serialization(error.to_string()))?
                    .len();
                if json_bytes > MAX_PAYLOAD {
                    oversized += 1;
                }
                let Some(run) = run else {
                    // No safe attribution to order this fragment against a response.
                    flush_all(conn, options, &mut pending, &mut stage, guard, &mut preview)?;
                    stage.input_rows += 1;
                    stage.output_rows += 1;
                    stage.payload_bytes_before += bytes;
                    stage.payload_bytes_after += bytes;
                    observe_original(conn, id, bytes, &mut preview)?;
                    continue;
                };
                let run = run.to_owned();
                let extra = serde_json::to_vec(delta)
                    .map_err(|error| StorageError::Serialization(error.to_string()))?
                    .len()
                    - 2;
                if pending.get(&run).is_some_and(|first| {
                    !compatible(&first.first, &stored)
                        || first.json_bytes.saturating_add(extra) > MAX_PAYLOAD
                }) {
                    flush(
                        conn,
                        options,
                        pending.remove(&run).unwrap(),
                        &mut stage,
                        guard,
                        &mut preview,
                    )?;
                }
                if let Some(first) = pending.get_mut(&run) {
                    match &mut first.first.event.kind {
                        EventKind::Message(
                            MessageEvent::MessageDelta { delta: body, .. }
                            | MessageEvent::ReasoningDelta { delta: body, .. },
                        ) => body.push_str(delta),
                        _ => unreachable!("pending text delta"),
                    }
                    first.last_id = id;
                    first.count += 1;
                    first.stored_bytes += bytes;
                    first.json_bytes += extra;
                } else {
                    if pending.len() >= 64 {
                        flush_all(conn, options, &mut pending, &mut stage, guard, &mut preview)?;
                    }
                    pending.insert(
                        run,
                        Pending {
                            first: stored,
                            last_id: id,
                            count: 1,
                            stored_bytes: bytes,
                            json_bytes,
                        },
                    );
                }
            } else {
                // Run attribution alone is insufficient: a child's terminal
                // lifecycle event is also visible in the root's thread. Without
                // reconstructing every historical thread binding, preserve ALL
                // such display boundaries. Same-kind run deltas may interleave.
                flush_all(conn, options, &mut pending, &mut stage, guard, &mut preview)?;
            }
        }
        if !delete_ids.is_empty() {
            let tx = conn.unchecked_transaction()?;
            for (id, session, bytes) in delete_ids {
                tx.execute("DELETE FROM events WHERE id=?1", [id])?;
                super::adjust_session_bytes(&tx, session.as_deref(), bytes, 0)?;
            }
            tx.commit()?;
        }
        if options.apply {
            checkpoint(conn)?;
        }
    }
    flush_all(conn, options, &mut pending, &mut stage, guard, &mut preview)?;
    Ok((stage, oversized))
}

fn flush_all(
    conn: &Connection,
    options: &CompactOptions,
    pending: &mut BTreeMap<String, Pending>,
    stage: &mut Stage,
    guard: &SecretGuard,
    preview: &mut Option<&mut statistics::EventSample>,
) -> Result<(), StorageError> {
    for (_, first) in std::mem::take(pending) {
        flush(conn, options, first, stage, guard, preview)?;
    }
    Ok(())
}

fn flush(
    conn: &Connection,
    options: &CompactOptions,
    pending: Pending,
    stage: &mut Stage,
    guard: &SecretGuard,
    preview: &mut Option<&mut statistics::EventSample>,
) -> Result<(), StorageError> {
    stage.input_rows += pending.count;
    stage.output_rows += 1;
    stage.payload_bytes_before += pending.stored_bytes;
    if pending.count == 1 {
        stage.payload_bytes_after += pending.stored_bytes;
        observe_original(conn, pending.first.id, pending.stored_bytes, preview)?;
        return Ok(());
    }
    // Checking individual fragments is insufficient: a credential may span them.
    guard.check_event_kind(&pending.first.event.kind)?;
    let payload = serde_json::to_string(&pending.first.event.kind)
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
    stage.payload_bytes_after += payload.len() as u64;
    let subkind = pending.first.event.kind.subkind_name();
    if let Some(sample) = preview.as_deref_mut() {
        sample.record(pending.first.id, payload.len(), || {
            crate::repo::event::encode_payload("Message", subkind, &payload).stored_len()
        });
    }
    let removed = stage
        .removed_by_kind
        .entry(statistics::key("Message", subkind))
        .or_default();
    removed.rows += pending.count - 1;
    // This can be an expansion for already-compressed legacy fragments; preview
    // reports that separately in stage before/after, rather than underflowing.
    removed.payload_bytes += pending.stored_bytes.saturating_sub(payload.len() as u64);
    if options.apply {
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE events SET payload=?2 WHERE id=?1",
            params![pending.first.id, payload],
        )?;
        let deleted = tx.execute(
            "DELETE FROM events WHERE id>?1 AND id<=?2 AND kind='Message' AND subkind=?3 AND run_id=?4",
            params![pending.first.id, pending.last_id, subkind, pending.first.event.primary_run_id()],
        )?;
        if deleted as u64 != pending.count - 1 {
            return Err(super::super::invalid(
                "delta group changed during offline maintenance",
            ));
        }
        super::adjust_session_bytes(
            &tx,
            pending.first.session_id.as_deref(),
            pending.stored_bytes,
            payload.len() as u64,
        )?;
        tx.commit()?;
    }
    Ok(())
}

fn observe_original(
    conn: &Connection,
    id: i64,
    bytes: u64,
    preview: &mut Option<&mut statistics::EventSample>,
) -> Result<(), StorageError> {
    if bytes < crate::codec::MIN_COMPRESSED_BYTES as u64 {
        return Ok(());
    }
    if let Some(sample) = preview.as_deref_mut() {
        let value: rusqlite::types::Value =
            conn.query_row("SELECT payload FROM events WHERE id=?1", [id], |row| {
                row.get(0)
            })?;
        // An unchanged BLOB is already encoded; do not count it as new work.
        if let rusqlite::types::Value::Text(payload) = value {
            sample.record(id, payload.len(), || {
                crate::repo::event::encode_payload("Message", "", &payload).stored_len()
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assembled_secrets_fail_closed_without_rewriting_the_group() {
        let db = crate::Database::open_in_memory().unwrap();
        for fragment in ["secret-", "test-value"] {
            let kind = EventKind::Message(MessageEvent::ReasoningDelta {
                run_id: Some("run".into()),
                delta: fragment.into(),
            });
            db.conn.execute(
                "INSERT INTO events(schema_version,monotonic_ns,wall_clock_ns,kind,subkind,run_id,payload)
                 VALUES(1,0,0,'Message','ReasoningDelta','run',?1)",
                [serde_json::to_string(&kind).unwrap()],
            ).unwrap();
        }
        let before = crate::repo::event::list_all_ordered(&db.conn).unwrap();
        let result = compact_with_guard(
            &db.conn,
            &CompactOptions {
                apply: true,
                ..Default::default()
            },
            true,
            None,
            &SecretGuard::with_known_values(["secret-test-value".into()]),
            None,
            Some(&[]),
        );
        assert!(matches!(result, Err(StorageError::SecretDetected { .. })));
        assert_eq!(
            before,
            crate::repo::event::list_all_ordered(&db.conn).unwrap()
        );
    }
}
