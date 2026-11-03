//! Structural checks of the run conversation rows that the reference check cannot see:
//! the messages of a run are `0..message_count` with no gap, their lengths add up to the
//! header's `messages_len`, the header's projections agree with the configuration, and
//! (with `verify`) every message and header document resolves to the hash and length
//! its row records. Read-only; findings go into the report.

use rusqlite::{Connection, params};

use super::check::{BodyScope, CheckReport};
use crate::StorageError;
use crate::repo::run_messages::{BODY_FORMAT, load_messages, resolved_document};
use sha2::{Digest, Sha256};

struct Header {
    run_id: String,
    body_format: i64,
    message_count: i64,
    messages_len: i64,
    checkpoints_len: i64,
    config_len: i64,
    config_json: String,
    checkpoints_json: String,
    reason: Option<String>,
}

fn headers(conn: &Connection, scope: BodyScope<'_>) -> Result<Vec<Header>, StorageError> {
    let (filter, value) = match scope {
        BodyScope::All => ("1 = 1", None),
        BodyScope::Run(run) => ("run_id = ?1", Some(run)),
        BodyScope::Thread(thread) => (
            "run_id IN (SELECT run_id FROM thread_runs WHERE thread_id = ?1)",
            Some(thread),
        ),
    };
    let mut statement = conn.prepare(&format!(
        "SELECT run_id, body_format, message_count, messages_len, checkpoints_len, config_len, \
         config_json, checkpoints_json, non_restorable_reason FROM run_contexts \
         WHERE {filter} ORDER BY run_id"
    ))?;
    let map = |row: &rusqlite::Row<'_>| {
        Ok(Header {
            run_id: row.get(0)?,
            body_format: row.get(1)?,
            message_count: row.get(2)?,
            messages_len: row.get(3)?,
            checkpoints_len: row.get(4)?,
            config_len: row.get(5)?,
            config_json: row.get(6)?,
            checkpoints_json: row.get(7)?,
            reason: row.get(8)?,
        })
    };
    let rows = match value {
        Some(value) => statement.query_map([value], map)?,
        None => statement.query_map([], map)?,
    };
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Adds the findings of the run rows in `scope` to `report`.
pub(crate) fn check_runs(
    conn: &Connection,
    scope: BodyScope<'_>,
    verify: bool,
    report: &mut CheckReport,
) -> Result<(), StorageError> {
    for header in headers(conn, scope)? {
        let label = format!("run_context run_id={}", header.run_id);
        if header.body_format != BODY_FORMAT {
            report.add(
                "body_format",
                Some(&label),
                None,
                format!("body_format is {}, not {BODY_FORMAT}", header.body_format),
            );
            continue;
        }
        let (count, min, max, plain): (i64, i64, i64, i64) = conn.query_row(
            "SELECT COUNT(*), COALESCE(MIN(ordinal), 0), COALESCE(MAX(ordinal), -1), \
             COALESCE(SUM(plain_len), 0) FROM run_messages WHERE run_id = ?1",
            [&header.run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        if count != header.message_count {
            report.add(
                "message_count",
                Some(&label),
                None,
                format!(
                    "{count} message rows, the header says {}",
                    header.message_count
                ),
            );
        } else if count > 0 && (min != 0 || max != count - 1) {
            report.add(
                "message_ordinals",
                Some(&label),
                None,
                format!("ordinals run from {min} to {max} for {count} rows"),
            );
        } else {
            // `[`, `]` and one comma between neighbours.
            let joined = plain + 2 + (count - 1).max(0);
            if joined != header.messages_len {
                report.add(
                    "messages_len",
                    Some(&label),
                    None,
                    format!(
                        "messages add up to {joined} bytes, the header says {}",
                        header.messages_len
                    ),
                );
            }
        }
        let config = serde_json::from_str::<serde_json::Value>(&header.config_json);
        match &config {
            Ok(config) => {
                let reason = config
                    .get("non_restorable_reason")
                    .and_then(serde_json::Value::as_str);
                // A reason long enough to be a body is compared when it is resolved.
                if reason.is_none_or(|text| !crate::body_ref::is_token_position(text))
                    && reason != header.reason.as_deref()
                {
                    report.add(
                        "non_restorable_reason",
                        Some(&label),
                        None,
                        "the projection differs from the configuration",
                    );
                }
            }
            Err(error) => report.add(
                "config_json",
                Some(&label),
                None,
                format!("configuration is not JSON: {error}"),
            ),
        }
        if verify {
            verify_run(conn, &header, &label, report)?;
        }
    }
    if matches!(scope, BodyScope::All) {
        let orphans: i64 = conn.query_row(
            "SELECT COUNT(*) FROM run_messages m WHERE NOT EXISTS \
             (SELECT 1 FROM run_contexts c WHERE c.run_id = m.run_id)",
            [],
            |row| row.get(0),
        )?;
        if orphans > 0 {
            report.add(
                "orphan_messages",
                None,
                None,
                format!("{orphans} message rows have no run header"),
            );
        }
    }
    Ok(())
}

fn verify_run(
    conn: &Connection,
    header: &Header,
    label: &str,
    report: &mut CheckReport,
) -> Result<(), StorageError> {
    let count = usize::try_from(header.message_count).unwrap_or(0);
    match load_messages(conn, &header.run_id, count, 0..count) {
        Ok(messages) => {
            let mut statement = conn.prepare_cached(
                "SELECT ordinal, msg_sha256 FROM run_messages WHERE run_id = ?1 ORDER BY ordinal",
            )?;
            let rows = statement.query_map(params![header.run_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?;
            for row in rows {
                let (ordinal, sha) = row?;
                let matches = usize::try_from(ordinal)
                    .ok()
                    .and_then(|index| messages.get(index))
                    .is_some_and(|text| Sha256::digest(text.as_bytes()).as_slice() == sha);
                if !matches {
                    report.add(
                        "message_hash",
                        Some(label),
                        None,
                        format!("message {ordinal} does not hash to its row"),
                    );
                }
            }
        }
        Err(error) => report.add("run_unreadable", Some(label), None, error.to_string()),
    }
    for (what, text, expected) in [
        ("configuration", &header.config_json, header.config_len),
        (
            "checkpoint list",
            &header.checkpoints_json,
            header.checkpoints_len,
        ),
    ] {
        match resolved_document(conn, &header.run_id, text) {
            Ok(value) => {
                if what == "configuration" {
                    // The projection is compared with the resolved reason, so a reason
                    // that is a blob is checked too (the plain check skips it).
                    let reason = value
                        .get("non_restorable_reason")
                        .and_then(serde_json::Value::as_str);
                    if reason != header.reason.as_deref() {
                        report.add(
                            "non_restorable_reason",
                            Some(label),
                            None,
                            "the projection differs from the resolved configuration",
                        );
                    }
                }
                let len = serde_json::to_string(&value).map_or(0, |text| text.len());
                if i64::try_from(len).ok() != Some(expected) {
                    report.add(
                        "document_len",
                        Some(label),
                        None,
                        format!("{what} resolves to {len} bytes, the header says {expected}"),
                    );
                }
            }
            Err(error) => report.add("run_unreadable", Some(label), None, error.to_string()),
        }
    }
    Ok(())
}
