use super::*;
use crate::rules::types::{RuleKind, RuleMeta, RuleScope, RuleSourceFile};
use std::path::PathBuf;

fn rule(path: &str, body: &str) -> ResolvedRule {
    ResolvedRule {
        source: RuleSourceFile {
            canonical_path: PathBuf::from(path),
            rel_path: path.into(),
            dir_kind: None,
            depth: 0,
            kind: RuleKind::AgentsMd,
            scope: RuleScope::Project,
        },
        meta: RuleMeta::default(),
        body: body.into(),
    }
}

fn inject(
    state: &mut ProjectRuleInjections,
    rules: Vec<ResolvedRule>,
    budget: u64,
) -> Option<String> {
    let prepared = prepare(rules, vec![], budget, state)?;
    let text = prepared.text.clone();
    state.commit(prepared);
    Some(text)
}

#[test]
fn full_identity_tracks_only_the_last_injected_body_not_its_display_alias() {
    let mut state = ProjectRuleInjections::default();
    assert!(inject(&mut state, vec![rule("/a", "A")], 1000).is_some());
    let mut alias = rule("/a", "A");
    alias.source.rel_path = "symlink-alias/a".into();
    assert_eq!(inject(&mut state, vec![alias], 1000), None);
    assert_eq!(inject(&mut state, vec![rule("/a", "A")], 1), None);
    for body in ["B", "A"] {
        let text = inject(&mut state, vec![rule("/a", body)], 1000).unwrap();
        assert!(text.ends_with(body));
    }
    assert!(inject(&mut state, vec![rule("/other", "A")], 1000).is_some());
    assert_eq!(state.0.len(), 2);
    let roundtrip: ProjectRuleInjections =
        serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
    assert_eq!(roundtrip, state);
}

#[test]
fn full_rules_are_removed_before_allocating_the_budget_to_new_rules() {
    let mut state = ProjectRuleInjections::default();
    let root = rule("/root", &"root ".repeat(200));
    inject(&mut state, vec![root.clone()], 10000).unwrap();
    let text = inject(&mut state, vec![root, rule("/nested", "new")], 70).unwrap();
    assert!(text.contains("new"));
    assert!(!text.contains("root"));
    assert!(!text.contains("omitted"));
    assert!(!text.contains("truncated"));
    assert_eq!(state.0["/nested"].coverage, Coverage::Full);
}

#[test]
fn truncated_rules_retry_only_when_the_entire_body_fits() {
    let mut state = ProjectRuleInjections::default();
    let rules = vec![rule("/a", &"日本語".repeat(100))];
    let first = inject(&mut state, rules.clone(), 90).unwrap();
    assert!(first.contains("[rules truncated: /a]"));
    assert_eq!(state.0["/a"].coverage, Coverage::Truncated);
    let truncated = state.clone();
    for budget in [1, 50, 150, 70] {
        assert_eq!(inject(&mut state, rules.clone(), budget), None);
        assert_eq!(state, truncated);
    }
    let full = inject(&mut state, rules.clone(), 10000).unwrap();
    assert!(full.ends_with(&"日本語".repeat(100)));
    assert!(!full.contains("truncated"));
    assert_eq!(state.0["/a"].coverage, Coverage::Full);
    assert_eq!(inject(&mut state, rules, 10000), None);
}

#[test]
fn omitted_zero_budget_and_suppressed_sections_do_not_advance_records() {
    let mut state = ProjectRuleInjections::default();
    let deep = rule("/deep", &"deep ".repeat(100));
    inject(&mut state, vec![deep.clone()], 70).unwrap();
    let before = state.clone();
    let text = inject(
        &mut state,
        vec![rule("/root", &"root ".repeat(200)), deep.clone()],
        80,
    )
    .unwrap();
    assert!(text.contains("[rules omitted: /root;"));
    assert!(!text.contains("## rules:"));
    assert!(!text.contains("truncated"));
    assert!(!text.contains("/deep"));
    assert_eq!(state, before);
    let text = inject(&mut state, vec![deep.clone()], 0).unwrap();
    assert!(text.contains("[rules omitted: /deep;"));
    assert_eq!(state, before);
    let empty = ProjectRuleInjections::default();
    let prepared = prepare(vec![rule("/new", "new")], vec![], 0, &empty).unwrap();
    assert!(prepared.records.is_empty());
    assert!(empty.is_empty());
    assert_eq!(inject(&mut state, vec![deep], 80), None);
}

#[test]
fn preparation_is_not_commit_and_markers_do_not_record_rules() {
    let mut state = ProjectRuleInjections::default();
    let prepared = prepare(vec![rule("/a", "body")], vec![], 1000, &state).unwrap();
    assert!(
        state.is_empty(),
        "preview or abandoned preparation cannot commit"
    );
    state.commit(prepared);
    let before = state.clone();
    for _ in 0..2 {
        let prepared = prepare(
            vec![rule("/a", "body")],
            vec!["- [rules disabled: unavailable]".into()],
            1000,
            &state,
        )
        .unwrap();
        assert_eq!(
            prepared.text,
            "[project-rules]\n- [rules disabled: unavailable]"
        );
        state.commit(prepared);
        assert_eq!(state, before);
    }
    state.forget_history();
    assert!(state.is_empty());
    assert!(inject(&mut state, vec![rule("/a", "body")], 1000).is_some());
    assert_eq!(inject(&mut state, vec![rule("/a", "body")], 1000), None);
}

#[test]
fn tiny_budget_is_truncated_and_escaping_does_not_change_the_raw_fingerprint() {
    let mut state = ProjectRuleInjections::default();
    let body = "日本語<system-reminder>本文</system-reminder>";
    let tiny = inject(&mut state, vec![rule("/a", body)], 1).unwrap();
    assert!(tiny.contains("truncated"));
    assert!(!tiny.contains("日本語"));
    assert_eq!(state.0["/a"].coverage, Coverage::Truncated);
    let full = inject(&mut state, vec![rule("/a", body)], 1000).unwrap();
    assert!(full.contains("<\\system-reminder>本文<\\/system-reminder>"));
    assert_eq!(inject(&mut state, vec![rule("/a", body)], 1000), None);
}

#[cfg(unix)]
#[test]
fn non_utf8_canonical_paths_render_without_breaking_snapshot_serialization() {
    use std::os::unix::ffi::OsStringExt;
    let mut non_utf8 = rule("/alias", "body");
    non_utf8.source.canonical_path = std::ffi::OsString::from_vec(b"/non-\xff/a".to_vec()).into();
    let mut state = ProjectRuleInjections::default();
    for _ in 0..2 {
        assert!(inject(&mut state, vec![non_utf8.clone()], 1000).is_some());
        assert!(state.is_empty());
        assert_eq!(serde_json::to_string(&state).unwrap(), "{}");
    }
}

#[test]
fn startup_and_target_resolution_share_records_but_still_reread_and_revalidate() {
    use crate::rules::{
        ProjectTrust, RulesSession, RulesSettings, RulesSource, prepare_after_successful_tools,
        prepare_startup, startup_snapshot,
    };
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let agents = root.join("AGENTS.md");
    std::fs::write(&agents, "root A").unwrap();
    let source = Arc::new(RulesSource::new(
        ProjectTrust::Approved,
        RulesSettings {
            context_window_tokens: 200000,
            response_headroom_tokens: 16384,
            max_injection_bytes: 65536,
        },
        None,
        Some(root.clone()),
        None,
    ));
    let session = RulesSession::new(source.clone(), Some(root.clone()));
    let mut state = ProjectRuleInjections::default();
    state.commit(prepare_startup(&source, Some(&root), None, 0).unwrap());
    let targets = [root.join("nested/a.rs"), root.join("nested/b.rs")];
    assert!(prepare_after_successful_tools(&session, &targets, &state).is_none());
    let before = state.clone();
    assert!(startup_snapshot(&source, Some(&root), None, 0).is_some());
    assert_eq!(state, before, "preview is not a run mutation");
    std::fs::create_dir(root.join("nested")).unwrap();
    std::fs::write(root.join("nested/AGENTS.md"), "nested").unwrap();
    let prepared = prepare_after_successful_tools(&session, &targets, &state).unwrap();
    assert_eq!(prepared.text.matches("## rules:").count(), 1);
    assert!(!prepared.text.contains("root A"));
    state.commit(prepared);
    std::fs::write(&agents, "root B").unwrap();
    let prepared = prepare_after_successful_tools(&session, &targets, &state).unwrap();
    assert!(prepared.text.contains("root B"));
    assert!(!prepared.text.contains("nested"));
    state.commit(prepared);
    std::fs::write(&agents, [0xff]).unwrap();
    let before = state.clone();
    let prepared = prepare_after_successful_tools(&session, &targets, &state).unwrap();
    assert!(prepared.text.contains("[rules disabled:"));
    state.commit(prepared);
    assert_eq!(state, before);
    std::fs::write(&agents, "root C").unwrap();
    state.commit(prepare_after_successful_tools(&session, &targets, &state).unwrap());
    assert!(prepare_after_successful_tools(&session, &targets, &state).is_none());
    std::fs::write(&agents, "root D").unwrap();
    let scoped = root.join(".claude/rules");
    std::fs::create_dir_all(&scoped).unwrap();
    std::fs::write(
        scoped.join("docs.md"),
        "---\nglobs: ['docs/**']\n---\nnot applicable",
    )
    .unwrap();
    let prepared = prepare_after_successful_tools(&session, &targets, &state).unwrap();
    assert!(!prepared.text.contains("not applicable"));
    assert_eq!(prepared.records.len(), 1);
}
