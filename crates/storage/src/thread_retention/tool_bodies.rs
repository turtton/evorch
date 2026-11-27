//! Display-body retention. Canonical membership only; no deletion-family closure.
use super::{BindingInput, NANOS_PER_DAY};
use crate::{Database, StorageError};
use rusqlite::{Connection, OptionalExtension, params};

pub const MAX_RUNS: usize = 1_024;
pub const REFERENCE_WINDOW: usize = 2_000;
pub const EVENT_ROWS: usize = 256;
pub const SKELETON_BYTES: usize = 2 * 1024 * 1024;
pub const REMOVED: &str = "[removed by retention]";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub event_id: i64,
    pub blob_id: i64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub event_id: i64,
    pub run_id: String,
    pub thread_id: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalThread {
    pub thread_id: String,
    pub runs: Vec<String>,
    pub root_run_id: Option<String>,
    pub created_ns: i64,
    pub pinned: bool,
    pub pin_revision: Option<u64>,
}
#[derive(Clone, Debug)]
pub struct Request {
    pub db_identity: String,
    pub binding: BindingInput,
    pub expected: CanonicalThread,
    pub run_id: String,
    pub after_event: i64,
    pub now_ns: i64,
    pub after_days: u32,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    pub changed: usize,
    pub inspected: usize,
    pub stored_bytes: usize,
    pub decoded_bytes: usize,
    pub old_refs: usize,
    pub new_refs: usize,
    pub after_event: i64,
    pub done: bool,
    pub hold: Option<&'static str>,
}

pub fn cutoff(now_ns: i64, after_days: u32) -> Option<i64> {
    if after_days == 0 || now_ns < 0 {
        return None;
    }
    now_ns
        .checked_sub(i64::from(after_days).checked_mul(NANOS_PER_DAY)?)
        .filter(|n| *n >= 0)
}

pub(crate) fn canonical(
    conn: &Connection,
    id: &str,
) -> Result<Option<CanonicalThread>, StorageError> {
    let row = conn.query_row("SELECT root_run_id, created_at_ns, pinned, pin_revision FROM threads WHERE thread_id=?1 AND NOT EXISTS(SELECT 1 FROM thread_tombstones WHERE thread_id=?1)", [id], |r| Ok((r.get::<_,Option<String>>(0)?,r.get::<_,i64>(1)?,r.get::<_,bool>(2)?,r.get::<_,Option<u64>>(3)?))).optional()?;
    let Some((root_run_id, created_ns, pinned, pin_revision)) = row else {
        return Ok(None);
    };
    let mut runs: Vec<String> = conn.prepare_cached("SELECT run_id FROM thread_runs INDEXED BY idx_thread_runs_thread WHERE thread_id=?1 LIMIT ?2")?.query_map(params![id, MAX_RUNS+1], |r|r.get(0))?.collect::<Result<_,_>>()?;
    runs.sort();
    Ok(Some(CanonicalThread {
        thread_id: id.into(),
        runs,
        root_run_id,
        created_ns,
        pinned,
        pin_revision,
    }))
}

impl Database {
    /// Raw reference rows are bounded before header/type/membership probes. Cursor is
    /// volatile and advances through non-target and malformed rows too.
    pub fn tool_body_candidate(
        &self,
        cursor: &mut Cursor,
        now_ns: i64,
    ) -> Result<Option<Candidate>, StorageError> {
        if cursor.event_id == 0 && cursor.blob_id == 0 {
            let max: i64 = self.conn.query_row(
                "SELECT coalesce(max(event_id),0) FROM content_blob_refs_event",
                [],
                |r| r.get(0),
            )?;
            cursor.event_id = (now_ns.unsigned_abs() % (max.max(1) as u64)) as i64;
            cursor.blob_id = -1;
        }
        let refs: Vec<(i64,i64)> = self.conn.prepare_cached("SELECT event_id, blob_id FROM content_blob_refs_event WHERE (event_id,blob_id) > (?1,?2) ORDER BY event_id,blob_id LIMIT ?3")?.query_map(params![cursor.event_id,cursor.blob_id,REFERENCE_WINDOW],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<Result<_,_>>()?;
        if refs.is_empty() {
            cursor.event_id = -1;
            cursor.blob_id = -1;
            return Ok(None);
        }
        for (event_id, blob_id) in refs.into_iter().take(EVENT_ROWS) {
            *cursor = Cursor { event_id, blob_id };
            let run: Option<String> = self.conn.query_row("SELECT run_id FROM events WHERE id=?1 AND body_format=1 AND kind='Tool' AND subkind IN ('ToolStarted','ToolCompleted') AND run_id IS NOT NULL", [event_id], |r|r.get(0)).optional()?;
            let Some(run_id) = run else {
                continue;
            };
            let long: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM content_blobs WHERE id=?1 AND byte_len>=2048)",
                [blob_id],
                |r| r.get(0),
            )?;
            if !long {
                continue;
            }
            let thread_id: Option<String> = self
                .conn
                .query_row(
                    "SELECT thread_id FROM thread_runs WHERE run_id=?1",
                    [&run_id],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(thread_id) = thread_id {
                return Ok(Some(Candidate {
                    event_id,
                    run_id,
                    thread_id,
                }));
            }
        }
        Ok(None)
    }
    pub fn tool_body_canonical(&self, id: &str) -> Result<Option<CanonicalThread>, StorageError> {
        let tx = self.conn.unchecked_transaction()?;
        canonical(&tx, id)
    }
}

/// Re-read all durable eligibility evidence inside the writer's IMMEDIATE tx.
/// No callback into runtime memory and no admission/fence is created here.
pub(crate) fn check(
    conn: &Connection,
    request: &Request,
) -> Result<Option<&'static str>, StorageError> {
    let identity: Option<String> = conn
        .query_row("SELECT db_id FROM retention_identity WHERE id=1", [], |r| {
            r.get(0)
        })
        .optional()?;
    if identity.as_deref() != Some(request.db_identity.as_str()) {
        return Ok(Some("DatabaseIdentityMismatch"));
    }
    let binding = crate::repo::thread_retention::binding(conn)?;
    if !binding.is_some_and(|b| {
        b.ownership_root == request.binding.ownership_root
            && b.sidebar_path == request.binding.sidebar_path
    }) {
        return Ok(Some("BindingMismatch"));
    }
    let Some(cutoff) = cutoff(request.now_ns, request.after_days) else {
        return Ok(Some("InvalidDeadline"));
    };
    let Some(actual) = canonical(conn, &request.expected.thread_id)? else {
        return Ok(Some("ThreadMissing"));
    };
    if actual != request.expected {
        return Ok(Some("CanonicalThreadChanged"));
    }
    if actual.runs.len() > MAX_RUNS {
        return Ok(Some("CanonicalRunBudget"));
    }
    if actual.created_ns < 0
        || actual.runs.is_empty()
        || !actual.runs.contains(&request.run_id)
        || actual
            .root_run_id
            .as_ref()
            .is_none_or(|r| !actual.runs.contains(r))
    {
        return Ok(Some("CanonicalUnresolved"));
    }
    if actual.pinned {
        return Ok(Some("Pinned"));
    }
    let runs = serde_json::to_string(&actual.runs)
        .map_err(|_| StorageError::Serialization("tool retention membership".into()))?;
    let deleted: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM json_each(?1) j JOIN run_tombstones t ON t.run_id=j.value)",
        [runs],
        |r| r.get(0),
    )?;
    if deleted {
        return Ok(Some("RunTombstoned"));
    }
    let member = super::decision::FamilyMember {
        thread_id: actual.thread_id,
        created_ns: actual.created_ns,
        runs: actual.runs,
    };
    let decision = super::decision::decide_tool_bodies_on_connection(
        conn,
        &member,
        cutoff,
        &Default::default(),
    )?;
    Ok(match decision.verdict {
        super::decision::Verdict::Due => None,
        super::decision::Verdict::Hold(h) => Some(h.reason()),
    })
}
