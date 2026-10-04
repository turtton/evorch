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
