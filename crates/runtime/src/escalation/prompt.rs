use super::EscalationMemo;

// Static tool description: no per-run values, schema or ordering changes.
pub(crate) const TOOL_DESCRIPTION: &str = "End a Direct run with a persisted handoff memo for an Orchestrator. Include the original request and user-agreed constraints/completion criteria in original_request; adopted/rejected approaches and their reasons in findings; unverified items in blockers; concrete escalation reason and enough workspace_state/suggested_next for independent continuation. Pending questions are inherited with their existing IDs and original requester provenance; do not ask the same question under a new ID. Escalation is allowed with pending questions, but required answers must arrive and be observed before finish. Returns {escalated:true, source_run_id} after recording the memo. Missing/empty required text or unknown fields fails and leaves the run active; source_run_id is derived by runtime and must not be supplied.";

use std::fmt::Write;

/// 新規 Orchestrator root run 用の日本語引継ぎプロンプトを描画する。
pub(crate) fn render_escalation_prompt(
    memo: &EscalationMemo,
    questions: &[event_bus::UserQuestion],
) -> String {
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
    prompt.push_str("\n## Inherited unanswered questions\n既存の質問 ID と元 requester provenance を維持してください。同内容を新 ID で再 ask せず、ユーザーには継承先 thread の既存質問への回答を案内してください。必須回答は到着後に観測するまで finish できません。ユーザーと合意した制約・完了条件、採用/却下した案と理由、未確認事項を引き継ぎ、メモにない内容は合意済みと推測しないでください。\n");
    for question in questions
        .iter()
        .filter(|question| question.answer.is_none())
    {
        let _ = writeln!(
            prompt,
            "- ID: {} | requester: {} | root: {} ({}) | blocking: {}\n  Question: {}\n  Options: {:?}",
            question.id,
            question.run_id,
            question.root_run_id,
            question.root_name,
            question.blocking,
            question.title,
            question.options
        );
    }
    if questions.iter().all(|question| question.answer.is_some()) {
        prompt.push_str("- (none)\n");
    }
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

        let question = event_bus::UserQuestion {
            id: "question-original".into(),
            run_id: "run-7".into(),
            root_run_id: "run-7".into(),
            root_name: "chat:Worker:source".into(),
            recipient_run_ids: vec!["run-9".into()],
            title: "追加の制約は？".into(),
            options: vec!["選択肢 A".into()],
            blocking: true,
            answer: None,
        };
        let answered = event_bus::UserQuestion {
            id: "question-answered".into(),
            answer: Some("解決済み".into()),
            ..question.clone()
        };
        let prompt = render_escalation_prompt(&memo, &[question, answered]);
        assert!(!prompt.contains("question-answered"));

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
            "question-original",
            "requester: run-7",
            "root: run-7 (chat:Worker:source)",
            "blocking: true",
            "追加の制約は？",
            "選択肢 A",
            "新 ID で再 ask せず",
            "観測するまで finish できません",
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

        let prompt = render_escalation_prompt(&memo, &[]);

        assert_eq!(prompt.matches("- (none)").count(), 4);
    }
}
