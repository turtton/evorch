//! `recorded_state` of `inspect run`: the newest phase event the log holds for a run.
//!
//! The newest `AgentRunStarted` / `AgentRunStateChanged` row decides, found by
//! `events.id` descending, so a run with more than the event listing's bound still
//! shows its latest state and an older `Waiting` is never reported as current: a later
//! state change or registration replaces it, and an unreadable newest row is an error
//! rather than a fallback to an older one. The value is what the log recorded, not a
//! live probe of the process.

use event_bus::{AgentRunPhase, EventKind, LifecycleEvent};
use serde_json::{Value, json};

use super::{InspectBodies, InspectEventsOptions, event_row};
use crate::{Database, StorageError};

const NEWEST_PHASE_EVENT_SQL: &str = "SELECT id, session_id, wall_clock_ns, kind, payload, \
     run_id, body_format FROM events WHERE run_id = ?1 AND kind = 'Lifecycle' \
     AND subkind IN ('AgentRunStarted', 'AgentRunStateChanged') ORDER BY id DESC LIMIT 1";

impl Database {
    /// The newest recorded phase event of `run_id`: `event`, `phase`, `reason`, `waiting`
    /// (the typed cause a `Waiting` was entered for), `event_id` and `recorded_at_ns` (the
    /// event's wall clock, which is when a wait began). `Value::Null` when the run has no
    /// such event or was deleted.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails or the newest row cannot be decoded.
    pub fn inspect_recorded_state(&self, run_id: &str) -> Result<Value, StorageError> {
        crate::repo::run_messages::read_snapshot(&self.conn, || {
            if crate::thread_retention::TombstoneSet::load(&self.conn)?.contains_run(run_id) {
                return Ok(Value::Null);
            }
            let mut statement = self.conn.prepare(NEWEST_PHASE_EVENT_SQL)?;
            let mut rows = statement.query([run_id])?;
            let Some(row) = rows.next()? else {
                return Ok(Value::Null);
            };
            let options = InspectEventsOptions {
                bodies: InspectBodies::Resolved,
                contains: None,
            };
            let (event, resolved) = event_row(&self.conn, row, options)?;
            let kind: EventKind = serde_json::from_value(resolved).map_err(|error| {
                StorageError::Serialization(format!(
                    "stored phase event {} of {run_id}: {error}",
                    event.id
                ))
            })?;
            recorded_state(event.id, event.wall_clock_ns, kind)
        })
    }
}

fn recorded_state(
    event_id: i64,
    recorded_at_ns: i64,
    kind: EventKind,
) -> Result<Value, StorageError> {
    let EventKind::Lifecycle(event) = kind else {
        return Err(StorageError::Serialization(format!(
            "stored phase event {event_id} is not a lifecycle event"
        )));
    };
    let (name, phase, reason, waiting) = match event {
        // A registration says nothing about the phase; it only replaces older state.
        LifecycleEvent::AgentRunStarted { .. } => ("AgentRunStarted", None, None, None),
        LifecycleEvent::AgentRunStateChanged {
            to,
            reason,
            waiting,
            ..
        } => ("AgentRunStateChanged", Some(to), reason, waiting),
        _ => {
            return Err(StorageError::Serialization(format!(
                "stored phase event {event_id} is neither a registration nor a phase change"
            )));
        }
    };
    let mut value = json!({
        "event": name,
        "phase": phase,
        "reason": reason,
        "waiting": waiting,
        "event_id": event_id,
        "recorded_at_ns": recorded_at_ns,
    });
    if phase == Some(AgentRunPhase::Waiting) && waiting.is_none() {
        // Recorded before waiting causes existed; nothing is inferred for it.
        value["waiting_unrecorded"] = json!(true);
    }
    Ok(value)
}
