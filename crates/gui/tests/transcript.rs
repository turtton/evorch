use event_bus::{
    Event, FaultEvent, LifecycleEvent, MessageEvent, ProviderEvent, ProviderFailureKind,
    SkillDiagnosticKind,
};
use gui::model::transcript::{TranscriptEntry, TranscriptModel};
use gui::model::transcript_registry::{TranscriptKey, TranscriptRegistry};

fn failed(failure: ProviderFailureKind, run_id: Option<&str>) -> Event {
    Event::new(ProviderEvent::RequestFailed {
        request_id: "request-1".into(),
        provider: "provider-a".into(),
        profile: None,
        protocol: "openai-chat-completions".into(),
        model: "model-a".into(),
        streaming: true,
        duration_ms: 42,
        failure,
        run_id: run_id.map(str::to_owned),
    })
}

#[test]
fn provider_request_failed_retry_attempt_becomes_notice() {
    // Given: potentially transient attempt failures, not confirmed retry decisions.
    for (failure, label) in [
        (ProviderFailureKind::RateLimited, "rate limited"),
        (ProviderFailureKind::Server, "server error"),
        (ProviderFailureKind::Transport, "transport error"),
        (ProviderFailureKind::Http { status: 500 }, "HTTP 500"),
        (ProviderFailureKind::Http { status: 599 }, "HTTP 599"),
    ] {
        let mut model = TranscriptModel::new();
        // When: an attempt failure is applied.
        model.apply(&failed(failure, Some("run-1")));
        // Then: the classification is visible as a notice.
        assert_eq!(
            model.entries(),
            &[TranscriptEntry::Notice {
                text: format!(
                    "Provider request failed: provider-a/model-a — {label} (42 ms, retrying)"
                ),
            }]
        );
    }
}

#[test]
fn provider_request_failed_final_attempt_becomes_error() {
    // Given: failures classified as definite rather than transient.
    for (failure, label) in [
        (ProviderFailureKind::Auth, "authentication failed"),
        (ProviderFailureKind::Timeout, "timed out"),
        (ProviderFailureKind::InvalidResponse, "invalid response"),
        (ProviderFailureKind::Quota, "quota exceeded"),
        (ProviderFailureKind::Other, "unknown error"),
        (ProviderFailureKind::Http { status: 401 }, "HTTP 401"),
        (ProviderFailureKind::Http { status: 499 }, "HTTP 499"),
        (ProviderFailureKind::Http { status: 600 }, "HTTP 600"),
    ] {
        let mut model = TranscriptModel::new();
        // When: an attempt failure is applied.
        model.apply(&failed(failure, None));
        // Then: the failure is an error without a retry suffix.
        assert_eq!(
            model.entries(),
            &[TranscriptEntry::Error {
                text: format!("Provider request failed: provider-a/model-a — {label} (42 ms)"),
            }]
        );
    }
}

#[test]
fn fault_skill_diagnostic_becomes_error() {
    // Given: every skill diagnostic kind.
    for kind in [
        SkillDiagnosticKind::DiscoveryError,
        SkillDiagnosticKind::ValidationError,
        SkillDiagnosticKind::Shadowed,
    ] {
        let mut registry = TranscriptRegistry::new();
        let event = Event::new(FaultEvent::SkillDiagnostic {
            kind,
            skill: "demo".into(),
            scope: "repo".into(),
            detail: "diagnostic detail".into(),
        });
        // When: the diagnostic is applied.
        registry.apply(&event);
        // Then: only the thread receives a visible error with identifying context.
        assert_eq!(registry.route(&event), vec![TranscriptKey::Thread]);
        assert!(
            matches!(registry.thread().entries(), [TranscriptEntry::Error { text }]
            if text.contains("demo") && text.contains("repo") && text.contains("diagnostic detail"))
        );
        assert_eq!(registry.run_ids().count(), 0);
    }
}

#[test]
fn subscriber_lagged_renders_no_inline_notice() {
    // Given: a subscriber lost events.
    let mut registry = TranscriptRegistry::new();
    let event = Event::new(FaultEvent::SubscriberLagged {
        subscriber_id: 7,
        skipped: 12,
    });
    // When: the infrastructure fault is applied.
    registry.apply(&event);
    let mut model = TranscriptModel::new();
    model.apply(&event);
    // Then: neither direct projection nor routing renders an inline notice.
    assert_eq!(registry.route(&event), vec![TranscriptKey::Thread]);
    assert!(model.entries().is_empty());
    assert!(registry.thread().entries().is_empty());
    assert_eq!(registry.run_ids().count(), 0);
}

#[test]
fn registry_routes_request_failed_with_run_id_to_thread_and_run() {
    // Given: an attributed provider failure.
    let mut registry = TranscriptRegistry::new();
    registry.bind_thread_root("thread", "run-1");
    registry.select_thread(Some("thread".into()));
    let event = failed(ProviderFailureKind::Auth, Some("run-1"));
    // When: routing and applying the event.
    let routes = registry.route(&event);
    registry.apply(&event);
    // Then: both destinations receive exactly the same error.
    assert_eq!(
        routes,
        vec![TranscriptKey::Thread, TranscriptKey::Run("run-1".into())]
    );
    assert_eq!(
        registry.run("run-1").expect("run").entries(),
        registry.thread().entries()
    );
    assert!(matches!(
        registry.thread().entries(),
        [TranscriptEntry::Error { .. }]
    ));
}

#[test]
fn registry_routes_request_failed_without_run_id_to_thread_only() {
    // Given: a run-less provider failure.
    let mut registry = TranscriptRegistry::new();
    let event = failed(ProviderFailureKind::RateLimited, None);
    // When: routing and applying the event.
    let routes = registry.route(&event);
    registry.apply(&event);
    // Then: only the thread receives the notice.
    assert_eq!(routes, vec![TranscriptKey::Thread]);
    assert_eq!(registry.run_ids().count(), 0);
    assert!(matches!(
        registry.thread().entries(),
        [TranscriptEntry::Notice { .. }]
    ));
}

fn task_prompt(run: &str, parent: Option<&str>, prompt: &str) -> Event {
    Event::new(LifecycleEvent::TaskPromptPublished {
        run_id: run.into(),
        parent_run_id: parent.map(str::to_owned),
        agent_name: "agent".into(),
        role: "worker".into(),
        prompt: prompt.into(),
    })
}

#[test]
fn task_prompt_routes_root_only_to_run_and_child_to_owning_thread() {
    let mut registry = TranscriptRegistry::new();
    registry.bind_thread_root("owner", "root");
    registry.bind_run("child", "owner");
    registry.select_thread(Some("other".into()));
    let root = task_prompt("root", None, "main instruction");
    let child = task_prompt("child", Some("root"), "child instruction");
    assert_eq!(
        registry.route(&root),
        vec![TranscriptKey::Run("root".into())]
    );
    assert_eq!(
        registry.route(&child),
        vec![TranscriptKey::Thread, TranscriptKey::Run("child".into())]
    );
    registry.apply(&root);
    registry.apply(&child);
    assert!(registry.thread().entries().is_empty());
    registry.select_thread(Some("owner".into()));
    assert_eq!(
        registry.thread().entries(),
        &[TranscriptEntry::UserMessage {
            text: "child instruction".into()
        }]
    );
}

#[test]
fn task_prompt_dedups_exact_first_instruction_on_redelivery() {
    let mut model = TranscriptModel::new();
    model.push_user_message("instruction");
    model.push_notice("restored");
    model.apply(&task_prompt("run", Some("parent"), "instruction"));
    assert_eq!(model.entries().len(), 2);
    model.apply(&task_prompt("run", Some("parent"), "instruction "));
    assert_eq!(model.entries().len(), 3, "dedup must not trim text");
    model.apply(&task_prompt("run", Some("parent"), "instruction"));
    assert_eq!(
        model.entries().len(),
        3,
        "compare the first instruction, not the last entry"
    );
}

#[test]
fn final_result_routes_like_message_delta_including_background_and_non_root_runs() {
    let mut registry = TranscriptRegistry::new();
    registry.bind_thread_root("owner", "root");
    registry.bind_run("child", "owner");
    registry.select_thread(Some("other".into()));
    for run in ["root", "child", "unbound"] {
        let result = Event::new(MessageEvent::FinalResultPublished {
            run_id: run.into(),
            text: "canonical report".into(),
        });
        let delta = Event::new(MessageEvent::MessageDelta {
            run_id: Some(run.into()),
            delta: "canonical report".into(),
        });
        assert_eq!(registry.route(&result), registry.route(&delta));
        registry.apply(&result);
        assert_eq!(
            registry.run(run).unwrap().entries(),
            &[TranscriptEntry::Message {
                text: "canonical report".into(),
                run_id: Some(run.into()),
            }]
        );
    }
    assert!(registry.thread().entries().is_empty());
    registry.select_thread(Some("owner".into()));
    assert_eq!(
        registry.thread().entries(),
        registry.run("root").unwrap().entries()
    );
}

#[test]
fn final_result_dedups_last_message_for_same_run_even_after_other_entries() {
    let mut registry = TranscriptRegistry::new();
    registry.bind_thread_root("owner", "run");
    registry.select_thread(Some("owner".into()));
    for delta in ["canonical ", "report"] {
        registry.apply(&Event::new(MessageEvent::MessageDelta {
            run_id: Some("run".into()),
            delta: delta.into(),
        }));
    }
    registry.apply(&Event::new(event_bus::ToolEvent::ToolStarted {
        run_id: Some("run".into()),
        tool_name: "read".into(),
        call_id: "call".into(),
        input: None,
    }));
    let result = Event::new(MessageEvent::FinalResultPublished {
        run_id: "run".into(),
        text: "canonical report".into(),
    });
    registry.apply(&result);
    registry.apply(&result);
    for model in [registry.thread(), registry.run("run").unwrap()] {
        assert_eq!(
            model
                .entries()
                .iter()
                .filter(|entry| matches!(entry, TranscriptEntry::Message { .. }))
                .count(),
            1
        );
    }
    let mut model = registry.run("run").unwrap().clone();
    model.apply(&Event::new(MessageEvent::FinalResultPublished {
        run_id: "other".into(),
        text: "canonical report".into(),
    }));
    model.apply(&result);
    assert_eq!(model.entries().len(), 3, "other runs do not affect dedup");
    model.apply(&Event::new(MessageEvent::FinalResultPublished {
        run_id: "run".into(),
        text: "canonical report ".into(),
    }));
    assert_eq!(
        model.entries().len(),
        4,
        "only exact duplicates are skipped"
    );
    model.apply(&result);
    assert_eq!(
        model.entries().len(),
        5,
        "only the last message for that run is compared"
    );
}

#[test]
fn final_result_is_separate_from_progress_and_finishes_thinking_before_done() {
    let mut model = TranscriptModel::new();
    model.apply(&Event::new(MessageEvent::MessageDelta {
        run_id: Some("run".into()),
        delta: "Progress".into(),
    }));
    let result = Event::new(MessageEvent::FinalResultPublished {
        run_id: "run".into(),
        text: "Final report".into(),
    });
    model.apply(&result);
    assert_eq!(
        model.entries(),
        &[
            TranscriptEntry::Message {
                text: "Progress".into(),
                run_id: Some("run".into())
            },
            TranscriptEntry::Message {
                text: "Final report".into(),
                run_id: Some("run".into())
            },
        ]
    );
    model.apply(&Event::new(MessageEvent::ReasoningDelta {
        run_id: Some("run".into()),
        delta: "Thinking".into(),
    }));
    let id = model.visible_entry_id(2);
    assert!(model.thinking_is_streaming(id));
    model.apply(&result);
    assert!(!model.thinking_is_streaming(id));
    assert_eq!(
        model.entries().len(),
        3,
        "a deduped result still ends thinking"
    );
}
