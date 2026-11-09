//! 昇格系メタ操作 (handoff_to_lane) のハンドラ。
//!
//! Direct run が単独での解消を断念して Orchestrator root run へ昇格する
//! ための引継ぎメモ ([`EscalationMemo`]) を凍結・記録し、run を終端指示
//! [`Terminal::Escalate`](super::Terminal::Escalate) で返す。

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::json;
use tools::ToolResult;

use super::{DispatchResult, Terminal, error, parse};
use crate::agent_loop::LoopState;
use crate::escalation::EscalationMemo;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct HandoffToLaneArgs {
    original_request: String,
    escalation_reason: String,
    #[serde(default)]
    findings: Vec<String>,
    #[serde(default)]
    files_touched: Vec<PathBuf>,
    #[serde(default)]
    blockers: Vec<String>,
    #[serde(default)]
    workspace_state: String,
    #[serde(default)]
    suggested_next: String,
}

/// handoff_to_lane meta-op: メモを凍結・記録して run をエスカレーション終端させる。
///
/// fail-closed: 未知フィールド・必須フィールド欠落・空の original_request /
/// escalation_reason はいずれも error 結果となり run は継続する。
/// `source_run_id` は引数として受け付けず呼び出し元 run から導出する
/// (`deny_unknown_fields` によりモデル供給分は拒否される)。
///
/// メモの凍結順序: 終端指示を返す前に [`AgentRuntime::record_escalation_memo`]
/// で記録を先に完了させる。run 終端後もメモが確実に観測できることを
/// 記録時点で保証するためである。
pub(super) async fn handoff_to_lane(
    state: &LoopState,
    runtime: &crate::AgentRuntime,
    input: serde_json::Value,
) -> DispatchResult {
    if state.task.parent.is_some() || state.task.role != agents::Role::Worker {
        return error("handoff_to_lane is only available to root Worker runs");
    }
    let args = match parse::<HandoffToLaneArgs>(input) {
        Ok(args) => args,
        Err(message) => return error(message),
    };
    if args.original_request.is_empty() {
        return error("invalid arguments: original_request must not be empty");
    }
    if args.escalation_reason.is_empty() {
        return error("invalid arguments: escalation_reason must not be empty");
    }
    let source_run_id = state.caller_run_id();
    let memo = EscalationMemo {
        source_run_id,
        original_request: args.original_request,
        findings: args.findings,
        files_touched: args.files_touched,
        blockers: args.blockers,
        workspace_state: args.workspace_state,
        escalation_reason: args.escalation_reason,
        suggested_next: args.suggested_next,
    };
    if state
        .shared
        .executor
        .has_unobserved_shell_jobs(&source_run_id.to_string())
    {
        return error("Observe or stop shell jobs before handing off");
    }
    let prepared = state
        .continuity
        .freeze(runtime, source_run_id, &state.context.messages)
        .and_then(|continuity| {
            runtime.prepare_handoff_to_lane(
                &memo,
                state.run_config(),
                continuity,
                &state.task.publication,
            )
        });
    if let Err(reason) = prepared {
        return error(format!("handoff preparation failed: {reason}"));
    }
    runtime.record_escalation_memo(source_run_id, memo.clone());
    DispatchResult {
        result: ToolResult::success(
            json!({
                "escalated": true,
                "source_run_id": source_run_id.to_string(),
            })
            .to_string(),
        ),
        terminal: Terminal::Escalate(Box::new(memo)),
    }
}
