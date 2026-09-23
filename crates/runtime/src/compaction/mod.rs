pub(crate) mod cut;
pub(crate) mod estimator;
pub(crate) mod policy;
pub(crate) mod summary;

#[cfg(test)]
mod window_tests;

use std::sync::atomic::Ordering;

use event_bus::{CompactionEvent, CompactionReason, Event};
use providers::{ContentBlock, Message, Role};

use crate::agent_loop::LoopState;
use crate::context::CompactionCheckpoint;

use self::cut::select_cut;
use self::estimator::estimate_tokens;
use self::policy::{
    GuardDecision, SummarizerKindSel, ThresholdDecision, guard_decision, threshold_decision,
};
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
    #[error("compaction cooldown is active")]
    Cooldown,
    #[error("compaction budget for this run is exhausted")]
    BudgetExhausted,
    #[error("compaction already completed at the current turn boundary")]
    AlreadyThisBoundary,
    #[error("compaction is already in flight")]
    InFlight,
    #[error("compaction is disabled")]
    Disabled,
    #[error("context usage is below the automatic compaction threshold")]
    TooSmall,
    #[error("compaction summary failed: {0}")]
    SummarizeFailed(String),
}

/// Resolve the identity of the next request, including an interactive override.
pub(crate) fn selected_model(state: &LoopState) -> String {
    let preference = state.channels.model_preference_rx.borrow().clone();
    if let Some(preference) = preference {
        let model = preference.model.or_else(|| {
            state
                .shared
                .model
                .available_profiles()
                .into_iter()
                .find(|profile| profile.name == preference.profile)
                .and_then(|profile| profile.default_model)
        });
        if let Some(model) = model {
            return format!("{}/{model}", preference.profile);
        }
    }
    state
        .shared
        .model
        .selected_model(state.run_role(), state.task.config.category.as_deref())
}

pub(crate) async fn compact_now(
    state: &mut LoopState,
    reason: CompactionReason,
) -> Result<CompactionOutcome, CompactionError> {
    state.activity(event_bus::RunActivity::Compaction);
    let settings = state.shared.compaction.clone();
    let visible = state.context.visible_messages();
    let estimated_before = state.estimated_context_tokens(&visible);
    let (window, window_source) = state.resolved_context_window();
    let at_limit = estimated_before >= window;
    if let Some(decision) = guard_decision(&state.compaction, &settings) {
        // At the hard window boundary, cooldown and same-turn suppression are
        // secondary to making the next request fit. The attempt budget remains.
        if !(at_limit
            && matches!(
                decision,
                GuardDecision::Cooldown | GuardDecision::AlreadyThisBoundary
            ))
        {
            return Err(error_from_guard(decision));
        }
    }
    if reason == CompactionReason::Automatic {
        // The post-compaction latch is only for proactive compression.
        if state.compaction.auto_suspended && !at_limit {
            return Err(CompactionError::TooSmall);
        }
        match threshold_decision(&settings, estimated_before, window) {
            ThresholdDecision::Trigger => {}
            ThresholdDecision::BelowThreshold => return Err(CompactionError::TooSmall),
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
    // cut の protected floor が先頭 User(goal) を保護するため、goal は raw 履歴全体から取得する。
    let goal = first_user_text(&state.context.messages);

    // 予算は要約「試行」で消費する (要約失敗のまま同一応答内で compact を
    // 連呼される増幅を stop するため、成功時のみの counting では不十分)。
    state.compaction.compaction_count = state.compaction.compaction_count.saturating_add(1);
    state.compaction.in_flight = true;
    state.channels.compaction_busy.store(true, Ordering::SeqCst);
    let model_preference = state.channels.model_preference_rx.borrow().clone();
    let (summary_result, summary_usage) = match settings.summarizer {
        SummarizerKindSel::Model => {
            ModelSummarizer {
                model: state.shared.model.clone(),
                role: state.run_role(),
                run_id: state.caller_run_id().to_string(),
                category: state.task.config.category.clone(),
                model_preference,
                idle_timeout: std::time::Duration::from_secs(settings.summary_idle_timeout_secs),
                timeout: std::time::Duration::from_secs(settings.summary_timeout_secs),
            }
            .summarize_with_usage(&SummaryInput { goal, compacted })
            .await
        }
        SummarizerKindSel::Structural => (
            StructuralSummarizer
                .summarize(&SummaryInput { goal, compacted })
                .await,
            None,
        ),
    };
    if let Some(usage) = summary_usage {
        state.budget.usage(usage);
    }
    state.compaction.in_flight = false;
    state
        .channels
        .compaction_busy
        .store(false, Ordering::SeqCst);
    let summary = match summary_result {
        Ok(summary) => enforce_max_bytes(&summary, settings.max_summary_bytes),
        Err(error) => {
            state.compaction.record_failure(&settings);
            return Err(CompactionError::SummarizeFailed(error.to_string()));
        }
    };
    // max_summary_bytes=0 などで要約本文が空になる設定を黙って成功扱いしない。
    if summary.is_empty() {
        state.compaction.record_failure(&settings);
        return Err(CompactionError::SummarizeFailed(
            "summary became empty after max_summary_bytes enforcement".to_string(),
        ));
    }

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
    )
    .saturating_add(state.estimated_tool_tokens());
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
    // Provider usage describes the pre-compaction input and cannot be reused.
    state.last_usage = None;
    state.compaction.last_usage_estimated_tokens = None;
    state.compaction.consecutive_failures = 0;
    state.compaction.retry_after_turn = 0;
    state.compaction.checkpoint_seq = state.compaction.checkpoint_seq.saturating_add(1);
    state.compaction.last_compaction_turn = Some(state.compaction.turn_counter);
    state.compaction.compacted_this_boundary = true;
    // 圧縮成功後は自動トリガを一時停止する。閾値未満の境界観測で再武装される。
    state.compaction.auto_suspended = true;
    state.compaction.last_estimated_tokens = estimated_after;
    state
        .shared
        .bus
        .emit(Event::new(CompactionEvent::Compacted {
            run_id: state.caller_run_id().to_string(),
            reason,
            threshold: settings.threshold,
            context_window_tokens: window,
            window_source,
            estimated_tokens_before: estimated_before,
            estimated_tokens_after: estimated_after,
            compacted_range_start: plan.start,
            compacted_range_end: plan.end,
            checkpoint_id,
            summary,
        }));

    Ok(outcome)
}

const fn error_from_guard(decision: GuardDecision) -> CompactionError {
    match decision {
        GuardDecision::Disabled => CompactionError::Disabled,
        GuardDecision::InFlight => CompactionError::InFlight,
        GuardDecision::AlreadyThisBoundary => CompactionError::AlreadyThisBoundary,
        GuardDecision::Cooldown => CompactionError::Cooldown,
        GuardDecision::BudgetExhausted => CompactionError::BudgetExhausted,
    }
}

fn first_user_text(messages: &[Message]) -> Option<&str> {
    messages
        .iter()
        .filter(|message| message.role == Role::User)
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            ContentBlock::Image { .. }
            | ContentBlock::Reasoning { .. }
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
