//! View models for the Context tab: budget segments, run message rows and role comparison.
//!
//! Pure functions over runtime reports so the pane only lays out what these return.

use providers::{ContentBlock, Message, Role as MessageRole, ToolResultContent};
use runtime::Role;
use runtime::base_context::{
    BaseContextReport, BaseContextRequest, ContextSectionKind, ContextSource, RunContextView,
    estimate_text_tokens,
};

/// Roles a run can be started as, in display order.
pub const ROLES: [Role; 8] = [
    Role::Orchestrator,
    Role::Worker,
    Role::Explorer,
    Role::Reviewer,
    Role::Planner,
    Role::WebResearcher,
    Role::Oracle,
    Role::MultimodalLooker,
];

/// The run shape the Preview view asks the runtime to compose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewSpec {
    pub role: Role,
    pub category: Option<String>,
    pub conversation: bool,
    pub child: bool,
    pub isolated: bool,
}

impl PreviewSpec {
    pub fn new(role: Role) -> Self {
        Self {
            role,
            category: None,
            conversation: false,
            child: role != Role::Orchestrator,
            isolated: false,
        }
    }

    pub fn request(&self) -> BaseContextRequest {
        BaseContextRequest {
            category: self.category.clone(),
            conversation: self.conversation,
            child: self.child,
            workspace_mode: if self.isolated {
                runtime::WorkspaceMode::Isolated
            } else {
                runtime::WorkspaceMode::Shared
            },
            ..BaseContextRequest::new(self.role)
        }
    }
}

/// Categories selectable for `role`; only workers and reviewers take one.
pub fn categories_for(role: Role) -> Vec<&'static str> {
    let key = match role {
        Role::Worker => "worker",
        Role::Reviewer => "reviewer",
        _ => return Vec::new(),
    };
    config::agent_categories::settings_categories()
        .filter(|category| category.role == key)
        .map(|category| category.name)
        .collect()
}

/// One stacked-bar segment of the token budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetSegment {
    pub label: &'static str,
    pub tokens: u64,
}

const PROMPT_KINDS: [ContextSectionKind; 4] = [
    ContextSectionKind::RoleBaseline,
    ContextSectionKind::ModelFamily,
    ContextSectionKind::CategoryOverlay,
    ContextSectionKind::Appendix,
];

/// Budget segments of a preview, largest groups first in join order; empty groups are dropped.
pub fn preview_segments(report: &BaseContextReport) -> Vec<BudgetSegment> {
    let sum = |kinds: &[ContextSectionKind]| {
        report
            .sections
            .iter()
            .filter(|section| kinds.contains(&section.kind))
            .map(|section| section.estimated_tokens)
            .sum()
    };
    [
        BudgetSegment {
            label: "System prompt",
            tokens: sum(&PROMPT_KINDS),
        },
        BudgetSegment {
            label: "Intent gate",
            tokens: sum(&[ContextSectionKind::IntentGate]),
        },
        BudgetSegment {
            label: "Skills",
            tokens: sum(&[ContextSectionKind::Skills]),
        },
        BudgetSegment {
            label: "Rules",
            tokens: sum(&[ContextSectionKind::Rules]),
        },
        BudgetSegment {
            label: "Runtime notes",
            tokens: sum(&[
                ContextSectionKind::CompactionPolicy,
                ContextSectionKind::Workspace,
                ContextSectionKind::Memory,
            ]),
        },
        BudgetSegment {
            label: "Tools",
            tokens: report.tool_tokens(),
        },
    ]
    .into_iter()
    .filter(|segment| segment.tokens > 0)
    .collect()
}

/// A provider message flattened for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRow {
    pub role: &'static str,
    /// First line, for the list.
    pub summary: String,
    pub text: String,
    pub tokens: u64,
}

/// Flatten persisted messages; tool calls and results render inline.
pub fn message_rows(messages: &[Message]) -> Vec<MessageRow> {
    messages
        .iter()
        .map(|message| {
            let text = message
                .content
                .iter()
                .map(block_text)
                .collect::<Vec<_>>()
                .join("\n\n");
            let summary = text
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or_default()
                .chars()
                .take(120)
                .collect();
            MessageRow {
                role: role_label(message),
                tokens: estimate_text_tokens(&serde_json::to_string(message).unwrap_or_default()),
                summary,
                text,
            }
        })
        .collect()
}

fn role_label(message: &Message) -> &'static str {
    let tool_result = message
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolResult { .. }));
    match message.role {
        MessageRole::System => "system",
        MessageRole::User if tool_result => "tool result",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
    }
}

fn block_text(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text { text } | ContentBlock::Reasoning { text } => text.clone(),
        ContentBlock::Image { media_type, .. } => format!("[image {media_type}]"),
        ContentBlock::ToolUse { name, input, .. } => format!("→ {name} {input}"),
        ContentBlock::ToolResult { content, .. } => content
            .iter()
            .map(|ToolResultContent::Text { text }| text.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        ContentBlock::Compaction { .. } => "[compaction summary]".into(),
    }
}

/// Budget segments of a run's persisted history, by message role.
pub fn run_segments(rows: &[MessageRow]) -> Vec<BudgetSegment> {
    ["system", "user", "assistant", "tool result"]
        .into_iter()
        .map(|label| BudgetSegment {
            label,
            tokens: rows
                .iter()
                .filter(|row| row.role == label)
                .map(|row| row.tokens)
                .sum(),
        })
        .filter(|segment| segment.tokens > 0)
        .collect()
}

/// Header facts of a run view.
pub fn run_summary(view: &RunContextView) -> String {
    let category = view
        .category
        .as_deref()
        .map(|category| format!(" · {category}"))
        .unwrap_or_default();
    let model = view
        .selected_model
        .as_deref()
        .unwrap_or("model not recorded");
    format!(
        "{}{category} · {model} · {} messages · {} compaction checkpoints · saved at {}",
        view.role_name,
        view.messages.len(),
        view.compaction_checkpoint_count,
        view.checkpoint_phase
    )
}

/// Human-readable provenance of a section.
pub fn source_label(source: &ContextSource) -> String {
    match source {
        ContextSource::Preset { name } | ContextSource::BundledPreset { name } => {
            format!("bundled preset {name}")
        }
        ContextSource::UserPreset { name, path } => {
            format!("user preset {name} ({})", path.display())
        }
        ContextSource::Files { paths } => paths
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
        ContextSource::Generated => "generated by the runtime".into(),
    }
}

/// One aligned row of the role comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompareRow {
    pub label: String,
    pub left: Option<u64>,
    pub right: Option<u64>,
    /// Whether both sides carry byte-identical text; `None` when one side lacks it.
    pub identical: Option<bool>,
}

const COMPARE_KINDS: [ContextSectionKind; 10] = [
    ContextSectionKind::RoleBaseline,
    ContextSectionKind::ModelFamily,
    ContextSectionKind::CategoryOverlay,
    ContextSectionKind::IntentGate,
    ContextSectionKind::Appendix,
    ContextSectionKind::Skills,
    ContextSectionKind::Rules,
    ContextSectionKind::CompactionPolicy,
    ContextSectionKind::Workspace,
    ContextSectionKind::Memory,
];

/// Section-aligned comparison of two previews, then tools and the total.
pub fn compare_rows(left: &BaseContextReport, right: &BaseContextReport) -> Vec<CompareRow> {
    let text = |report: &BaseContextReport, kind| {
        report
            .sections
            .iter()
            .find(|section| section.kind == kind)
            .map(|section| (section.estimated_tokens, section.text.clone()))
    };
    let mut rows: Vec<_> = COMPARE_KINDS
        .into_iter()
        .filter_map(|kind| {
            let (a, b) = (text(left, kind), text(right, kind));
            (a.is_some() || b.is_some()).then(|| CompareRow {
                label: kind.label().into(),
                identical: a.as_ref().zip(b.as_ref()).map(|(a, b)| a.1 == b.1),
                left: a.map(|(tokens, _)| tokens),
                right: b.map(|(tokens, _)| tokens),
            })
        })
        .collect();
    let same_tools = left.tools.len() == right.tools.len()
        && left.tools.iter().zip(&right.tools).all(|(a, b)| {
            a.name == b.name && a.description == b.description && a.input_schema == b.input_schema
        });
    rows.push(CompareRow {
        label: format!("Tools ({} / {})", left.tools.len(), right.tools.len()),
        left: Some(left.tool_tokens()),
        right: Some(right.tool_tokens()),
        identical: Some(same_tools),
    });
    rows.push(CompareRow {
        label: "Total".into(),
        left: Some(left.total_tokens()),
        right: Some(right.total_tokens()),
        identical: None,
    });
    rows
}

/// Tool names only one side sees: `(left only, right only)`.
pub fn tool_difference(
    left: &BaseContextReport,
    right: &BaseContextReport,
) -> (Vec<String>, Vec<String>) {
    let only = |a: &BaseContextReport, b: &BaseContextReport| {
        a.tools
            .iter()
            .filter(|tool| !b.tools.iter().any(|other| other.name == tool.name))
            .map(|tool| tool.name.clone())
            .collect()
    };
    (only(left, right), only(right, left))
}

/// Compact token count: `950`, `12.4K`, `1.2M`.
pub fn compact_tokens(tokens: u64) -> String {
    match tokens {
        0..1_000 => tokens.to_string(),
        1_000..1_000_000 => format!("{:.1}K", tokens as f64 / 1_000.0),
        _ => format!("{:.1}M", tokens as f64 / 1_000_000.0),
    }
}

#[cfg(test)]
#[path = "context_inspector_tests.rs"]
mod tests;
