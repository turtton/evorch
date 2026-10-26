use providers::{ContentBlock, Message, Role as MessageRole, ToolResultContent};
use runtime::Role;
use runtime::base_context::{
    BaseContextReport, ContextSection, ContextSectionKind, ContextSource, ContextTool,
};

use super::*;

fn section(kind: ContextSectionKind, text: &str, tokens: u64) -> ContextSection {
    ContextSection {
        kind,
        source: ContextSource::Generated,
        text: text.into(),
        estimated_tokens: tokens,
    }
}

fn tool(name: &str, tokens: u64) -> ContextTool {
    ContextTool {
        name: name.into(),
        description: format!("{name} tool"),
        input_schema: serde_json::json!({}),
        estimated_tokens: tokens,
    }
}

fn report(role: Role, sections: Vec<ContextSection>, tools: Vec<ContextTool>) -> BaseContextReport {
    BaseContextReport {
        role,
        category: None,
        selected_model: "main/model".into(),
        context_window: 200_000,
        window_source: event_bus::WindowSource::Default,
        binding: None,
        sections,
        tools,
        excluded_tools: Vec::new(),
        exclusions: Vec::new(),
        available_skills: Vec::new(),
        reasoning_effort: None,
    }
}

#[test]
fn preview_segments_group_sections_and_drop_empty_groups() {
    let report = report(
        Role::Orchestrator,
        vec![
            section(ContextSectionKind::RoleBaseline, "role", 100),
            section(ContextSectionKind::ModelFamily, "family", 20),
            section(ContextSectionKind::IntentGate, "gate", 300),
            section(ContextSectionKind::Rules, "rules", 50),
            section(ContextSectionKind::Workspace, "ws", 5),
        ],
        vec![tool("read", 40), tool("delegate", 60)],
    );

    assert_eq!(
        preview_segments(&report),
        [
            BudgetSegment {
                label: "System prompt",
                tokens: 120
            },
            BudgetSegment {
                label: "Intent gate",
                tokens: 300
            },
            BudgetSegment {
                label: "Rules",
                tokens: 50
            },
            BudgetSegment {
                label: "Runtime notes",
                tokens: 5
            },
            BudgetSegment {
                label: "Tools",
                tokens: 100
            },
        ]
    );
}

#[test]
fn compare_rows_align_sections_and_flag_shared_text() {
    let orchestrator = report(
        Role::Orchestrator,
        vec![
            section(ContextSectionKind::RoleBaseline, "orch", 10),
            section(ContextSectionKind::IntentGate, "gate", 30),
            section(ContextSectionKind::Rules, "same rules", 7),
        ],
        vec![tool("read", 4), tool("delegate", 6)],
    );
    let worker = report(
        Role::Worker,
        vec![
            section(ContextSectionKind::RoleBaseline, "worker", 12),
            section(ContextSectionKind::Rules, "same rules", 7),
        ],
        vec![tool("read", 4), tool("shell", 9)],
    );

    let rows = compare_rows(&orchestrator, &worker);
    let row = |label: &str| rows.iter().find(|row| row.label == label).unwrap();
    assert_eq!(row("Role baseline").identical, Some(false));
    assert_eq!(
        (row("Intent gate").left, row("Intent gate").right),
        (Some(30), None)
    );
    assert_eq!(row("Rules").identical, Some(true));
    assert_eq!(row("Tools (2 / 2)").identical, Some(false));
    assert_eq!(
        (row("Total").left, row("Total").right),
        (Some(57), Some(32))
    );
    assert_eq!(
        tool_difference(&orchestrator, &worker),
        (vec!["delegate".to_owned()], vec!["shell".to_owned()])
    );
}

#[test]
fn message_rows_label_tool_traffic_and_summarize_first_line() {
    let messages = vec![
        Message {
            role: MessageRole::System,
            content: vec![ContentBlock::Text {
                text: "\nfirst line\nsecond".into(),
            }],
        },
        Message {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "c1".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "a"}),
            }],
        },
        Message {
            role: MessageRole::User,
            content: vec![ContentBlock::ToolResult {
                tool_call_id: "c1".into(),
                content: vec![ToolResultContent::Text {
                    text: "file body".into(),
                }],
                is_error: false,
            }],
        },
    ];

    let rows = message_rows(&messages);
    assert_eq!(
        rows.iter().map(|row| row.role).collect::<Vec<_>>(),
        ["system", "assistant", "tool result"]
    );
    assert_eq!(rows[0].summary, "first line");
    assert_eq!(rows[1].text, r#"→ read {"path":"a"}"#);
    assert_eq!(rows[2].text, "file body");
    assert_eq!(
        run_segments(&rows)
            .iter()
            .map(|segment| segment.label)
            .collect::<Vec<_>>(),
        ["system", "assistant", "tool result"]
    );
}

#[test]
fn anthropic_context_displays_plaintext_once_without_signatures_or_redacted_data() {
    let rows = message_rows(&[Message {
        role: MessageRole::Assistant,
        content: vec![
            ContentBlock::Anthropic {
                block: serde_json::json!({"type":"thinking","thinking":"visible reasoning","signature":"private-signature"}),
                prefix_hash: None,
            },
            ContentBlock::Anthropic {
                block: serde_json::json!({"type":"redacted_thinking","data":"private-encrypted-data"}),
                prefix_hash: None,
            },
            ContentBlock::Anthropic {
                block: serde_json::json!({"type":"text","text":"visible answer","citations":[]}),
                prefix_hash: None,
            },
            ContentBlock::Text {
                text: "visible answer".into(),
            },
        ],
    }]);
    assert_eq!(rows[0].summary, "visible reasoning");
    assert_eq!(rows[0].text.matches("visible answer").count(), 1);
    assert!(!rows[0].text.contains("private-"));
}

#[test]
fn preview_spec_defaults_delegated_roles_to_child_runs() {
    assert!(!PreviewSpec::new(Role::Orchestrator).child);
    assert!(PreviewSpec::new(Role::Worker).child);
    assert!(categories_for(Role::Worker).contains(&config::agent_categories::CategoryId::Deep));
    assert!(categories_for(Role::Orchestrator).is_empty());
    let mut spec = PreviewSpec::new(Role::Reviewer);
    spec.category = Some(config::agent_categories::CategoryId::PlanReview);
    assert_eq!(spec.request().category.as_deref(), Some("plan-review"));
    assert_eq!(compact_tokens(950), "950");
    assert_eq!(compact_tokens(12_400), "12.4K");
}

#[test]
fn preview_thread_is_forwarded_and_part_of_the_reuse_key() {
    let mut first = PreviewSpec::new(Role::Worker);
    first.thread_id = Some("triage-a".into());
    assert_eq!(first.request().thread_id.as_deref(), Some("triage-a"));
    let mut second = first.clone();
    second.thread_id = Some("triage-b".into());
    assert_ne!(first, second);
}

mod responses_native_items {
    use super::*;

    const CIPHERTEXT: &str = "gAAAAB3-gui-sentinel-ciphertext";
    const ITEM_ID: &str = "rs_gui_sentinel_item";

    fn native(ciphertext: &str) -> ContentBlock {
        ContentBlock::ResponsesReasoning {
            item: serde_json::json!({
                "type": "reasoning",
                "id": ITEM_ID,
                "summary": [{"type": "summary_text", "text": "visible summary"}],
                "encrypted_content": ciphertext,
                "future_field": {"native": true}
            }),
            origin: providers::ResolvedResponsesScope {
                dialect: providers::ResponsesDialect::Codex,
                profile: "work".into(),
                route_model: "gpt-5.1-codex".into(),
                wire_model: "gpt-5.1-codex".into(),
                speed: providers::ResponsesSpeed::Standard,
                service_tier: None,
                endpoint_fingerprint: "ab".repeat(32),
                account: providers::ResponsesAccount::Codex {
                    identity: "gui-account".into(),
                },
            },
        }
    }

    fn assistant(content: Vec<ContentBlock>) -> Message {
        Message {
            role: MessageRole::Assistant,
            content,
        }
    }

    fn json_tokens(message: &Message) -> u64 {
        u64::try_from(serde_json::to_string(message).unwrap().len())
            .unwrap()
            .div_ceil(4)
    }

    #[test]
    fn native_state_adds_no_text_or_blank_separator_but_keeps_its_budget() {
        // Given: a reply whose native item sits between the summary and the answer.
        let message = assistant(vec![
            ContentBlock::Reasoning {
                text: "visible summary".into(),
            },
            native(CIPHERTEXT),
            ContentBlock::Text {
                text: "visible answer".into(),
            },
        ]);
        let mut without_native = message.clone();
        without_native.content.remove(1);

        // When: the saved history is flattened for display.
        let rows = message_rows(std::slice::from_ref(&message));

        // Then: the text is identical to the reply without native state, yet the
        // budget counts the whole canonical JSON including item and origin.
        assert_eq!(
            rows[0].text,
            message_rows(&[without_native.clone()])[0].text
        );
        assert_eq!(rows[0].text, "visible summary\n\nvisible answer");
        assert_eq!(rows[0].summary, "visible summary");
        for private in [
            CIPHERTEXT,
            ITEM_ID,
            "future_field",
            "gui-account",
            "encrypted",
        ] {
            assert!(!rows[0].text.contains(private), "text leaked {private:?}");
        }
        assert_eq!(rows[0].tokens, json_tokens(&message));
        assert!(rows[0].tokens > json_tokens(&without_native));
    }

    #[test]
    fn native_only_message_is_budgeted_but_not_listed() {
        // Given: a native-only reply between two displayable messages.
        let native_only = assistant(vec![native(CIPHERTEXT)]);
        let messages = [
            Message {
                role: MessageRole::User,
                content: vec![ContentBlock::Text {
                    text: "question".into(),
                }],
            },
            native_only.clone(),
            assistant(vec![ContentBlock::Text {
                text: "answer".into(),
            }]),
        ];

        // When: the history is flattened.
        let rows = message_rows(&messages);

        // Then: no empty row is listed, but its tokens still reach the assistant budget.
        assert_eq!(
            rows.iter().map(|row| row.listed).collect::<Vec<_>>(),
            [true, false, true]
        );
        assert!(rows[1].text.is_empty() && rows[1].summary.is_empty());
        let assistant_budget = run_segments(&rows)
            .into_iter()
            .find(|segment| segment.label == "assistant")
            .map(|segment| segment.tokens);
        assert_eq!(
            assistant_budget,
            Some(json_tokens(&native_only) + json_tokens(&messages[2]))
        );
        assert_eq!(
            message_list_heading(&rows),
            (
                "Messages (2 of 3)".to_owned(),
                Some("1 replay-only message hidden · counted in the budget".to_owned())
            )
        );
        let mut twice = messages.to_vec();
        twice.push(native_only.clone());
        assert_eq!(
            message_list_heading(&message_rows(&twice)).1.as_deref(),
            Some("2 replay-only messages hidden · counted in the budget")
        );
        assert_eq!(
            message_list_heading(&message_rows(&[messages[0].clone(), messages[2].clone()])),
            ("Messages (2)".to_owned(), None)
        );
    }

    #[test]
    fn budget_estimate_follows_ceil_bytes_over_four_for_large_native_items() {
        // Given: ciphertexts straddling the 4n / 4n+1 boundary, escapes, Japanese and 1 MiB.
        let base = assistant(vec![native("")]);
        let base_len = serde_json::to_string(&base).unwrap().len();
        let aligned = "A".repeat((4 - base_len % 4) % 4 + 4);
        let cases = [
            aligned.clone(),
            format!("{aligned}A"),
            format!("{aligned}\"\\\n日本語"),
            "Q".repeat(1 << 20),
        ];
        let mut previous = None;
        for ciphertext in &cases[..2] {
            let message = assistant(vec![native(ciphertext)]);
            let tokens = message_rows(std::slice::from_ref(&message))[0].tokens;
            assert_eq!(tokens, json_tokens(&message));
            if let Some(previous) = previous {
                assert_eq!(tokens, previous + 1, "one byte past 4n starts a new token");
            }
            previous = Some(tokens);
        }
        for ciphertext in &cases[2..] {
            let message = assistant(vec![native(ciphertext)]);
            let rows = message_rows(std::slice::from_ref(&message));

            // Then: every estimate is exact and the run budget keeps it under "assistant".
            assert_eq!(rows[0].tokens, json_tokens(&message));
            assert_eq!(
                run_segments(&rows),
                [BudgetSegment {
                    label: "assistant",
                    tokens: rows[0].tokens
                }]
            );
        }
    }
}
