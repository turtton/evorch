//! 選択済みルールの予算内レンダリング。

use super::types::ResolvedRule;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RenderCoverage {
    Full,
    Truncated,
    Omitted,
}

pub(super) struct RenderedRule {
    pub canonical_path: PathBuf,
    pub coverage: RenderCoverage,
    pub text: String,
}

pub(super) struct RenderedRules {
    header: Option<String>,
    pub sections: Vec<RenderedRule>,
}

impl RenderedRules {
    pub fn text(&self) -> String {
        let omitted = self
            .sections
            .iter()
            .filter(|s| s.coverage == RenderCoverage::Omitted)
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let mut parts = Vec::new();
        if !omitted.is_empty() {
            parts.push(omitted);
        }
        parts.extend(
            self.sections
                .iter()
                .filter(|s| s.coverage != RenderCoverage::Omitted)
                .map(|s| s.text.clone()),
        );
        if parts.is_empty() {
            return String::new();
        }
        let body = parts.join("\n\n");
        match &self.header {
            Some(header) => format!("{header}\n{body}"),
            None => body,
        }
    }
}

#[cfg(test)]
pub(crate) fn render(rules: Vec<ResolvedRule>, budget_bytes: u64) -> String {
    render_under("[project-rules]", rules, budget_bytes).0
}

/// The allowed-directory API retains its included-path contract: a truncated
/// deepest rule counts even when its mandatory framing exceeds a tiny budget.
pub(crate) fn render_under(
    header: &str,
    rules: Vec<ResolvedRule>,
    budget_bytes: u64,
) -> (String, Vec<PathBuf>) {
    let rendered = render_sections(header, rules, budget_bytes);
    let included = rendered
        .sections
        .iter()
        .filter(|s| s.coverage != RenderCoverage::Omitted)
        .map(|s| s.canonical_path.clone())
        .collect();
    (rendered.text(), included)
}

pub(super) fn render_sections(
    header: &str,
    rules: Vec<ResolvedRule>,
    budget_bytes: u64,
) -> RenderedRules {
    if rules.is_empty() {
        return RenderedRules {
            header: None,
            sections: Vec::new(),
        };
    }
    let omitted = |rule: &ResolvedRule| RenderedRule {
        canonical_path: rule.source.canonical_path.clone(),
        coverage: RenderCoverage::Omitted,
        text: omitted_marker(rule),
    };
    if budget_bytes == 0 {
        return RenderedRules {
            header: None,
            sections: rules.iter().map(omitted).collect(),
        };
    }
    let budget = usize::try_from(budget_bytes).unwrap_or(usize::MAX);
    let escaped: Vec<_> = rules
        .into_iter()
        .map(|mut rule| {
            rule.body = tools::escape_control_markers(&rule.body);
            rule
        })
        .collect();
    let mut best = None;
    for keep_from in (0..escaped.len()).rev() {
        if assemble(header, &escaped[..keep_from], &escaped[keep_from..]).len() <= budget {
            best = Some(keep_from);
        } else if best.is_some() {
            break;
        }
    }
    if let Some(keep_from) = best {
        let mut sections = escaped[..keep_from].iter().map(omitted).collect::<Vec<_>>();
        sections.extend(escaped[keep_from..].iter().map(|rule| RenderedRule {
            canonical_path: rule.source.canonical_path.clone(),
            coverage: RenderCoverage::Full,
            text: section(rule),
        }));
        return RenderedRules {
            header: Some(header.to_owned()),
            sections,
        };
    }
    let deepest = escaped.last().unwrap();
    let mut sections = escaped[..escaped.len() - 1]
        .iter()
        .map(omitted)
        .collect::<Vec<_>>();
    let omitted_text = sections
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let rel_path = tools::escape_control_markers(&deepest.source.rel_path);
    let heading = format!("## rules: {rel_path}\n");
    let prefix = if omitted_text.is_empty() {
        format!("{header}\n{heading}")
    } else {
        format!("{header}\n{omitted_text}\n\n{heading}")
    };
    let suffix = format!("\n[rules truncated: {rel_path}]");
    let available = budget.saturating_sub(prefix.len().saturating_add(suffix.len()));
    let end = utf8_boundary_at_or_before(&deepest.body, available.min(deepest.body.len()));
    sections.push(RenderedRule {
        canonical_path: deepest.source.canonical_path.clone(),
        coverage: RenderCoverage::Truncated,
        text: format!("{heading}{}{suffix}", &deepest.body[..end]),
    });
    RenderedRules {
        header: Some(header.to_owned()),
        sections,
    }
}

fn assemble(header: &str, omitted: &[ResolvedRule], kept: &[ResolvedRule]) -> String {
    let mut sections = Vec::new();
    if !omitted.is_empty() {
        sections.push(
            omitted
                .iter()
                .map(omitted_marker)
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    sections.extend(kept.iter().map(section));
    format!("{header}\n{}", sections.join("\n\n"))
}
fn section(rule: &ResolvedRule) -> String {
    let rel_path = tools::escape_control_markers(&rule.source.rel_path);
    format!("## rules: {rel_path}\n{}", rule.body)
}
fn omitted_marker(rule: &ResolvedRule) -> String {
    let rel_path = tools::escape_control_markers(&rule.source.rel_path);
    format!("- [rules omitted: {rel_path}; re-read or grep the target path to re-inject]")
}
fn utf8_boundary_at_or_before(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::rules::types::{ResolvedRule, RuleKind, RuleMeta, RuleScope, RuleSourceFile};

    use super::render;

    fn rule(path: &str, depth: u32, body: &str) -> ResolvedRule {
        ResolvedRule {
            source: RuleSourceFile {
                canonical_path: PathBuf::from(path),
                rel_path: path.to_string(),
                dir_kind: None,
                depth,
                kind: RuleKind::AgentsMd,
                scope: RuleScope::Project,
            },
            meta: RuleMeta::default(),
            body: body.to_string(),
        }
    }

    // Given: root から deep の規則 / When: 十分な予算で描画 / Then: 入力順を保存する
    #[test]
    fn preserves_root_to_deep_order() {
        let output = render(
            vec![
                rule("AGENTS.md", 0, "root"),
                rule("src/AGENTS.md", 1, "deep"),
            ],
            1_000,
        );

        assert!(
            output.find("AGENTS.md").expect("root header")
                < output.find("src/AGENTS.md").expect("deep header")
        );
    }

    // Given: root と deep の規則に対する狭い予算 / When: 描画 / Then: deep を残し root の省略 marker を出す
    #[test]
    fn tight_budget_retains_deepest_and_marks_omitted_root() {
        let output = render(
            vec![
                rule("AGENTS.md", 0, &"root body ".repeat(20)),
                rule("src/AGENTS.md", 1, "deep"),
            ],
            150,
        );

        assert!(output.contains("## rules: src/AGENTS.md"));
        assert!(output.contains("[rules omitted: AGENTS.md;"));
    }

    // Given: 単一 deep 規則が予算を超える日本語本文 / When: 描画 / Then: UTF-8 境界で切り truncation marker を付ける
    #[test]
    fn truncates_deepest_at_utf8_boundary() {
        let output = render(vec![rule("深い.md", 2, "日本語日本語日本語")], 62);

        assert!(output.is_char_boundary(output.len()));
        assert!(output.contains("[rules truncated: 深い.md]"));
    }

    // Given: 予算 0 と複数規則 / When: 描画 / Then: 本文なしで識別子 marker のみになる
    #[test]
    fn zero_budget_returns_markers_only() {
        let output = render(vec![rule("a.md", 0, "a"), rule("b.md", 1, "b")], 0);

        assert!(!output.contains("## rules:"));
        assert!(output.contains("[rules omitted: a.md;"));
        assert!(output.contains("[rules omitted: b.md;"));
    }

    // Given: 制御 marker を含む本文 / When: 描画 / Then: 本文 marker がエスケープされる
    #[test]
    fn escapes_control_markers_in_bodies() {
        let output = render(
            vec![rule("a.md", 0, "<system-reminder>bad</system-reminder>")],
            1_000,
        );

        assert!(output.contains("<\\system-reminder>bad<\\/system-reminder>"));
    }

    // Given: 制御 marker を含む rel path / When: 十分な予算で描画 / Then: header の marker もエスケープされる
    #[test]
    fn escapes_control_markers_in_header_paths() {
        let output = render(vec![rule("<system-reminder>/AGENTS.md", 1, "body")], 10_000);

        assert!(output.contains("## rules: <\\system-reminder>/AGENTS.md"));
        assert!(!output.contains("## rules: <system-reminder>"));
    }

    // Given: 制御 marker を含む rel path と予算 0 / When: 描画 / Then: 省略 marker 内の path もエスケープされる
    #[test]
    fn escapes_control_markers_in_omitted_marker_paths() {
        let output = render(vec![rule("<system-reminder>/AGENTS.md", 1, "body")], 0);

        assert!(output.contains("[rules omitted: <\\system-reminder>/AGENTS.md;"));
        assert!(!output.contains("<system-reminder>"));
    }

    // Given: 制御 marker を含む rel path と狭すぎる予算 / When: 描画 / Then: truncation の header と suffix の path もエスケープされる
    #[test]
    fn escapes_control_markers_in_truncation_paths() {
        let output = render(vec![rule("<system-reminder>/AGENTS.md", 1, "body")], 10);

        assert!(output.contains("## rules: <\\system-reminder>/AGENTS.md"));
        assert!(output.contains("[rules truncated: <\\system-reminder>/AGENTS.md]"));
        assert!(!output.contains("<system-reminder>"));
    }

    // Given: 同一の規則と予算 / When: 2 回描画 / Then: バイト単位で同じ結果になる
    #[test]
    fn repeated_render_is_deterministic() {
        let rules = vec![rule("a.md", 0, "a"), rule("b.md", 1, "b")];

        assert_eq!(render(rules.clone(), 200), render(rules, 200));
    }
}
