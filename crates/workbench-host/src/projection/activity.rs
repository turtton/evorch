//! The bus is the only source of live activity. No commit-scanner reconciliation.
use super::*;
use event_bus::{AgentRunPhase, LifecycleEvent as BusLifecycle, ToolEvent};
use std::time::SystemTime;
use storage::history::rows::{duration_ms, wall_clock_ns};
use workbench_protocol::namespaces::execution::{ToolActivityStatus, ToolActivityView};

/// Below 2 MiB even with maximally escaped 16 KiB inline inputs and summaries.
/// Excess parallel starts are not displayed; durable history is unaffected.
pub(super) const MAX_LIVE_TOOLS: usize = 16;
pub(super) struct LiveTool {
    pub tool: ToolActivityView,
    pub started_ns: i64,
}
impl LiveTool {
    pub fn at(&self, now: SystemTime) -> ToolActivityView {
        let mut tool = self.tool.clone();
        tool.elapsed_ms = duration_ms(self.started_ns, wall_clock_ns(now)).unwrap_or(0);
        tool
    }
}
pub(super) fn relevant(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Tool(ToolEvent::ToolStarted { .. } | ToolEvent::ToolCompleted { .. })
            | EventKind::Lifecycle(BusLifecycle::AgentRunStateChanged {
                to: AgentRunPhase::Stopped | AgentRunPhase::Done | AgentRunPhase::Error,
                ..
            })
    )
}
impl Projection {
    pub(super) fn observe_activity(&self, event: event_bus::Event) {
        match event.kind {
            EventKind::Tool(ToolEvent::ToolStarted {
                run_id: Some(run),
                call_id,
                parent_call_id,
                tool_name,
                mut input,
            }) => {
                let key = (run.clone(), call_id.clone());
                let (thread, epoch) = {
                    let state = self.state.lock().unwrap();
                    if state.activities.len() >= MAX_LIVE_TOOLS
                        && !state.activities.contains_key(&key)
                    {
                        return;
                    }
                    let Some(run) = state.models.runs.get(&run) else {
                        return;
                    };
                    (run.thread.clone(), state.epoch)
                };
                let Ok(run) = RunId::new(run) else { return };
                // Raw bus arguments are unsafe for display. Mask and format exactly once,
                // outside State's lock, using the same formatter as durable E2 rows.
                if let Some(input) = input.as_mut() {
                    self.activity_redactor.redact_json(input);
                }
                #[cfg(test)]
                self.activity_formats
                    .fetch_add(usize::from(input.is_some()), Ordering::Relaxed);
                let input = input
                    .as_ref()
                    .map(|v| storage::history::rows::tool_input(&tool_name, v, false));
                let Ok(formatted) = self.bodies.tool(
                    &thread,
                    &run,
                    storage::history::ToolRow {
                        tool_name,
                        call_id,
                        parent_call_id,
                        input,
                        is_error: false,
                        detail: None,
                        duration_ms: None,
                    },
                ) else {
                    return;
                };
                let live = LiveTool {
                    tool: ToolActivityView {
                        thread,
                        run,
                        call_id: formatted.call_id,
                        parent_call_id: formatted.parent_call_id,
                        tool_name: formatted.tool_name,
                        input_summary: formatted.input_summary,
                        input: formatted.input,
                        elapsed_ms: 0,
                    },
                    started_ns: wall_clock_ns(event.meta.wall_clock),
                };
                let mut state = self.state.lock().unwrap();
                // An invalidation while formatting cannot resurrect old activity.
                if state.epoch != epoch {
                    return;
                }
                let tool = live.at((self.activity_now)());
                state.activities.insert(key, live);
                state.publish(
                    UiEvent::Execution(ExecutionEvent::ToolStarted { tool }),
                    self.limits,
                );
            }
            EventKind::Tool(ToolEvent::ToolCompleted {
                run_id: Some(run),
                call_id,
                is_error,
                ..
            }) => {
                let mut state = self.state.lock().unwrap();
                let live = state.activities.remove(&(run.clone(), call_id.clone()));
                let Some(summary) = state.models.runs.get(&run) else {
                    return;
                };
                let event = ExecutionEvent::ToolFinished {
                    thread: summary.thread.clone(),
                    run: summary.id.clone(),
                    call_id,
                    status: if is_error {
                        ToolActivityStatus::Failed
                    } else {
                        ToolActivityStatus::Succeeded
                    },
                    duration_ms: live
                        .map(|v| v.started_ns)
                        .and_then(|start| duration_ms(start, wall_clock_ns(event.meta.wall_clock))),
                };
                if event.validate().is_ok() {
                    state.publish(UiEvent::Execution(event), self.limits);
                }
            }
            EventKind::Lifecycle(BusLifecycle::AgentRunStateChanged {
                run_id,
                to: AgentRunPhase::Stopped | AgentRunPhase::Done | AgentRunPhase::Error,
                ..
            }) => {
                let mut state = self.state.lock().unwrap();
                let keys: Vec<_> = state
                    .activities
                    .keys()
                    .filter(|(run, _)| run == &run_id)
                    .cloned()
                    .collect();
                for key in keys {
                    let tool = state.activities.remove(&key).unwrap().tool;
                    state.publish(
                        UiEvent::Execution(ExecutionEvent::ToolFinished {
                            thread: tool.thread,
                            run: tool.run,
                            call_id: tool.call_id,
                            status: ToolActivityStatus::Interrupted,
                            duration_ms: None,
                        }),
                        self.limits,
                    );
                }
            }
            _ => (),
        }
    }
}
