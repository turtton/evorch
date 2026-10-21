//! The prompt a draft triage thread starts from (ADR 0037).
//!
//! The thread may read the draft directory and nothing more: the prompt says
//! so, and leaves every status change and publication to the operator.

use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};

use storage::improvement::{ImprovementCandidate, ImprovementStatus};

/// Candidate rows the prompt lists before pointing at the directory instead.
pub const TRIAGE_MAX_ROWS: usize = 100;

/// A triage prompt for the candidates matching `status` (all when `None`),
/// listing at most `max_rows` of them. Draft paths are resolved against
/// `draft_dir`; a stored path that could leave it is shown as `-`.
pub fn triage_prompt(
    draft_dir: &Path,
    entries: &[ImprovementCandidate],
    status: Option<ImprovementStatus>,
    max_rows: usize,
) -> String {
    let dir = std::path::absolute(draft_dir).unwrap_or_else(|_| draft_dir.to_owned());
    let dir_text = dir.display();
    let matching: Vec<_> = entries
        .iter()
        .filter(|entry| status.is_none_or(|status| entry.status == status))
        .collect();
    let mut prompt = String::new();
    prompt.push_str(
        "Self-improvement が集めた改善候補の draft を triage してください。\
         重複や同根の候補をまとめ、それぞれの対応方針を提案するのが目的です。\n\n",
    );
    let _ = writeln!(prompt, "draft のディレクトリ: `{dir_text}`\n");
    prompt.push_str(
        "- このディレクトリは、この thread に read-only で追加されています。\n\
         - shell からも読めますが、書き込みはできません。\n\
         - 候補の DB は読めません。候補の一覧には下の表を使ってください。\n\
         - issue の投稿や外部への送信はしないでください（ADR 0011）。\n",
    );
    let _ = writeln!(
        prompt,
        "\n## 候補（Status: {}、{} 件）\n",
        status.map_or("all", ImprovementStatus::as_str),
        matching.len()
    );
    prompt.push_str("| id | code | title | occurrences | status | draft_path |\n");
    prompt.push_str("|---|---|---|---|---|---|\n");
    for entry in matching.iter().take(max_rows) {
        let path = entry
            .draft_path
            .as_deref()
            .and_then(|relative| draft_path(&dir, relative))
            .map_or_else(|| "-".to_owned(), |path| cell(&path.display().to_string()));
        let _ = writeln!(
            prompt,
            "| {} | {} | {} | {} | {} | {path} |",
            cell(&entry.id),
            cell(&entry.code),
            cell(&entry.title),
            entry.occurrences,
            entry.status.as_str(),
        );
    }
    let rest = matching.len().saturating_sub(max_rows);
    if rest > 0 {
        let _ = writeln!(
            prompt,
            "\n他 {rest} 件は `ls {dir_text}` で確認してください。"
        );
    }
    prompt.push_str(
        "\n## 手順\n\n\
         1. draft を grep / read で読む。\n\
         2. 重複・同根の候補をグループ化する。\n\
         3. 各グループを、Harness の欠陥 / 外部要因 / ノイズに分類する。\n\
         4. 推奨アクション（Dismiss / 要調査 / Issue 化候補）を、候補 ID 付きの表で出す。\n\
         \n## 禁止事項\n\n\
         - ファイルの書き込み、issue の投稿、外部への送信はしない。\n\
         - Dismiss などの状態変更は人間が Self-improvement ペインで行う。agent は提案にとどめる。\n",
    );
    prompt
}

/// Storage keeps a caller-supplied relative path; only plain names stay inside.
fn draft_path(dir: &Path, relative: &str) -> Option<PathBuf> {
    let relative = Path::new(relative);
    let mut parts = relative.components().peekable();
    (parts.peek().is_some() && parts.all(|part| matches!(part, Component::Normal(_))))
        .then(|| dir.join(relative))
}

/// Candidate text is untrusted; keep each value inside its table cell.
fn cell(text: &str) -> String {
    text.replace(['\r', '\n'], " ").replace('|', "\\|")
}

/// The GUI uses the runtime's protected-path rules; run start also knows scratch.
pub(crate) fn read_directory_error(
    dir: &Path,
    project: Option<&Path>,
    database: Option<&Path>,
) -> Option<String> {
    runtime::thread_read_dirs::validate_thread_read_directory(dir, project, database).err()
}

#[cfg(test)]
mod tests {
    use storage::improvement::{ImprovementSeverity, ImprovementSource};

    use super::*;

    fn candidate(id: &str, status: ImprovementStatus, draft: Option<&str>) -> ImprovementCandidate {
        ImprovementCandidate {
            id: id.into(),
            project: "p".into(),
            created_at_ns: 0,
            source: ImprovementSource::Diagnostic,
            code: "NoProgress".into(),
            severity: ImprovementSeverity::Warning,
            title: format!("Candidate {id}"),
            evidence: "evidence".into(),
            dedup_key: id.into(),
            status,
            draft_path: draft.map(Into::into),
            run_id: None,
            occurrences: 3,
            last_seen_at_ns: 0,
            recent_run_ids: Vec::new(),
        }
    }

    /// Table body rows, without the header and its separator.
    fn rows(prompt: &str) -> Vec<&str> {
        prompt
            .lines()
            .filter(|line| line.starts_with("| ") && !line.starts_with("| id |"))
            .collect()
    }

    #[test]
    fn rows_follow_the_header_order_with_absolute_draft_paths() {
        let prompt = triage_prompt(
            Path::new("/drafts"),
            &[candidate("c1", ImprovementStatus::New, Some("sub/c1.md"))],
            None,
            TRIAGE_MAX_ROWS,
        );
        assert!(prompt.contains("| id | code | title | occurrences | status | draft_path |\n"));
        assert_eq!(
            rows(&prompt),
            ["| c1 | NoProgress | Candidate c1 | 3 | new | /drafts/sub/c1.md |"]
        );
        assert!(prompt.contains("draft のディレクトリ: `/drafts`"));
    }

    #[test]
    fn draft_paths_that_could_leave_the_directory_are_not_listed() {
        let entries = [
            candidate("up", ImprovementStatus::New, Some("../secret.md")),
            candidate("nested", ImprovementStatus::New, Some("a/../../b.md")),
            candidate("absolute", ImprovementStatus::New, Some("/etc/passwd")),
            candidate("dot", ImprovementStatus::New, Some("./c.md")),
            candidate("empty", ImprovementStatus::New, Some("")),
            candidate("none", ImprovementStatus::New, None),
        ];
        let prompt = triage_prompt(Path::new("/drafts"), &entries, None, TRIAGE_MAX_ROWS);
        let rows = rows(&prompt);
        assert_eq!(rows.len(), entries.len());
        assert!(rows.iter().all(|row| row.ends_with("| - |")), "{rows:?}");
        assert!(!prompt.contains("secret.md") && !prompt.contains("/etc/passwd"));
    }

    #[test]
    fn rows_follow_the_status_filter_up_to_the_row_limit() {
        let mut entries: Vec<_> = (0..103)
            .map(|n| candidate(&format!("n{n}"), ImprovementStatus::New, None))
            .collect();
        entries.push(candidate("gone", ImprovementStatus::Dismissed, None));
        let drafts = Path::new("/drafts");
        let prompt = triage_prompt(drafts, &entries, Some(ImprovementStatus::New), 100);
        assert_eq!(rows(&prompt).len(), 100);
        assert!(!prompt.contains("| gone |"));
        assert!(prompt.contains("## 候補（Status: new、103 件）"));
        assert!(prompt.contains("他 3 件は `ls /drafts` で確認してください。"));
        let exact = triage_prompt(drafts, &entries[..100], None, 100);
        assert_eq!(rows(&exact).len(), 100);
        assert!(!exact.contains("他 "));
        let all = triage_prompt(drafts, &entries, None, 200);
        assert_eq!(rows(&all).len(), 104);
        assert!(all.contains("| gone |"));
    }

    #[test]
    fn untrusted_text_stays_inside_its_cell() {
        let mut entry = candidate("c1", ImprovementStatus::New, None);
        entry.title = "a | b\n| injected |".into();
        let prompt = triage_prompt(Path::new("/drafts"), &[entry], None, TRIAGE_MAX_ROWS);
        assert_eq!(
            rows(&prompt),
            [r"| c1 | NoProgress | a \| b \| injected \| | 3 | new | - |"]
        );
    }

    #[test]
    fn the_prompt_grants_reading_only_and_orders_its_steps() {
        let prompt = triage_prompt(
            Path::new("/drafts"),
            &[candidate("c1", ImprovementStatus::New, None)],
            None,
            TRIAGE_MAX_ROWS,
        );
        for text in [
            "read-only で追加されています",
            "shell からも読めますが、書き込みはできません",
            "候補の DB は読めません",
            "issue の投稿や外部への送信はしないでください（ADR 0011）",
            "## 禁止事項",
            "ファイルの書き込み、issue の投稿、外部への送信はしない",
            "Dismiss などの状態変更は人間が Self-improvement ペインで行う",
        ] {
            assert!(prompt.contains(text), "missing {text:?} in\n{prompt}");
        }
        let steps = [
            "1. draft を grep / read で読む",
            "2. 重複・同根の候補をグループ化する",
            "3. 各グループを、Harness の欠陥 / 外部要因 / ノイズに分類する",
            "4. 推奨アクション（Dismiss / 要調査 / Issue 化候補）を、候補 ID 付きの表で出す",
        ]
        .map(|step| {
            prompt
                .find(step)
                .unwrap_or_else(|| panic!("missing {step}"))
        });
        assert!(steps.is_sorted(), "{steps:?}");
    }
}
