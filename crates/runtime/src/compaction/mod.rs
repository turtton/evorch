pub(crate) mod cut;
pub(crate) mod estimator;
pub(crate) mod policy;
pub(crate) mod summary;

use event_bus::{CompactionEvent, CompactionReason, Event};
use providers::{ContentBlock, Message, Role};

use crate::agent_loop::LoopState;
use crate::context::CompactionCheckpoint;

use self::cut::select_cut;
use self::estimator::{estimate_tokens, estimate_visible};
use self::policy::{SummarizerKindSel, TriggerDecision, resolve_window, should_trigger};
use self::summary::{
    ModelSummarizer, StructuralSummarizer, Summarizer, SummaryInput, enforce_max_bytes,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompactionOutcome {
    pub(crate) estimated_tokens_before: u64,
    pub(crate) estimated_tokens_after: u64,
    pub(crate) compacted_range: (usize, usize),
    pub(crate) checkpoint_id: String,
    pub(crate) summary: String,
    pub(crate) still_above_threshold: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum CompactionError {
    #[error("compaction has no safe message range to replace")]
    NothingToCompact,
    #[error("compaction is blocked by the cooldown or current turn boundary")]
    Cooldown,
    #[error("compaction is already in flight")]
    InFlight,
    #[error("automatic compaction is disabled")]
    Disabled,
    #[error("context usage is below the automatic compaction threshold")]
    TooSmall,
    #[error("compaction summary failed: {0}")]
    SummarizeFailed(String),
}

pub(crate) async fn compact_now(
    state: &mut LoopState,
    reason: CompactionReason,
) -> Result<CompactionOutcome, CompactionError> {
    if state.compaction.in_flight {
        return Err(CompactionError::InFlight);
    }

    let visible = state.context.visible_messages();
    let estimated_before = estimate_visible(&visible, state.last_usage.as_ref());
    let settings = state.shared.compaction.clone();
    let window = resolve_window(
        &settings,
        &state.shared.model.selected_model(state.run_role()),
    );
    if reason == CompactionReason::Automatic {
        match should_trigger(&state.compaction, &settings, estimated_before, window) {
            TriggerDecision::Trigger => {}
            TriggerDecision::BelowThreshold => return Err(CompactionError::TooSmall),
            TriggerDecision::Disabled => return Err(CompactionError::Disabled),
            TriggerDecision::InFlight => return Err(CompactionError::InFlight),
            TriggerDecision::Cooldown | TriggerDecision::AlreadyThisBoundary => {
                return Err(CompactionError::Cooldown);
            }
        }
    }

    let protected_prefix = usize::from(
        state
            .context
            .messages
            .first()
            .is_some_and(|message| message.role == Role::System),
    );
    let plan = select_cut(
        &state.context.messages,
        settings.keep_recent_tokens,
        protected_prefix,
    )
    .ok_or(CompactionError::NothingToCompact)?;
    let compacted = &state.context.messages[plan.start..plan.end];
    let goal = first_user_text(compacted);

    state.compaction.in_flight = true;
    let summary_result = match settings.summarizer {
        SummarizerKindSel::Model => {
            ModelSummarizer {
                model: state.shared.model.clone(),
                role: state.run_role(),
                run_id: state.caller_run_id().to_string(),
            }
            .summarize(&SummaryInput { goal, compacted })
            .await
        }
        SummarizerKindSel::Structural => {
            StructuralSummarizer
                .summarize(&SummaryInput { goal, compacted })
                .await
        }
    };
    state.compaction.in_flight = false;
    let summary = enforce_max_bytes(
        &summary_result.map_err(|error| CompactionError::SummarizeFailed(error.to_string()))?,
        settings.max_summary_bytes,
    );

    let checkpoint_id = format!(
        "ckpt-{}-{}",
        state.caller_run_id(),
        state.compaction.checkpoint_seq
    );
    let summary_message = Message {
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: format!("[COMPACTION CHECKPOINT {checkpoint_id}]\n{summary}"),
        }],
    };
    let estimated_after = estimate_checkpoint(
        &state.context.messages,
        plan.start,
        plan.end,
        &summary_message,
    );
    let still_above_threshold = estimated_after as f64 / window as f64 >= settings.threshold;
    let outcome = CompactionOutcome {
        estimated_tokens_before: estimated_before,
        estimated_tokens_after: estimated_after,
        compacted_range: (plan.start, plan.end),
        checkpoint_id: checkpoint_id.clone(),
        summary: summary.clone(),
        still_above_threshold,
    };

    state.context.apply_checkpoint(CompactionCheckpoint {
        id: checkpoint_id.clone(),
        summary: summary_message,
        range: (plan.start, plan.end),
    });
    state.compaction.checkpoint_seq = state.compaction.checkpoint_seq.saturating_add(1);
    state.compaction.last_compaction_turn = Some(state.compaction.turn_counter);
    state.compaction.compacted_this_boundary = true;
    state.compaction.last_estimated_tokens = estimated_after;
    state
        .shared
        .bus
        .emit(Event::new(CompactionEvent::Compacted {
            run_id: state.caller_run_id().to_string(),
            reason,
            threshold: settings.threshold,
            context_window_tokens: window,
            estimated_tokens_before: estimated_before,
            estimated_tokens_after: estimated_after,
            compacted_range_start: plan.start,
            compacted_range_end: plan.end,
            checkpoint_id,
            summary,
        }));

    Ok(outcome)
}

fn first_user_text(messages: &[Message]) -> Option<&str> {
    messages
        .iter()
        .filter(|message| message.role == Role::User)
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            ContentBlock::Reasoning { .. }
            | ContentBlock::ToolUse { .. }
            | ContentBlock::ToolResult { .. } => None,
        })
}

fn estimate_checkpoint(messages: &[Message], start: usize, end: usize, summary: &Message) -> u64 {
    let mut projected = Vec::with_capacity(messages.len().saturating_sub(end - start) + 1);
    projected.extend_from_slice(&messages[..start]);
    projected.push(summary.clone());
    projected.extend_from_slice(&messages[end..]);
    estimate_tokens(&projected)
}
