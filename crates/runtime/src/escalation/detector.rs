//! 観測専用の停滞検出器。提案イベントを返すだけで自動昇格は行わない。
#![cfg_attr(
    not(test),
    expect(dead_code, reason = "T7でruntimeへ接続するまでの純粋コンポーネント")
)]

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use event_bus::EscalationTrigger;

    use super::{EscalationDetector, ToolObservation};
    use crate::escalation::EscalationSettings;

    fn observation<'a>(tool: &'a str, path: Option<&str>, is_error: bool) -> ToolObservation<'a> {
        ToolObservation {
            tool,
            path: path.map(PathBuf::from),
            is_error,
        }
    }

    // Given: 既定設定と空の検出器
    // When: edit の失敗を3回連続して観測する
    // Then: 3回目に連続編集失敗の提案を返す
    #[test]
    fn proposes_after_three_consecutive_edit_failures() {
        let settings = EscalationSettings::default();
        let mut detector = EscalationDetector::default();

        assert_eq!(
            detector.observe(&observation("edit", None, true), &settings),
            None
        );
        assert_eq!(
            detector.observe(&observation("edit", None, true), &settings),
            None
        );
        assert_eq!(
            detector.observe(&observation("edit", None, true), &settings),
            Some(EscalationTrigger::ConsecutiveEditFailures { count: 3 })
        );
    }

    // Given: 既定設定と空の検出器
    // When: 失敗、成功、失敗、失敗、失敗の順にeditを観測する
    // Then: 成功で連続数がリセットされ、最後の失敗で初めて提案する
    #[test]
    fn successful_edit_resets_consecutive_failures() {
        let settings = EscalationSettings::default();
        let mut detector = EscalationDetector::default();

        for is_error in [true, false, true, true] {
            assert_eq!(
                detector.observe(&observation("edit", None, is_error), &settings),
                None
            );
        }
        assert_eq!(
            detector.observe(&observation("edit", None, true), &settings),
            Some(EscalationTrigger::ConsecutiveEditFailures { count: 3 })
        );
    }

    // Given: 既定設定と空の検出器
    // When: 同一パスへのeditを読み取りに挟んで5回観測する
    // Then: 5回目に同一ファイルの反復書き換えを提案する
    #[test]
    fn proposes_after_five_edits_to_same_path() {
        let settings = EscalationSettings::default();
        let mut detector = EscalationDetector::default();
        let path = "src/lib.rs";

        for index in 0..4 {
            assert_eq!(
                detector.observe(&observation("edit", Some(path), index % 2 == 0), &settings),
                None
            );
            assert_eq!(
                detector.observe(&observation("read", None, false), &settings),
                None
            );
        }
        assert_eq!(
            detector.observe(&observation("edit", Some(path), false), &settings),
            Some(EscalationTrigger::RepeatedFileRewrite {
                path: path.to_owned(),
                count: 5,
            })
        );
    }

    // Given: 既定設定と空の検出器
    // When: メタ操作を除く成功したツール呼び出しを200回観測する
    // Then: 200回目にツール呼び出し数の提案を返し、メタ操作は数えない
    #[test]
    fn proposes_after_two_hundred_non_meta_tool_calls() {
        let settings = EscalationSettings::default();
        let mut detector = EscalationDetector::default();

        assert_eq!(
            detector.observe(&observation("send", None, false), &settings),
            None
        );
        assert_eq!(
            detector.observe(&observation("finish", None, false), &settings),
            None
        );
        for _ in 0..199 {
            assert_eq!(
                detector.observe(&observation("read", None, false), &settings),
                None
            );
        }
        assert_eq!(
            detector.observe(&observation("read", None, false), &settings),
            Some(EscalationTrigger::ToolCallThreshold { count: 200 })
        );
    }

    // Given: 既定設定と健全な読み取り・異なるファイルへの編集
    // When: 観測を完了する
    // Then: エスカレーション提案は発火しない
    #[test]
    fn healthy_sequence_does_not_propose() {
        let settings = EscalationSettings::default();
        let mut detector = EscalationDetector::default();

        for obs in [
            observation("read", None, false),
            observation("edit", Some("a.rs"), false),
            observation("read", None, false),
            observation("edit", Some("b.rs"), false),
        ] {
            assert_eq!(detector.observe(&obs, &settings), None);
        }
    }

    // Given: 最初の提案が発火した検出器
    // When: さらにしきい値を超える観測を行う
    // Then: ラッチにより常に提案なしを返す
    #[test]
    fn latch_suppresses_all_observations_after_first_trigger() {
        let settings = EscalationSettings {
            consecutive_edit_failures: 1,
            ..EscalationSettings::default()
        };
        let mut detector = EscalationDetector::default();

        assert!(
            detector
                .observe(&observation("edit", None, true), &settings)
                .is_some()
        );
        assert_eq!(
            detector.observe(&observation("read", None, false), &settings),
            None
        );
        assert_eq!(
            detector.observe(&observation("edit", Some("a.rs"), false), &settings),
            None
        );
    }

    // Given: 2/2/3のカスタムしきい値
    // When: 各条件を個別にしきい値まで観測する
    // Then: 設定された低いしきい値がそれぞれ適用される
    #[test]
    fn honors_custom_thresholds() {
        let settings = EscalationSettings {
            consecutive_edit_failures: 2,
            same_file_rewrites: 2,
            tool_call_threshold: 3,
        };

        let mut failures = EscalationDetector::default();
        failures.observe(&observation("edit", None, true), &settings);
        assert!(matches!(
            failures.observe(&observation("edit", None, true), &settings),
            Some(EscalationTrigger::ConsecutiveEditFailures { count: 2 })
        ));

        let mut rewrites = EscalationDetector::default();
        rewrites.observe(&observation("edit", Some("a.rs"), false), &settings);
        assert!(matches!(
            rewrites.observe(&observation("edit", Some("a.rs"), false), &settings),
            Some(EscalationTrigger::RepeatedFileRewrite { count: 2, .. })
        ));

        let mut calls = EscalationDetector::default();
        calls.observe(&observation("read", None, false), &settings);
        calls.observe(&observation("read", None, false), &settings);
        assert!(matches!(
            calls.observe(&observation("read", None, false), &settings),
            Some(EscalationTrigger::ToolCallThreshold { count: 3 })
        ));
    }

    // Given: edit失敗数とツール呼び出し数が同時にしきい値へ達する設定
    // When: 2回目の失敗editを観測する
    // Then: 優先順位に従い連続編集失敗を提案する
    #[test]
    fn edit_failure_has_priority_over_tool_call_threshold() {
        let settings = EscalationSettings {
            consecutive_edit_failures: 1,
            tool_call_threshold: 2,
            ..EscalationSettings::default()
        };
        let mut detector = EscalationDetector::default();

        detector.observe(&observation("read", None, false), &settings);
        assert_eq!(
            detector.observe(&observation("edit", None, true), &settings),
            Some(EscalationTrigger::ConsecutiveEditFailures { count: 1 })
        );
    }
}

use std::{collections::BTreeMap, path::PathBuf};

use event_bus::EscalationTrigger;

use crate::{escalation::EscalationSettings, policy::is_meta_op};

/// 1回のツール実行について検出器へ渡す観測値。
#[derive(Debug)]
pub(crate) struct ToolObservation<'a> {
    /// 実行されたツール名。
    pub tool: &'a str,
    /// 編集対象のパス。編集以外では通常 `None`。
    pub path: Option<PathBuf>,
    /// ツール実行がエラーになったかどうか。
    pub is_error: bool,
}

/// 1 run 内の停滞兆候を観測し、最初の昇格提案だけを返す検出器。
#[derive(Debug, Default)]
pub(crate) struct EscalationDetector {
    consecutive_edit_failures: u32,
    edit_counts_by_path: BTreeMap<PathBuf, u32>,
    tool_calls: u32,
    proposed: bool,
}

impl EscalationDetector {
    /// 観測値を累積し、設定された条件に達した最初の提案を返す。
    pub(crate) fn observe(
        &mut self,
        obs: &ToolObservation,
        settings: &EscalationSettings,
    ) -> Option<EscalationTrigger> {
        if self.proposed {
            return None;
        }

        if !is_meta_op(obs.tool) {
            self.tool_calls = self.tool_calls.saturating_add(1);
        }

        if obs.tool == "edit" {
            if obs.is_error {
                self.consecutive_edit_failures = self.consecutive_edit_failures.saturating_add(1);
            } else {
                self.consecutive_edit_failures = 0;
            }

            if let Some(path) = &obs.path {
                let count = self.edit_counts_by_path.entry(path.clone()).or_default();
                *count = count.saturating_add(1);
            }
        }

        let trigger = if self.consecutive_edit_failures >= settings.consecutive_edit_failures {
            Some(EscalationTrigger::ConsecutiveEditFailures {
                count: self.consecutive_edit_failures,
            })
        } else if let Some((path, &count)) = self
            .edit_counts_by_path
            .iter()
            .find(|(_, count)| **count >= settings.same_file_rewrites)
        {
            Some(EscalationTrigger::RepeatedFileRewrite {
                path: path.to_string_lossy().into_owned(),
                count,
            })
        } else if self.tool_calls >= settings.tool_call_threshold {
            Some(EscalationTrigger::ToolCallThreshold {
                count: self.tool_calls,
            })
        } else {
            None
        };

        if trigger.is_some() {
            self.proposed = true;
        }
        trigger
    }
}
