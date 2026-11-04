//! Saved child Orchestrator runs predate ADR 0044 §2 and are never revived.
//!
//! Only a record with `role == Orchestrator && parent.is_some()` is refused. A root
//! Orchestrator restores as before. The refusal happens before any snapshot is
//! consumed or any run is registered, so the original snapshot stays on disk as
//! evidence, and one diagnostic names the run, its parent and its thread.

use event_bus::event::diagnostic_codes;

use super::*;
use crate::RunRestoreFailure;
use crate::restore::RunRestoreDescriptor;

impl AgentRuntime {
    pub(super) fn reject_nested_orchestrator(
        &self,
        record: &storage::RunContextRecord,
        descriptor: &RunRestoreDescriptor,
    ) -> Result<(), RuntimeError> {
        let parent = descriptor
            .parent_run_id
            .map(|parent| parent.to_string())
            .or_else(|| record.parent_run_id.clone());
        let is_orchestrator = [record.role.as_str(), descriptor.role.as_str()]
            .into_iter()
            .any(|role| role == Role::Orchestrator.name());
        let Some(parent) = parent.filter(|_| is_orchestrator) else {
            return Ok(());
        };
        let thread = descriptor
            .thread_id
            .clone()
            .or_else(|| {
                self.shared
                    .run_store
                    .get()
                    .and_then(|store| store.thread_of_run(&record.run_id).ok().flatten())
                    .map(|row| row.thread_id)
            })
            .unwrap_or_else(|| "unknown".into());
        let reason = RunRestoreFailure::NestedOrchestrator {
            parent: parent.clone(),
            thread: thread.clone(),
        };
        self.shared.bus.emit(Event::new(event_bus::DiagnosticEvent {
            source: "agent_runtime".into(),
            severity: event_bus::DiagnosticSeverity::Warning,
            code: diagnostic_codes::NESTED_ORCHESTRATOR_REJECTED.into(),
            detail: format!(
                "{reason} (run {}, snapshot terminal phase {}, updated_at_ns {})",
                record.run_id, record.terminal_phase, record.updated_at_ns
            ),
            run_id: Some(record.run_id.clone()),
            thread_id: Some(thread).filter(|thread| thread != "unknown"),
            call_id: None,
        }));
        Err(RuntimeError::RunRestoreFailed {
            run_id: record.run_id.clone(),
            reason,
        })
    }
}
