//! Persistent, events-only projection. All caller mutations share its transaction.
use super::{Reducer, Role, part_progress};
use crate::StorageError;
use event_bus::{EventKind, LifecycleEvent, MessageEvent};
use rusqlite::{Connection, OptionalExtension, params, types::ValueRef};
use std::{
    borrow::Cow,
    io::Read,
    panic::{AssertUnwindSafe, catch_unwind},
};

pub(crate) const FORMAT_VERSION: i64 = 1;
pub(crate) const REDUCER_VERSION: i64 = 1;
pub(crate) const MAX_DESCRIPTOR_BYTES: usize = 8 * 1024 * 1024;

fn invalid() -> StorageError {
    StorageError::Io("unavailable history projection".into())
}
fn json<T: serde::Serialize>(value: &T) -> Result<String, StorageError> {
    serde_json::to_string(value).map_err(|e| StorageError::Serialization(e.to_string()))
}
fn unjson<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, StorageError> {
    serde_json::from_str(value).map_err(|e| StorageError::Serialization(e.to_string()))
}
pub(crate) fn version_ok(conn: &Connection) -> Result<bool, StorageError> {
    Ok(conn.prepare_cached("SELECT format_version = ?1 AND reducer_version = ?2 AND generation IS NOT NULL FROM history_control WHERE id=1")?.query_row( params![FORMAT_VERSION, REDUCER_VERSION], |r| r.get(0))?)
}

#[derive(Debug, Clone)]
pub(crate) struct State {
    pub run: String,
    pub phase: i64,
    pub source: Option<i64>,
    pub applied: Option<i64>,
    pub generation: Option<i64>,
    pub checkpoint: Option<i64>,
    pub last_indexed: Option<i64>,
    pub reducer: Reducer,
    pub merge_max: Option<i64>,
    pub first_prompt: Option<i64>,
    pub first_prompt_merge: Option<i64>,
}
pub(crate) fn state(conn: &Connection, run: &str) -> Result<Option<State>, StorageError> {
    let row = conn.prepare_cached("SELECT phase,source,applied,generation,checkpoint,last_indexed_id,reducer,merge_max,first_prompt_id,first_prompt_merge_max FROM history_run_state WHERE run_id=?1")?.query_row( [run], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get::<_,String>(6)?,r.get(7)?,r.get(8)?,r.get(9)?))).optional()?;
    row.map(
        |(
            phase,
            source,
            applied,
            generation,
            checkpoint,
            last_indexed,
            reducer,
            merge_max,
            first_prompt,
            first_prompt_merge,
        )| {
            Ok(State {
                run: run.into(),
                phase,
                source,
                applied,
                generation,
                checkpoint,
                last_indexed,
                reducer: unjson(&reducer)?,
                merge_max,
                first_prompt,
                first_prompt_merge,
            })
        },
    )
    .transpose()
}

/// Bounded decompression is separate from the scanner's existing event meter.
pub(crate) fn decode_kind(value: ValueRef<'_>) -> Result<EventKind, StorageError> {
    let bytes: Cow<'_, [u8]> = match value {
        ValueRef::Text(s) => Cow::Borrowed(s),
        ValueRef::Blob(s) => {
            let decoder = zstd::stream::read::Decoder::new(s).map_err(|_| invalid())?;
            let mut out = Vec::new();
            decoder
                .take((MAX_DESCRIPTOR_BYTES + 1) as u64)
                .read_to_end(&mut out)
                .map_err(|_| invalid())?;
            Cow::Owned(out)
        }
        _ => return Err(invalid()),
    };
    if bytes.len() > MAX_DESCRIPTOR_BYTES {
        return Err(invalid());
    }
    serde_json::from_slice(&bytes).map_err(|_| invalid())
}
pub(crate) fn source_kind(conn: &Connection, id: i64) -> Result<(EventKind, bool), StorageError> {
    let mut s = conn.prepare_cached("SELECT payload,body_format FROM events WHERE id=?1")?;
    let mut rows = s.query([id])?;
    let row = rows.next()?.ok_or_else(invalid)?;
    let kind = decode_kind(row.get_ref(0)?)?;
    let format: i64 = row.get(1)?;
    if !matches!(format, 0 | 1) {
        return Err(invalid());
    }
    Ok((kind, format == 1))
}
fn text_part(kind: &EventKind, tokenized: bool) -> Result<(String, bool), StorageError> {
    let text = if let EventKind::Message(MessageEvent::MessagePart { text, .. }) = kind {
        text.as_str()
    } else {
        super::committed_text(kind).ok_or_else(invalid)?.2
    };
    Ok((text.into(), tokenized))
}

#[derive(Debug)]
pub(crate) struct Prepared {
    pub prior: State,
    pub next: State,
    pub id: i64,
    pub message: Option<PreparedMessage>,
    pub sources: Vec<i64>,
    pub response: Option<String>,
    pub clear_pending: bool,
    pub pending: Option<(usize, usize)>,
}

#[derive(Debug)]
pub(crate) struct PreparedMessage {
    pub index: u64,
    pub role: Role,
    pub encoded_len: usize,
}

pub(crate) fn prepare(
    conn: &Connection,
    prior: State,
    id: i64,
    at: i64,
    kind: &EventKind,
    tokenized: bool,
) -> Result<Prepared, StorageError> {
    if kind.primary_run_id() != Some(prior.run.as_str()) {
        return Err(invalid());
    }
    let mut next = prior.clone();
    next.merge_max = Some(next.merge_max.map_or(at, |m| m.max(at)));
    let mut sources = vec![id];
    let mut response = None;
    let mut clear_pending = false;
    let mut pending = None;
    let message = if let EventKind::Message(MessageEvent::MessagePart {
        response_id,
        index,
        total,
        ..
    }) = kind
    {
        response = Some(response_id.clone());
        let previous:Option<(usize,usize)>=conn.prepare_cached("SELECT part_index+1,total FROM history_pending_parts WHERE run_id=?1 AND response_id=?2 ORDER BY part_index DESC LIMIT 1")?.query_row(params![prior.run,response_id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let progress = part_progress(previous, *index, *total);
        clear_pending = *index == 0 || progress.is_none();
        if let Some((count, total)) = progress {
            if count == total {
                if *index > 0 {
                    let mut s=conn.prepare_cached("SELECT event_id FROM history_pending_parts WHERE run_id=?1 AND response_id=?2 ORDER BY part_index")?;
                    sources = s
                        .query_map(params![prior.run, response_id], |r| r.get(0))?
                        .collect::<Result<Vec<i64>, _>>()?;
                    if sources.len() != *index {
                        return Err(invalid());
                    }
                    sources.push(id);
                }
                let mut parts = Vec::new();
                let mut bytes = 0usize;
                for source in &sources {
                    let part = if *source == id {
                        text_part(kind, tokenized)?
                    } else {
                        let (kind, format) = source_kind(conn, *source)?;
                        text_part(&kind, format)?
                    };
                    bytes = bytes.saturating_add(part.0.len());
                    if bytes > MAX_DESCRIPTOR_BYTES {
                        return Err(invalid());
                    }
                    parts.push(part);
                }
                clear_pending = true;
                next.reducer.message(Role::Assistant, parts, false, false)
            } else {
                pending = Some((*index, total));
                None
            }
        } else {
            None
        }
    } else {
        if matches!(
            kind,
            EventKind::Lifecycle(LifecycleEvent::TaskPromptPublished { .. })
        ) && next.reducer.first_prompt.is_none()
        {
            next.first_prompt = Some(id);
            next.first_prompt_merge = next.merge_max;
        }
        next.reducer.observe(kind, tokenized, &mut None)
    };
    next.checkpoint = Some(id);
    next.last_indexed = Some(next.last_indexed.map_or(id, |last| last.max(id)));
    let message = message
        .map(|message| {
            Ok::<_, StorageError>(PreparedMessage {
                index: message.index,
                role: message.role,
                encoded_len: encoded_len(&prior.run, &message)?,
            })
        })
        .transpose()?;
    Ok(Prepared {
        prior,
        next,
        id,
        message,
        sources,
        response,
        clear_pending,
        pending,
    })
}

#[derive(serde::Serialize)]
struct EncodedMessage<'a> {
    run: &'a str,
    index: u64,
    role: &'static str,
    parts: &'a [(String, bool)],
    input: Option<()>,
}
fn encoded_len(run: &str, message: &super::Message) -> Result<usize, StorageError> {
    let role = match message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    serde_json::to_writer(
        &mut count,
        &EncodedMessage {
            run,
            index: message.index,
            role,
            parts: &message.parts,
            input: None,
        },
    )
    .map_err(|e| StorageError::Serialization(e.to_string()))?;
    Ok(count.0)
}

/// CAS the source and checkpoint before applying a prepared row from another connection.
pub(crate) fn apply(conn: &Connection, p: &Prepared, ready: bool) -> Result<bool, StorageError> {
    let n = &p.next;
    let old = &p.prior;
    let changed=conn.prepare_cached("UPDATE history_run_state SET checkpoint=?2,last_indexed_id=?3,reducer=?4,output_count=?5,merge_max=?6,first_prompt_id=?7,first_prompt_merge_max=?8,applied=CASE WHEN ?9 THEN source ELSE applied END WHERE run_id=?1 AND checkpoint IS ?10 AND generation IS ?11 AND source IS ?12 AND phase=?13")?.execute(params![n.run,n.checkpoint,n.last_indexed,json(&n.reducer)?,n.reducer.next,n.merge_max,n.first_prompt,n.first_prompt_merge,ready,old.checkpoint,old.generation,old.source,old.phase])?;
    if changed == 0 {
        return Ok(false);
    }
    if p.clear_pending
        && let Some(response) = &p.response
    {
        conn.execute(
            "DELETE FROM history_pending_parts WHERE run_id=?1 AND response_id=?2",
            params![n.run, response],
        )?;
    }
    if let Some((index, total)) = p.pending {
        conn.prepare_cached("INSERT INTO history_pending_parts(run_id,response_id,part_index,total,event_id) VALUES (?1,?2,?3,?4,?5)")?.execute(params![n.run,p.response,index as u64,total as u64,p.id])?;
    }
    conn.prepare_cached("INSERT INTO history_event_cuts(run_id,event_id,before_count,after_count,merge_max) VALUES (?1,?2,?3,?4,?5)")?.execute(params![n.run,p.id,old.reducer.next,n.reducer.next,n.merge_max])?;
    if let Some(message) = &p.message {
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        conn.prepare_cached("INSERT INTO history_messages(run_id,output_event_id,ordinal,role,encoded_len) VALUES (?1,?2,?3,?4,?5)")?.execute(params![n.run,p.id,message.index,role,message.encoded_len as u64])?;
        let mut s=conn.prepare_cached("INSERT INTO history_message_sources(run_id,output_event_id,part_index,event_id) VALUES (?1,?2,?3,?4)")?;
        for (index, id) in p.sources.iter().enumerate() {
            s.execute(params![n.run, p.id, index as u64, id])?;
        }
    }
    Ok(true)
}

pub(crate) enum Representation<'a> {
    Original(&'a EventKind),
    Tokenized(&'a serde_json::Value),
}

#[cfg(test)]
thread_local! {
    pub(super) static PANIC_AFTER_APPLY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Errors and unwinding affect only this projection; the trigger's source stays ahead.
pub(crate) fn append_hook(conn: &Connection, id: i64, at: i64, input: Representation<'_>) {
    if conn.execute_batch("SAVEPOINT history_projection").is_err() {
        return;
    }
    let result = catch_unwind(AssertUnwindSafe(|| -> Result<(), StorageError> {
        if !version_ok(conn)? {
            return Ok(());
        }
        let (kind, tokenized) = match input {
            Representation::Original(kind) => (Cow::Borrowed(kind), false),
            Representation::Tokenized(value) => (
                Cow::Owned(
                    serde_json::from_value::<EventKind>(value.clone()).map_err(|_| invalid())?,
                ),
                true,
            ),
        };
        let Some(run) = kind.primary_run_id() else {
            return Ok(());
        };
        let Some(state) = state(conn, run)? else {
            return Ok(());
        };
        if state.phase != 2
            || state.generation.is_none()
            || state.source.is_none()
            || state.applied.and_then(|v| v.checked_add(1)) != state.source
            || state.last_indexed.is_some_and(|last| id <= last)
        {
            return Ok(());
        }
        let p = prepare(conn, state, id, at, &kind, tokenized)?;
        apply(conn, &p, true)?;
        #[cfg(test)]
        PANIC_AFTER_APPLY.with(|fault| {
            assert!(!fault.replace(false), "injected projection panic");
        });
        Ok(())
    }));
    if !matches!(result, Ok(Ok(()))) {
        let _ = conn.execute_batch("ROLLBACK TO history_projection");
        tracing::warn!("history projection append deferred after failure");
    }
    let _ = conn.execute_batch("RELEASE history_projection");
}
