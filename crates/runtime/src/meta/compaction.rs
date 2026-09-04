//! コンテキスト圧縮を要求する `compact` メタ操作ハンドラ (issue #63 / AC2, AC8)。

use event_bus::CompactionReason;
use serde_json::json;

use super::{DispatchResult, EmptyArgs, error, parse, success};
use crate::agent_loop::LoopState;
use crate::compaction::compact_now;

pub(super) async fn compact(state: &mut LoopState, input: serde_json::Value) -> DispatchResult {
    if let Err(message) = parse::<EmptyArgs>(input) {
        return error(message);
    }
    match compact_now(state, CompactionReason::Agent).await {
        Ok(outcome) => success(
            json!({
                "checkpoint_id": outcome.checkpoint_id,
                "estimated_tokens_before": outcome.estimated_tokens_before,
                "estimated_tokens_after": outcome.estimated_tokens_after,
                "still_above_threshold": outcome.still_above_threshold,
                "reason": CompactionReason::Agent,
            })
            .to_string(),
        ),
        Err(compaction_error) => error(compaction_error.to_string()),
    }
}
