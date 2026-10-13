//! thread goals・thread todos・durable task に関するイベントスキーマを定義します。
//!
//! wire 形状は既存カテゴリと同じ隣接タグの二重ネスト
//! (`{"kind":"Orchestrator","payload":{"kind":…,"payload":{…}}}`) です。

use serde::{Deserialize, Serialize};

/// オーケストレーションループに関するイベント。
///
/// 既存カテゴリと同じ隣接タグの二重ネストで wire に載る。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload")]
pub enum OrchestratorEvent {
    /// Latest bounded durable state of a generic thread objective.
    ThreadGoalUpdated { snapshot: crate::ThreadGoalSnapshot },
    /// Latest durable procedure list, independent of the thread objective.
    ThreadTodoUpdated { snapshot: crate::ThreadTodoSnapshot },
    /// task の進捗を記録した。
    TaskProgressed {
        /// task の永続識別子。
        task_id: String,
        /// 進捗を報告した run の ID。
        run_id: String,
        /// task 固有の構造化進捗。
        progress: serde_json::Value,
        /// 進捗更新の理由。
        reason: String,
    },
    /// task の実行予算チェックポイントを記録した。
    TaskCheckpoint {
        /// task の永続識別子。
        task_id: String,
        /// 計測対象の run の ID。
        run_id: String,
        /// 累積ツール呼び出し回数。
        tool_call_count: u32,
        /// 累積入力トークン数。
        cumulative_input_tokens: u64,
        /// 累積出力トークン数。
        cumulative_output_tokens: u64,
        /// 経過ミリ秒。
        elapsed_ms: u64,
    },
}

#[cfg(test)]
mod tests {
    use crate::event::Event;

    use super::*;

    // Given: durable task の各バリアント
    // When: Event として JSON 往復する
    // Then: 値が保存され、外側タグ "Orchestrator" と内側隣接タグが期待どおりになる
    #[test]
    fn task_events_round_trip_with_adjacent_tags() {
        let cases: Vec<(&'static str, OrchestratorEvent)> = vec![
            (
                "TaskProgressed",
                OrchestratorEvent::TaskProgressed {
                    task_id: "task-1".into(),
                    run_id: "run-worker-1".into(),
                    progress: serde_json::json!({"completed": ["ac1"], "remaining": 2}),
                    reason: "criterion completed".into(),
                },
            ),
            (
                "TaskCheckpoint",
                OrchestratorEvent::TaskCheckpoint {
                    task_id: "task-1".into(),
                    run_id: "run-worker-1".into(),
                    tool_call_count: u32::MAX,
                    cumulative_input_tokens: u64::MAX,
                    cumulative_output_tokens: 4_294_967_296,
                    elapsed_ms: 4_294_967_297,
                },
            ),
        ];

        for (inner_tag, event) in cases {
            let outer = Event::new(event);
            let json = serde_json::to_string(&outer).expect("serialize Event");
            let restored: Event = serde_json::from_str(&json).expect("deserialize Event");
            assert_eq!(outer, restored, "round-trip mismatch: {inner_tag}");

            let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
            assert_eq!(
                value["kind"]["kind"], "Orchestrator",
                "outer tag mismatch: {inner_tag}"
            );
            assert_eq!(
                value["kind"]["payload"]["kind"], inner_tag,
                "inner tag mismatch"
            );
        }
    }
}
