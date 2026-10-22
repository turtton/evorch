//! 観測専用の停滞検出器。提案イベントを返すだけで自動昇格は行わない。

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use event_bus::EscalationTrigger;
    use serde_json::json;
    use tools::ToolError;

    use super::{EscalationDetector, ToolErrorClass, ToolObservation, no_progress_detail};
    use crate::escalation::EscalationSettings;

    // Given: 各種提案 / When: NoProgress 判定 / Then: 連続編集失敗のみが対象になる
    #[test]
    fn only_consecutive_edit_failures_are_no_progress_triggers() {
        assert!(EscalationDetector::is_no_progress_trigger(
            &EscalationTrigger::ConsecutiveEditFailures { count: 3 }
        ));
        assert!(!EscalationDetector::is_no_progress_trigger(
            &EscalationTrigger::RepeatedFileRewrite {
                path: "src/lib.rs".into(),
                count: 5,
            }
        ));
        assert!(!EscalationDetector::is_no_progress_trigger(
            &EscalationTrigger::ToolCallThreshold { count: 200 }
        ));
    }

    // Given: 識別子・編集本文と複数行エラー / When: detail 生成 / Then: 安定サマリと限定した evidence のみ
    #[test]
    fn no_progress_detail_has_stable_summary_and_structured_evidence() {
        let input = json!({
            "path": "src/lib.rs",
            "old_string": "private old text",
            "new_string": "private new text",
            "call_id": "edit-2",
            "run_id": "run-changing",
        });
        let detail = no_progress_detail(
            &EscalationTrigger::ConsecutiveEditFailures { count: 2 },
            "edit",
            &input,
            Some(ToolErrorClass::EditTargetNotFound),
        );

        assert_eq!(
            detail,
            concat!(
                "consecutive edit failures reached threshold (tool=edit)\n",
                "trigger=consecutive_edit_failures\n",
                "count=2\n",
                "tool=edit\n",
                "path=src/lib.rs\n",
                "input_sha256=0cbba5f8fbf63c2b\n",
                "error=edit_target_not_found\n",
                "site=consecutive_edit_failures",
            )
        );
        for excluded in [
            "edit-2",
            "run-changing",
            "private old text",
            "private new text",
        ] {
            assert!(!detail.contains(excluded));
        }
        let other = no_progress_detail(
            &EscalationTrigger::ConsecutiveEditFailures { count: 5 },
            "edit",
            &json!({"path": "another.rs", "call_id": "edit-5"}),
            Some(ToolErrorClass::Other),
        );
        assert_eq!(detail.lines().next(), other.lines().next());
        assert_eq!(detail.lines().last(), other.lines().last());
    }

    // Given: path のない成功結果 / When: detail 生成 / Then: path・error・入出力本文は含まれない
    #[test]
    fn no_progress_detail_omits_absent_path_and_success_content() {
        let detail = no_progress_detail(
            &EscalationTrigger::ConsecutiveEditFailures { count: 2 },
            "write",
            &json!({"content": "private file text"}),
            None,
        );

        assert!(!detail.lines().any(|line| line.starts_with("path=")));
        assert!(!detail.lines().any(|line| line.starts_with("error=")));
        assert!(!detail.contains("private"));
        assert!(detail.lines().any(|line| line == "tool=write"));
        assert_eq!(
            detail.lines().last(),
            Some("site=consecutive_edit_failures")
        );
    }

    // Given: 型付きエラー / When: 分類 / Then: 本文ではなく固定の分類名になる
    #[test]
    fn typed_tool_errors_map_to_fixed_classes() {
        let sentinel = "PRIVATE_SENTINEL_typed".to_string();
        for (error, expected) in [
            (
                ToolError::InvalidArgs {
                    detail: sentinel.clone(),
                },
                "invalid_args",
            ),
            (
                ToolError::InvalidPattern {
                    detail: sentinel.clone(),
                },
                "invalid_args",
            ),
            (
                ToolError::PathNotFound {
                    path: sentinel.clone(),
                },
                "path_not_found",
            ),
            (
                ToolError::EditTargetNotFound {
                    path: sentinel.clone(),
                    edit: Some(1),
                },
                "edit_target_not_found",
            ),
            (
                ToolError::ExecutionDenied {
                    tool_name: "edit".into(),
                    reason: sentinel.clone(),
                },
                "permission_denied",
            ),
            (
                ToolError::Io {
                    detail: sentinel.clone(),
                },
                "other",
            ),
        ] {
            let class = ToolErrorClass::from_tool_error(&error);
            assert_eq!(class.as_str(), expected, "{error:?}");
            // 型を失った同じ本文でも、既知 prefix のものは同じ分類になる。
            if expected != "permission_denied" {
                assert_eq!(
                    ToolErrorClass::from_content(&error.to_string()),
                    class,
                    "{error:?}"
                );
            }
        }
    }

    // Given: 自由形式・多行・空の失敗本文 / When: detail 生成 / Then: 本文は出ず固定値 other になる
    #[test]
    fn untyped_error_content_never_reaches_detail() {
        for content in [
            "PRIVATE_SENTINEL_free first line".to_string(),
            "first\nPRIVATE_SENTINEL_second".into(),
            "あ".repeat(300),
            String::new(),
            // 既知 prefix の途中にしか一致しない本文は分類しない。
            "x ツールの引数が不正です: PRIVATE_SENTINEL".into(),
        ] {
            let class = ToolErrorClass::from_content(&content);
            assert_eq!(class, ToolErrorClass::Other, "{content}");
            let detail = no_progress_detail(
                &EscalationTrigger::ConsecutiveEditFailures { count: 2 },
                "edit",
                &json!({"new_string": content}),
                Some(class),
            );
            assert!(detail.lines().any(|line| line == "error=other"));
            assert!(!detail.contains("PRIVATE_SENTINEL"));
            assert!(!detail.contains('あ'));
        }
    }

    // Given: 改行を含む tool/path / When: detail 生成 / Then: evidence 行を偽装できず site は固定名
    #[test]
    fn no_progress_detail_keeps_field_values_on_one_line_and_site_path_free() {
        let detail = no_progress_detail(
            &EscalationTrigger::RepeatedFileRewrite {
                path: "trigger/path.rs".into(),
                count: 5,
            },
            "edit\nsite=spoofed",
            &json!({"path": "input/path.rs\r\nsite=spoofed"}),
            None,
        );
        assert!(
            detail
                .lines()
                .any(|line| line == "tool=edit\\nsite=spoofed")
        );
        assert!(
            detail
                .lines()
                .any(|line| line == "path=input/path.rs\\r\\nsite=spoofed")
        );
        assert_eq!(
            detail
                .lines()
                .filter(|line| line.starts_with("site="))
                .count(),
            1
        );
        assert_eq!(detail.lines().last(), Some("site=repeated_file_rewrite"));
        assert!(!detail.contains("trigger/path.rs"));
    }

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
use serde_json::Value;
use sha2::{Digest, Sha256};
use tools::ToolError;

use crate::{escalation::EscalationSettings, policy::is_meta_op};

/// NoProgress evidence の `error=` に出す固定の失敗分類。
///
/// ツールの失敗本文 (スキーマ検証の表示など) は不正な入力値をそのまま含みうるため、
/// 診断・候補 evidence・draft へは本文ではなくこの分類名だけを流す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolErrorClass {
    /// 引数がスキーマや契約に適合しない。
    InvalidArgs,
    /// 対象パスが存在しない。
    PathNotFound,
    /// edit の置換対象が見つからない。
    EditTargetNotFound,
    /// 承認方針や応答により実行が拒否された。
    PermissionDenied,
    /// 上記以外、または分類できない失敗。
    Other,
}

impl ToolErrorClass {
    /// evidence に書く固定名。
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidArgs => "invalid_args",
            Self::PathNotFound => "path_not_found",
            Self::EditTargetNotFound => "edit_target_not_found",
            Self::PermissionDenied => "permission_denied",
            Self::Other => "other",
        }
    }

    /// 型付きのツールエラーから分類する。
    pub(crate) const fn from_tool_error(error: &ToolError) -> Self {
        match error {
            ToolError::InvalidArgs { .. } | ToolError::InvalidPattern { .. } => Self::InvalidArgs,
            ToolError::PathNotFound { .. } => Self::PathNotFound,
            ToolError::EditTargetNotFound { .. } => Self::EditTargetNotFound,
            ToolError::ExecutionDenied { .. } => Self::PermissionDenied,
            _ => Self::Other,
        }
    }

    /// 型を失った失敗本文を、[`ToolError`] の固定文言 prefix だけで分類する。
    /// 本文そのものは返さず、一致しなければ [`Self::Other`] になる。
    pub(crate) fn from_content(content: &str) -> Self {
        [
            ToolError::InvalidArgs {
                detail: String::new(),
            },
            ToolError::InvalidPattern {
                detail: String::new(),
            },
            ToolError::PathNotFound {
                path: String::new(),
            },
            ToolError::EditTargetNotFound {
                path: String::new(),
                edit: None,
            },
        ]
        .iter()
        .find(|error| content.starts_with(&error.to_string()))
        .map_or(Self::Other, Self::from_tool_error)
    }
}

/// 安定したサマリと行単位の evidence を生成する。入力本文・失敗本文・実行識別子は含めない。
/// `error` は失敗した呼び出しの固定分類で、成功時は `None`。
pub(crate) fn no_progress_detail(
    trigger: &EscalationTrigger,
    tool: &str,
    input: &Value,
    error: Option<ToolErrorClass>,
) -> String {
    let (trigger_name, summary, count) = match trigger {
        EscalationTrigger::ConsecutiveEditFailures { count } => (
            "consecutive_edit_failures",
            "consecutive edit failures reached threshold",
            count,
        ),
        EscalationTrigger::RepeatedFileRewrite { count, .. } => (
            "repeated_file_rewrite",
            "repeated file rewrites reached threshold",
            count,
        ),
        EscalationTrigger::ToolCallThreshold { count } => {
            ("tool_call_threshold", "tool calls reached threshold", count)
        }
    };
    // identical_calls.rs と同じ JSON 表現の SHA-256 先頭16桁。引数本文は保存しない。
    let input_sha256: String = Sha256::digest(input.to_string().as_bytes())
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    // 改行を含む値から別の evidence field が作られることを防ぐ。
    let tool = tool.replace('\r', "\\r").replace('\n', "\\n");
    let mut lines = vec![
        format!("{summary} (tool={tool})"),
        format!("trigger={trigger_name}"),
        format!("count={count}"),
        format!("tool={tool}"),
    ];
    if let Some(path) = input.get("path").and_then(Value::as_str) {
        let path = path.replace('\r', "\\r").replace('\n', "\\n");
        lines.push(format!("path={path}"));
    }
    lines.push(format!("input_sha256={input_sha256}"));
    if let Some(error) = error {
        lines.push(format!("error={}", error.as_str()));
    }
    lines.push(format!("site={trigger_name}"));
    lines.join("\n")
}

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
    pub(crate) const fn is_no_progress_trigger(trigger: &EscalationTrigger) -> bool {
        matches!(trigger, EscalationTrigger::ConsecutiveEditFailures { .. })
    }

    pub(crate) const fn tool_calls(&self) -> u32 {
        self.tool_calls
    }

    /// 観測値を累積し、設定された条件に達した最初の提案を返す。
    pub(crate) fn observe(
        &mut self,
        obs: &ToolObservation,
        settings: &EscalationSettings,
    ) -> Option<EscalationTrigger> {
        if !is_meta_op(obs.tool) {
            self.tool_calls = self.tool_calls.saturating_add(1);
        }

        if self.proposed {
            return None;
        }

        if matches!(obs.tool, "edit" | "write") {
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
