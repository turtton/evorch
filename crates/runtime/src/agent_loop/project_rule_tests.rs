use super::*;
use crate::restore::{RestoredState, RunRestoreDescriptor};
use std::path::Path;

pub(super) fn seed(state: &mut LoopState, root: &Path) -> Arc<crate::RulesSource> {
    std::fs::write(root.join("AGENTS.md"), "SNAPSHOT-PROJECT-RULE").unwrap();
    let source = Arc::new(crate::RulesSource::new(
        crate::ProjectTrust::Approved,
        crate::RulesSettings {
            context_window_tokens: 1_000_000,
            response_headroom_tokens: 0,
            max_injection_bytes: 65536,
        },
        None,
        Some(root.to_path_buf()),
        None,
    ));
    let prepared = crate::rules::prepare_startup(&source, Some(root), None, 0).unwrap();
    state.context.push_user(&prepared.text);
    state.project_rule_injections.commit(prepared);
    source
}

pub(super) fn reinject_once(state: &mut LoopState, source: Arc<crate::RulesSource>, root: &Path) {
    let session = RulesSession::new(source, Some(root.to_path_buf()));
    let targets = [root.join("x.rs")];
    let prepared = crate::rules::prepare_after_successful_tools(
        &session,
        &targets,
        &state.project_rule_injections,
    )
    .expect("cleared history allows exactly one reinjection");
    state.context.push_user(&prepared.text);
    state.project_rule_injections.commit(prepared);
    assert!(
        crate::rules::prepare_after_successful_tools(
            &session,
            &targets,
            &state.project_rule_injections
        )
        .is_none()
    );
}

#[tokio::test]
async fn checkpoint_intent_prepared_and_terminal_snapshots_keep_rule_evidence_with_safe_history() {
    let (dir, _storage, runtime, mut state) =
        super::continuity_tests::stored_fixture_in("rule-snapshots.db", false).await;
    seed(&mut state, dir.path());
    let run = state.caller_run_id();
    let store = runtime.shared.run_store.get().unwrap();
    let assert_record = || {
        let record = store.restore_record(run).unwrap().unwrap();
        let descriptor: RunRestoreDescriptor = serde_json::from_str(&record.config_json).unwrap();
        assert!(!descriptor.project_rule_injections.is_empty());
        assert!(record.messages_json.contains("SNAPSHOT-PROJECT-RULE"));
        record
    };
    crate::restore::persist_checkpoint(&state).unwrap();
    let checkpoint = assert_record();
    let restored = RestoredState::from_record(&checkpoint).unwrap();
    assert_eq!(
        restored.project_rule_injections,
        state.project_rule_injections
    );
    state.context.push_assistant(providers::Message {
        role: providers::Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: "unfinished-read".into(),
            name: "read".into(),
            input: serde_json::json!({"path":dir.path().join("x.rs")}),
        }],
    });
    crate::restore::persist_tool_intent(&state).unwrap();
    let intent = assert_record();
    assert_eq!(intent.messages_json, checkpoint.messages_json);
    crate::restore::persist_prepared_context(&state, &state.context).unwrap();
    assert_eq!(assert_record().messages_json, checkpoint.messages_json);
    state.transition(AgentRunPhase::Running, None).unwrap();
    state.transition(AgentRunPhase::Done, None).unwrap();
    crate::restore::persist_terminal_snapshot(&state, &[]).unwrap();
    let terminal = assert_record();
    assert_eq!(terminal.terminal_phase, "Done");
    assert_eq!(terminal.messages_json, checkpoint.messages_json);
    runtime.cancel(run).unwrap();
}

#[tokio::test]
async fn preparation_failure_preserves_rules_and_the_last_committed_snapshot() {
    let (dir, _storage, runtime, mut state) =
        super::continuity_tests::stored_fixture_in("failed-rule-prepare.db", false).await;
    seed(&mut state, dir.path());
    crate::restore::persist_checkpoint(&state).unwrap();
    let run = state.caller_run_id();
    let store = runtime.shared.run_store.get().unwrap();
    let record = store.restore_record(run).unwrap().unwrap();
    let history = state.context.clone();
    let injections = state.project_rule_injections.clone();
    state.shared.compaction.max_summary_bytes = 0;
    assert!(
        crate::compaction::compact_now(&mut state, CompactionReason::Manual)
            .await
            .is_err()
    );
    assert_eq!(state.context, history);
    assert_eq!(state.project_rule_injections, injections);
    assert_eq!(store.restore_record(run).unwrap().unwrap(), record);
    runtime.cancel(run).unwrap();
}

#[tokio::test]
async fn a_scope_boundary_without_history_replacement_keeps_rule_evidence() {
    let (dir, _storage, runtime, mut state) =
        super::continuity_tests::stored_fixture_in("unchanged-rule-history.db", false).await;
    seed(&mut state, dir.path());
    let history = state.context.messages.clone();
    let injections = state.project_rule_injections.clone();
    crate::compaction::prepare_responses_scope(&mut state, "mock/model", None)
        .await
        .unwrap();
    assert_eq!(state.context.messages, history);
    assert_eq!(state.project_rule_injections, injections);
    let run = state.caller_run_id();
    let record = runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_record(run)
        .unwrap()
        .unwrap();
    let restored = RestoredState::from_record(&record).unwrap();
    assert_eq!(restored.project_rule_injections, injections);
    assert_eq!(restored.messages, history);
    runtime.cancel(run).unwrap();
}

#[tokio::test]
async fn absent_descriptor_field_defaults_to_empty_but_malformed_evidence_is_not_hidden() {
    let (dir, _storage, runtime, mut state) =
        super::continuity_tests::stored_fixture_in("legacy-rules.db", false).await;
    seed(&mut state, dir.path());
    crate::restore::persist_checkpoint(&state).unwrap();
    let run = state.caller_run_id();
    let mut record = runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_record(run)
        .unwrap()
        .unwrap();
    let mut descriptor: serde_json::Value = serde_json::from_str(&record.config_json).unwrap();
    let evidence = descriptor
        .as_object_mut()
        .unwrap()
        .remove("project_rule_injections")
        .unwrap();
    record.config_json = descriptor.to_string();
    let restored = RestoredState::from_record(&record).unwrap();
    assert!(restored.project_rule_injections.is_empty());
    let roundtrip: RunRestoreDescriptor = serde_json::from_str(&record.config_json).unwrap();
    assert!(
        serde_json::to_value(roundtrip)
            .unwrap()
            .get("project_rule_injections")
            .is_none()
    );
    for invalid in [
        serde_json::Value::Null,
        serde_json::json!({"/a":{"fingerprint":"x","coverage":"prefix"}}),
    ] {
        descriptor["project_rule_injections"] = invalid;
        record.config_json = descriptor.to_string();
        assert!(RestoredState::from_record(&record).is_err());
    }
    descriptor["project_rule_injections"] = evidence;
    record.config_json = descriptor.to_string();
    assert_eq!(
        RestoredState::from_record(&record)
            .unwrap()
            .project_rule_injections,
        state.project_rule_injections
    );
    runtime.cancel(run).unwrap();
}

#[tokio::test]
async fn fork_clears_all_inherited_rule_evidence_without_mutating_the_source() {
    let (dir, _storage, runtime, mut state) =
        super::continuity_tests::stored_fixture_in("fork-rules.db", false).await;
    let source = seed(&mut state, dir.path());
    state.context.push_assistant(providers::Message {
        role: providers::Role::Assistant,
        content: vec![ContentBlock::Text {
            text: "completed turn".into(),
        }],
    });
    crate::restore::persist_checkpoint(&state).unwrap();
    let run = state.caller_run_id();
    let record = runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_record(run)
        .unwrap()
        .unwrap();
    let count = crate::restore::conversation_len(&state.context.messages) as u64;
    let child = RestoredState::for_fork_seed(&record, count).unwrap();
    assert!(child.project_rule_injections.is_empty());
    assert!(!state.project_rule_injections.is_empty());
    let parent = state.project_rule_injections.clone();
    state.project_rule_injections = child.project_rule_injections;
    reinject_once(&mut state, source, dir.path());
    assert_eq!(state.project_rule_injections, parent);
    assert!(
        !RestoredState::from_record(&record)
            .unwrap()
            .project_rule_injections
            .is_empty()
    );
    runtime.cancel(run).unwrap();
}
