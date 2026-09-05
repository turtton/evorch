//! 昇格系メタ操作 (escalate) のハンドラ。
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
struct EscalateArgs {
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

/// escalate meta-op: メモを凍結・記録して run をエスカレーション終端させる。
///
/// fail-closed: 未知フィールド・必須フィールド欠落・空の original_request /
/// escalation_reason はいずれも error 結果となり run は継続する。
/// `source_run_id` は引数として受け付けず呼び出し元 run から導出する
/// (`deny_unknown_fields` によりモデル供給分は拒否される)。
///
/// メモの凍結順序: 終端指示を返す前に [`AgentRuntime::record_escalation_memo`]
/// で記録を先に完了させる。run 終端後もメモが確実に観測できることを
/// 記録時点で保証するためである。
pub(super) async fn escalate(
    state: &LoopState,
    runtime: &crate::AgentRuntime,
    input: serde_json::Value,
) -> DispatchResult {
    let args = match parse::<EscalateArgs>(input) {
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
