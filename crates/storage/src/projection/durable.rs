use std::collections::BTreeMap;

use event_bus::{EventKind, OrchestratorEvent};
use rusqlite::{Connection, params};

use crate::StorageError;
use crate::db::system_time_to_ns;
use crate::entity::TaskContinuation;
use crate::repo::event::StoredEvent;

pub(super) fn reconcile(conn: &Connection, events: &[StoredEvent]) -> Result<(), StorageError> {
    let mut parents = BTreeMap::new();
    let mut runs = BTreeMap::new();
    for stored in events {
        match &stored.event.kind {
            EventKind::Orchestrator(OrchestratorEvent::RunAttached {
                run_id,
                parent_run_id,
                ..
            }) => {
                parents.insert(run_id.clone(), parent_run_id.clone());
            }
            EventKind::Orchestrator(OrchestratorEvent::TaskRetryScheduled {
                task_id,
                new_run_id,
                attempt,
                ..
            }) => {
                if let Some(previous) = runs.insert(task_id.clone(), new_run_id.clone()) {
                    let parent = parents.get(&previous).cloned().flatten();
                    parents.entry(new_run_id.clone()).or_insert(parent);
                }
                conn.execute(
                    "UPDATE tasks SET attempts = ?2, status = 'retrying' WHERE id = ?1",
                    params![task_id, attempt],
                )?;
            }
            EventKind::Orchestrator(OrchestratorEvent::TaskProgressed {
                task_id,
                run_id,
                progress,
                reason,
            }) => {
                if runs.get(task_id).is_some_and(|current| current != run_id) {
                    continue;
                }
                runs.insert(task_id.clone(), run_id.clone());
                if reason == "task heartbeat"
                    && let Some(record) = crate::repo::task::get(conn, task_id)?
                {
                    if let Some(heartbeat) = progress
                        .get("heartbeat_at_ns")
                        .and_then(serde_json::Value::as_u64)
                        && let Ok(heartbeat) = i64::try_from(heartbeat)
                    {
                        let mut saved: serde_json::Value = match record.progress {
                            Some(saved) => serde_json::from_str(&saved)
                                .map_err(|error| StorageError::Serialization(error.to_string()))?,
                            None => progress.clone(),
                        };
                        saved["heartbeat_at_ns"] = heartbeat.into();
                        conn.execute("UPDATE tasks SET heartbeat_at_ns = ?2, progress_json = ?3 WHERE id = ?1", params![task_id, heartbeat, saved.to_string()])?;
                    }
                    continue;
                }
                let Ok(task) = serde_json::from_value::<TaskContinuation>(progress.clone()) else {
                    continue;
                };
                let now = system_time_to_ns(stored.event.meta.wall_clock)?;
                let heartbeat = task
                    .heartbeat_at_ns
                    .and_then(|heartbeat| i64::try_from(heartbeat).ok());
                let parent = parents.get(run_id).cloned().flatten();
                conn.execute(
                    "INSERT INTO tasks (id,session_id,status,created_at_ns,updated_at_ns,parent_run_id,input_json,progress_json,last_artifact_json,failure_reason,resume_cursor_json,attempts,heartbeat_at_ns)
                     VALUES (?1,?2,?3,?4,?4,?5,?6,?7,?8,?9,?10,?11,?12)
                     ON CONFLICT(id) DO UPDATE SET status=excluded.status,updated_at_ns=excluded.updated_at_ns,parent_run_id=excluded.parent_run_id,input_json=excluded.input_json,progress_json=excluded.progress_json,last_artifact_json=excluded.last_artifact_json,failure_reason=excluded.failure_reason,resume_cursor_json=excluded.resume_cursor_json,attempts=excluded.attempts,heartbeat_at_ns=excluded.heartbeat_at_ns",
                    params![task_id, stored.session_id, task.status.as_str(), now, parent, task.input,
                        progress.to_string(), task.last_artifact, task.failure_reason, task.resume_cursor, task.attempts, heartbeat],
                )?;
            }
            EventKind::Orchestrator(
                OrchestratorEvent::TaskCheckpoint { .. }
                | OrchestratorEvent::TaskStaleMarked { .. }
                | OrchestratorEvent::GoalCreated { .. }
                | OrchestratorEvent::GoalStateChanged { .. }
                | OrchestratorEvent::GoalStageChanged { .. }
                | OrchestratorEvent::DeliverableBranchBound { .. }
                | OrchestratorEvent::EvidenceRecorded { .. }
                | OrchestratorEvent::FinishRejected { .. }
                | OrchestratorEvent::FinishAccepted { .. }
                | OrchestratorEvent::ContinuationDispatched { .. }
                | OrchestratorEvent::ContinuationSuppressed { .. }
                | OrchestratorEvent::ReviewRoundStarted { .. }
                | OrchestratorEvent::RepairDispatched { .. }
                | OrchestratorEvent::StallDetected { .. }
                | OrchestratorEvent::NudgeSent { .. }
                | OrchestratorEvent::MergeApprovalRequested { .. }
                | OrchestratorEvent::MergeApprovalResolved { .. }
                | OrchestratorEvent::MergeApprovalInvalidated { .. }
                | OrchestratorEvent::MergeExecuted { .. }
                | OrchestratorEvent::CloseoutStepRecorded { .. }
                | OrchestratorEvent::ShellCommandDenied { .. },
            )
            | EventKind::Lifecycle(_)
            | EventKind::Message(_)
            | EventKind::Tool(_)
            | EventKind::Usage(_)
            | EventKind::Provider(_)
            | EventKind::Fault(_)
            | EventKind::AgentMessage(_)
            | EventKind::Compaction(_)
            | EventKind::Diagnostic(_)
            | EventKind::Ownership(_)
            | EventKind::Snapshot(_)
            | EventKind::Ledger(_) => {}
        }
    }
    Ok(())
}
