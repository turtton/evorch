use super::EscalationMemo;

use std::fmt::Write;

/// 新規 Orchestrator root run 用の日本語引継ぎプロンプトを描画する。
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "エスカレーション handoff は後続タスクでこの描画器を呼び出す"
    )
)]
pub(crate) fn render_escalation_prompt(memo: &EscalationMemo) -> String {
    let mut prompt = format!(
        "[evorch escalation source_run_id={}]\n\n## 引継ぎ\nあなたは旧 Direct run からの昇格を受けた新規 Orchestrator root run です。以下のメモを引き継ぎ、必要な担当分割と実行計画を開始してください。\n\n## Source run ID\n{}\n\n## Original request\n{}\n\n## Findings\n",
        memo.source_run_id, memo.source_run_id, memo.original_request
    );
    append_lines(&mut prompt, memo.findings.iter().map(String::as_str));
    prompt.push_str("\n## Files touched\n");
    append_lines(
        &mut prompt,
        memo.files_touched.iter().map(|path| path.to_string_lossy()),
    );
    prompt.push_str("\n## Blockers\n");
    append_lines(&mut prompt, memo.blockers.iter().map(String::as_str));
    let _ = write!(
        prompt,
        "\n## Workspace state\n{}\n\n## Escalation reason\n{}\n\n## Suggested next\n{}\n",
        memo.workspace_state, memo.escalation_reason, memo.suggested_next
    );
    prompt
}

fn append_lines<T: std::fmt::Display>(output: &mut String, lines: impl Iterator<Item = T>) {
    let mut rendered = false;
    for line in lines {
        let _ = writeln!(output, "- {line}");
        rendered = true;
    }
    if !rendered {
        output.push_str("- (none)\n");
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::render_escalation_prompt;
    use crate::RunId;
    use crate::escalation::EscalationMemo;

    // Given: 全項目を持つ昇格メモ / When: 引継ぎプロンプトを描画 / Then: run ID と各値が含まれる
    #[test]
    fn render_escalation_prompt_includes_takeover_and_all_memo_values() {
        let memo = EscalationMemo {
            source_run_id: RunId::new(9),
            original_request: "元の依頼".to_string(),
            findings: vec!["発見事項".to_string()],
            files_touched: vec![PathBuf::from("src/example.rs")],
            blockers: vec!["阻害要因".to_string()],
            workspace_state: "workspace 状態".to_string(),
            escalation_reason: "昇格理由".to_string(),
            suggested_next: "次の提案".to_string(),
        };

        let prompt = render_escalation_prompt(&memo);

        for value in [
            "Direct run からの昇格",
            "run-9",
            "元の依頼",
            "発見事項",
            "src/example.rs",
            "阻害要因",
            "workspace 状態",
            "昇格理由",
            "次の提案",
        ] {
            assert!(prompt.contains(value), "missing prompt value: {value}");
        }
    }

    // Given: findings と blockers が空の昇格メモ / When: 引継ぎプロンプトを描画 / Then: 空欄は (none) で表現される
    #[test]
    fn render_escalation_prompt_marks_empty_findings_and_blockers() {
        let memo = EscalationMemo {
            source_run_id: RunId::new(1),
            original_request: "元の依頼".to_string(),
            findings: Vec::new(),
            files_touched: Vec::new(),
            blockers: Vec::new(),
            workspace_state: "clean".to_string(),
            escalation_reason: "理由".to_string(),
            suggested_next: "次".to_string(),
        };

        let prompt = render_escalation_prompt(&memo);

        assert_eq!(prompt.matches("- (none)").count(), 3);
    }
}
