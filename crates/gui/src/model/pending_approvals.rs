use std::collections::BTreeMap;

use event_bus::{Event, EventKind, ToolEvent};
use serde_json::Value;

use super::scoped_call::parse_scoped_call_id;

#[derive(Debug, Clone, PartialEq)]
pub struct PendingApproval {
    pub call_id: String,
    pub tool_name: String,
    pub run_id: Option<String>,
    pub attempt: Option<u64>,
    pub input: Option<Value>,
}

/// exact call ID をキーに、未解決の承認要求を挿入順で保持する。
#[derive(Debug, Default)]
pub struct PendingApprovalsModel {
    pending: BTreeMap<String, PendingApproval>,
    order: Vec<String>,
}

impl PendingApprovalsModel {
    /// run の補完と `(run ID, 元 call ID)` による引数取得を呼び出し元へ委譲する。
    /// 削除は `ApprovalResolved` の完全一致だけで行う。
    pub fn apply_event(
        &mut self,
        event: &Event,
        resolve_run: impl Fn(&str) -> Option<String>,
        resolve_input: impl Fn(&str, &str) -> Option<Value>,
    ) {
        match &event.kind {
            EventKind::Tool(ToolEvent::ApprovalRequested { tool_name, call_id }) => {
                let (run_id, original_call_id, attempt) = match parse_scoped_call_id(call_id) {
                    Some((run, original, attempt)) => (Some(run), original, attempt),
                    None => (resolve_run(call_id), call_id.clone(), None),
                };
                let input = run_id
                    .as_deref()
                    .and_then(|run| resolve_input(run, &original_call_id));
                let approval = PendingApproval {
                    call_id: call_id.clone(),
                    tool_name: tool_name.clone(),
                    run_id,
                    attempt,
                    input,
                };
                if self.pending.insert(call_id.clone(), approval).is_none() {
                    self.order.push(call_id.clone());
                }
            }
            EventKind::Tool(ToolEvent::ApprovalResolved { call_id, .. }) => {
                if self.pending.remove(call_id).is_some() {
                    self.order.retain(|id| id != call_id);
                }
            }
            EventKind::Tool(_)
            | EventKind::Lifecycle(_)
            | EventKind::Ledger(_)
            | EventKind::Message(_)
            | EventKind::Usage(_)
            | EventKind::Provider(_)
            | EventKind::Fault(_)
            | EventKind::AgentMessage(_)
            | EventKind::Compaction(_)
            | EventKind::Orchestrator(_)
            | EventKind::Diagnostic(_)
            | EventKind::Ownership(_)
            | EventKind::Snapshot(_) => {}
        }
    }

    pub fn get(&self, call_id: &str) -> Option<&PendingApproval> {
        self.pending.get(call_id)
    }

    pub fn items(&self) -> impl Iterator<Item = &PendingApproval> {
        self.order.iter().filter_map(|id| self.pending.get(id))
    }
}
