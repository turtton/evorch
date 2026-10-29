pub(crate) mod cut;
pub(crate) mod estimator;
pub(crate) mod policy;
pub(crate) mod summary;

#[cfg(test)]
mod window_tests;

use std::sync::atomic::Ordering;

use event_bus::event::diagnostic_codes;
use event_bus::{CompactionEvent, CompactionReason, DiagnosticEvent, DiagnosticSeverity, Event};
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
    #[error("cannot prepare durable context boundary: {0}")]
    PrepareFailed(String),
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
    state.shared.model.selected_model_for_run(
        &state.caller_run_id().to_string(),
        state.run_role(),
        state.task.config.category.as_deref(),
    )
}

pub(crate) async fn compact_now(
    state: &mut LoopState,
    reason: CompactionReason,
) -> Result<CompactionOutcome, CompactionError> {
    let mut candidate = state.context.clone();
    let previous_compaction = state.compaction.clone();
    let mut events = Vec::new();
    let mut cancel = state.channels.cancel_rx.clone();
    cancel.borrow_and_update();
    let result = tokio::select! {
        biased;
        _ = preparation_interrupted(&mut cancel) => Err(CompactionError::PrepareFailed("context preparation interrupted".into())),
        result = compact_candidate(state, &mut candidate, reason, None, &mut events) => result,
    };
    state.compaction.in_flight = false;
    state
        .channels
        .compaction_busy
        .store(false, Ordering::SeqCst);
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            if state.interrupted().is_some() || matches!(error, CompactionError::PrepareFailed(_)) {
                state.compaction = previous_compaction;
            }
            return Err(error);
        }
    };
    if let Err(error) = install_prepared_context(state, candidate, true) {
        state.compaction = previous_compaction;
        return Err(error);
    }
    committed_context(state, events);
    Ok(result)
}

pub(crate) async fn compact_candidate(
    state: &mut LoopState,
    context: &mut crate::AgentContext,
    reason: CompactionReason,
    selected: Option<&str>,
    events: &mut Vec<Event>,
) -> Result<CompactionOutcome, CompactionError> {
    state.activity(event_bus::RunActivity::Compaction);
    let settings = state.shared.compaction.clone();
    let visible = context.visible_messages();
    let estimated_before = if selected.is_some() {
        estimate_tokens(&visible).saturating_add(state.estimated_tool_tokens())
    } else {
        state.estimated_context_tokens(&visible)
    };
    let (window, window_source) = selected.map_or_else(
        || state.resolved_context_window(),
        |selected| {
            policy::resolve_window(
                &settings,
                selected,
                state.shared.model.catalog_context_window(selected),
            )
        },
    );
    let at_limit = estimated_before >= window;
    if let Some(decision) = guard_decision(&state.compaction, &settings) {
        // At the hard window boundary, or when a model switch is already a cache
        // boundary, cooldown and same-turn suppression are secondary to making the
        // next request fit. The attempt budget remains.
        if !((at_limit || reason == CompactionReason::ModelSwitch)
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

    // Only model-switch checkpoints replace raw history for future compaction.
    // Ordinary checkpoints keep their existing cut and summary input byte-for-byte.
    let indexed = context.compaction_view();
    let compaction_messages: Vec<_> = indexed.iter().map(|(_, message)| message.clone()).collect();
    let protected_prefix = usize::from(
        compaction_messages
            .first()
            .is_some_and(|message| message.role == Role::System),
    );
    let protected_checkpoint = context
        .latest_checkpoint()
        .filter(|checkpoint| checkpoint.protects_history);
    let summary_index = protected_checkpoint.and_then(|checkpoint| {
        indexed.iter().position(|(index, message)| {
            *index == checkpoint.range.1 - 1 && message == &checkpoint.summary
        })
    });
    let mut plan = select_cut(
        &compaction_messages,
        settings.keep_recent_tokens,
        protected_prefix,
    )
    .or_else(|| {
        // A large protected summary can itself exceed the next window even when
        // there is no complete later turn. Re-summarize that summary, never raw calls.
        let start = summary_index?;
        Some(cut::CutPlan {
            start,
            end: start + 1,
        })
    })
    .ok_or(CompactionError::NothingToCompact)?;
    if let Some(summary_index) = summary_index {
        if plan.end <= summary_index {
            plan = cut::CutPlan {
                start: summary_index,
                end: summary_index + 1,
            };
        }
        // Expanding the raw replacement must also carry its existing summary into
        // the new summarizer input, rather than silently dropping that evidence.
        plan.start = plan.start.min(summary_index);
    }
    let compacted = &compaction_messages[plan.start..plan.end];
    let mapped_start = indexed[plan.start].0;
    let mapped_end = indexed
        .get(plan.end)
        .map_or(context.messages.len(), |(index, _)| *index);
    plan.start = mapped_start;
    plan.end = mapped_end;
    let protects_history = context
        .latest_checkpoint()
        .is_some_and(|checkpoint| checkpoint.protects_history);
    if let Some(checkpoint) = context
        .latest_checkpoint()
        .filter(|checkpoint| checkpoint.protects_history)
    {
        plan.start = plan.start.min(checkpoint.range.0);
        plan.end = plan.end.max(checkpoint.range.1);
    }
    // cut の protected floor が先頭 User(goal) を保護するため、goal は raw 履歴全体から取得する。
    let goal = first_user_text(&context.messages);

    // 予算は要約「試行」で消費する (要約失敗のまま同一応答内で compact を
    // 連呼される増幅を stop するため、成功時のみの counting では不十分)。
    state.compaction.compaction_count = state.compaction.compaction_count.saturating_add(1);
    state.compaction.in_flight = true;
    state.channels.compaction_busy.store(true, Ordering::SeqCst);
    let model_preference = selected
        .and_then(|selected| selected.split_once("/"))
        .map(|(profile, model)| crate::ModelPreference {
            profile: profile.into(),
            model: Some(model.into()),
            reasoning_effort: None,
        })
        .or_else(|| state.channels.model_preference_rx.borrow().clone());
    let invocation = crate::AgentInvocationContext {
        attempts: None,
        turn: Some(state.turn.clone()),
        run_id: state.caller_run_id().to_string(),
        category: state.task.config.category.clone(),
        model_preference: model_preference.clone(),
        purpose: event_bus::RequestPurpose::Compaction,
    };
    let official = state
        .shared
        .model
        .compact_context(&invocation, state.run_role(), compacted, &state.tool_specs)
        .await;
    let official = match official {
        Ok(result) => result,
        Err(error) => {
            state.compaction.record_failure(&settings);
            tracing::warn!("公式 compaction に失敗したため Summarizer へフォールバックします");
            failure_diagnostic(
                state,
                reason,
                &format!("official compaction failed; falling back to the summarizer: {error}"),
            );
            None
        }
    };
    let (summary_result, summary_usage) = if let Some(result) = &official {
        (
            Ok("[provider-side API compaction]".to_owned()),
            Some(result.usage),
        )
    } else {
        match context.text_compaction_view() {
            Ok(view) => {
                let protected = view
                    .protected
                    .as_ref()
                    .filter(|(index, _)| *index >= plan.start && *index < plan.end)
                    .map(|(_, body)| body.as_str());
                let messages: Vec<_> = view
                    .indexed
                    .into_iter()
                    .filter(|(index, _)| *index >= plan.start && *index < plan.end)
                    .map(|(_, message)| message)
                    .collect();
                summarize_range(
                    state,
                    reason,
                    model_preference,
                    &SummaryInput {
                        goal,
                        compacted: &messages,
                    },
                    false,
                    protected,
                )
                .await
            }
            Err(reason) => (
                Err(summary::SummarizeError::Model(crate::RuntimeError::Model {
                    reason: reason.into(),
                })),
                None,
            ),
        }
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
        Ok(summary) if official.is_some() => summary,
        Ok(summary) => enforce_max_bytes(&summary, settings.max_summary_bytes),
        Err(error) => {
            state.compaction.record_failure(&settings);
            if matches!(error, summary::SummarizeError::ProtectedContent { .. }) {
                return Err(CompactionError::PrepareFailed(error.to_string()));
            }
            let error = CompactionError::SummarizeFailed(error.to_string());
            failure_diagnostic(state, reason, &error.to_string());
            return Err(error);
        }
    };
    // max_summary_bytes=0 などで要約本文が空になる設定を黙って成功扱いしない。
    if summary.is_empty() {
        state.compaction.record_failure(&settings);
        let error = CompactionError::SummarizeFailed(
            "summary became empty after max_summary_bytes enforcement".to_string(),
        );
        failure_diagnostic(state, reason, &error.to_string());
        return Err(error);
    }

    let checkpoint_id = format!(
        "ckpt-{}-{}",
        state.caller_run_id(),
        state.compaction.checkpoint_seq
    );
    let summary_message = Message {
        role: Role::User,
        content: vec![match official {
            Some(result) => ContentBlock::Compaction {
                encrypted_content: result.encrypted_content,
            },
            None => ContentBlock::Text {
                text: format!("[COMPACTION CHECKPOINT {checkpoint_id}]\n{summary}"),
            },
        }],
    };
    let mut checkpoint = CompactionCheckpoint {
        id: checkpoint_id.clone(),
        summary: summary_message,
        range: (plan.start, plan.end),
        protects_history,
    };
    attach_working_memory(state, &mut checkpoint.summary)?;
    let mut prepared = context.clone();
    prepared.apply_checkpoint(checkpoint);
    let estimated_after =
        estimate_tokens(&prepared.visible_messages()).saturating_add(state.estimated_tool_tokens());
    if protects_history && estimated_after >= window {
        return Err(CompactionError::PrepareFailed(
            "protected history does not fit the target context window".into(),
        ));
    }
    let still_above_threshold = estimated_after as f64 / window as f64 >= settings.threshold;
    let outcome = CompactionOutcome {
        estimated_tokens_before: estimated_before,
        estimated_tokens_after: estimated_after,
        compacted_range: (plan.start, plan.end),
        checkpoint_id: checkpoint_id.clone(),
        summary: summary.clone(),
        still_above_threshold,
    };

    *context = prepared;
    // A compact tool may still have an outstanding ToolUse. Append the latest
    // procedure only at the next complete request boundary, after all results.
    // Provider usage describes the pre-compaction input and cannot be reused.
    state.compaction.last_usage_estimated_tokens = None;
    state.compaction.consecutive_failures = 0;
    state.compaction.retry_after_turn = 0;
    state.compaction.checkpoint_seq = state.compaction.checkpoint_seq.saturating_add(1);
    state.compaction.last_compaction_turn = Some(state.compaction.turn_counter);
    state.compaction.compacted_this_boundary = true;
    // 圧縮成功後は自動トリガを一時停止する。閾値未満の境界観測で再武装される。
    state.compaction.auto_suspended = true;
    state.compaction.last_estimated_tokens = estimated_after;
    events.push(Event::new(CompactionEvent::Compacted {
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

/// Summarizes `input` with the role's chain, falling back to the structural summary.
///
/// Structural fallback preserves trusted protected text verbatim, even when it
/// is outside its ordinary recent-message heuristic. It fails rather than
/// silently truncating that body (ADR 0030: official → chain → structural).
async fn summarize_range(
    state: &LoopState,
    reason: CompactionReason,
    model_preference: Option<crate::ModelPreference>,
    input: &SummaryInput<'_>,
    text_tool_history: bool,
    protected: Option<&str>,
) -> (
    Result<String, summary::SummarizeError>,
    Option<providers::Usage>,
) {
    let settings = &state.shared.compaction;
    if settings.summarizer == SummarizerKindSel::Structural {
        return (
            structural_summary(input, protected, settings.max_summary_bytes).await,
            None,
        );
    }
    let (result, usage) = ModelSummarizer {
        turn: Some(state.turn.clone()),
        model: state.shared.model.clone(),
        role: state.run_role(),
        run_id: state.caller_run_id().to_string(),
        category: state.task.config.category.clone(),
        model_preference,
        idle_timeout: std::time::Duration::from_secs(settings.summary_idle_timeout_secs),
        timeout: std::time::Duration::from_secs(settings.summary_timeout_secs),
    }
    .summarize_with_usage_for_switch(input, text_tool_history)
    .await;
    match result {
        Ok(summary) => (Ok(summary), usage),
        Err(error) => {
            failure_diagnostic(
                state,
                reason,
                &format!("summary model failed; using the structural summary: {error}"),
            );
            (
                structural_summary(input, protected, settings.max_summary_bytes).await,
                usage,
            )
        }
    }
}

async fn structural_summary(
    input: &SummaryInput<'_>,
    protected: Option<&str>,
    max_bytes: u64,
) -> Result<String, summary::SummarizeError> {
    let Some(body) = protected else {
        return StructuralSummarizer.summarize(input).await;
    };
    if body.len() as u64 > max_bytes {
        return Err(summary::SummarizeError::ProtectedContent {
            required: body.len(),
            limit: max_bytes,
        });
    }
    let ordinary = StructuralSummarizer.summarize(input).await?;
    // Reserve the original body before spending any budget on lossy structural
    // sections. This is provenance from a checkpoint, not a string heuristic.
    let remaining = max_bytes.saturating_sub(body.len() as u64);
    if remaining == 0 {
        return Ok(body.to_owned());
    }
    Ok(format!(
        "{body}\n{}",
        enforce_max_bytes(&ordinary, remaining - 1)
    ))
}

/// Prepares the context before the run's requests move to `selected` (ADR 0030).
///
/// A requested re-summary replaces the previous editing interface's history, even
/// without a checkpoint. History above the target's window × threshold is compacted.
/// Both replace history at an explicit boundary; later requests preserve its prefix.
pub(crate) async fn prepare_model_switch(
    state: &mut LoopState,
    selected: &str,
    resummarize: bool,
) -> Result<(), CompactionError> {
    let mut candidate = state.context.clone();
    let preference = selected
        .split_once('/')
        .map(|(profile, model)| crate::ModelPreference {
            profile: profile.into(),
            model: Some(model.into()),
            reasoning_effort: None,
        });
    let invocation = crate::AgentInvocationContext {
        attempts: None,
        run_id: state.caller_run_id().to_string(),
        turn: Some(state.turn.clone()),
        category: state.task.config.category.clone(),
        model_preference: preference,
        purpose: event_bus::RequestPurpose::Agent,
    };
    let mut cancel = state.channels.cancel_rx.clone();
    cancel.borrow_and_update();
    let scope = tokio::select! {
        biased;
        _ = preparation_interrupted(&mut cancel) => return Err(CompactionError::PrepareFailed("context preparation interrupted".into())),
        scope = state.shared.model.resolve_replay_scope(&invocation, state.run_role(), &state.tool_specs) =>
            scope.map_err(|error| CompactionError::PrepareFailed(error.to_string()))?,
    };
    // A ModelSwitch is an explicit boundary even for providers without a scope.
    candidate.responses_replay = crate::ResponsesReplayState::reset(candidate.messages.len());
    candidate
        .responses_replay
        .prepare(scope, candidate.messages.len());
    prepare_candidate(state, selected, resummarize, candidate).await
}

pub(crate) async fn prepare_responses_scope(
    state: &mut LoopState,
    selected: &str,
    scope: Option<providers::ResolvedResponsesScope>,
) -> Result<(), CompactionError> {
    let mut candidate = state.context.clone();
    candidate
        .responses_replay
        .prepare(scope, candidate.messages.len());
    prepare_candidate(state, selected, false, candidate).await
}

async fn prepare_candidate(
    state: &mut LoopState,
    selected: &str,
    resummarize: bool,
    mut candidate: crate::AgentContext,
) -> Result<(), CompactionError> {
    let previous_compaction = state.compaction.clone();
    let original_checkpoints = candidate.checkpoints().len();
    let mut events = Vec::new();
    let mut cancel = state.channels.cancel_rx.clone();
    cancel.borrow_and_update();
    let preparation = async {
        if resummarize {
            resummarize_history(state, &mut candidate, &mut events).await?;
        }
        let settings = state.shared.compaction.clone();
        let (window, _) = policy::resolve_window(
            &settings,
            selected,
            state.shared.model.catalog_context_window(selected),
        );
        // Usage from the old scope must not inflate the candidate estimate.
        let estimated = estimate_tokens(&candidate.visible_messages())
            .saturating_add(state.estimated_tool_tokens());
        if estimated as f64 >= window as f64 * settings.threshold
            && let Err(error) = compact_candidate(
                state,
                &mut candidate,
                CompactionReason::ModelSwitch,
                Some(selected),
                &mut events,
            )
            .await
        {
            tracing::warn!(%error, selected, "compaction before the model switch could not proceed");
            if matches!(error, CompactionError::PrepareFailed(_)) {
                return Err(error);
            }
        }
        let replaced = candidate.checkpoints().len() != original_checkpoints;
        if !replaced {
            let mut boundary = Message {
                role: Role::User,
                content: Vec::new(),
            };
            attach_working_memory(state, &mut boundary)?;
            if !boundary.content.is_empty() {
                candidate.messages.push(boundary);
            }
        }
        // Include the final working-memory attachment in the same fit check as
        // the protected replacement; neither is allowed to bypass rollback.
        if candidate
            .checkpoints()
            .iter()
            .any(|checkpoint| checkpoint.protects_history)
            && estimate_tokens(&candidate.visible_messages())
                .saturating_add(state.estimated_tool_tokens())
                >= window
        {
            return Err(CompactionError::PrepareFailed(
                "protected history does not fit the target context window".into(),
            ));
        }
        install_prepared_context(state, candidate, replaced)?;
        committed_context(state, events);
        Ok(())
    };
    let result = tokio::select! {
        biased;
        _ = preparation_interrupted(&mut cancel) => Err(CompactionError::PrepareFailed("context preparation interrupted".into())),
        result = preparation => result,
    };
    state.compaction.in_flight = false;
    state
        .channels
        .compaction_busy
        .store(false, Ordering::SeqCst);
    if result.is_err() {
        state.compaction = previous_compaction;
    }
    result
}

/// Reuses the text re-summary boundary for both opaque checkpoints and editing
/// interface switches. Include the completed tool batch at the tail: the ordinary
/// compaction cut deliberately retains that batch, which would replay old edit calls.
async fn resummarize_history(
    state: &mut LoopState,
    context: &mut crate::AgentContext,
    events: &mut Vec<Event>,
) -> Result<(), CompactionError> {
    let messages = &context.messages;
    let mut start = usize::from(
        messages
            .first()
            .is_some_and(|message| message.role == Role::System),
    );
    if messages.get(start).is_some_and(|message| {
        message.role == Role::User
            && matches!(message.content.first(), Some(ContentBlock::Text { .. }))
            && message.content.iter().all(|block| {
                !matches!(
                    block,
                    ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. }
                )
            })
    }) {
        start += 1;
    }
    // Keep the newest plain user input verbatim, but never keep an old tool batch.
    let mut end = messages.len();
    if messages.last().is_some_and(|message| {
        message.role == Role::User
            && matches!(message.content.first(), Some(ContentBlock::Text { .. }))
            && message.content.iter().all(|block| {
                !matches!(
                    block,
                    ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. }
                )
            })
    }) {
        end = end.saturating_sub(1);
    }
    if let Some(latest) = context.latest_checkpoint() {
        start = start.min(latest.range.0);
        end = end.max(latest.range.1);
    }
    if start >= end {
        return Ok(());
    }
    state.activity(event_bus::RunActivity::Compaction);
    let settings = state.shared.compaction.clone();
    let estimated_before = state.estimated_context_tokens(&context.visible_messages());
    let (window, window_source) = state.resolved_context_window();
    let view = context
        .text_compaction_view()
        .map_err(|reason| CompactionError::PrepareFailed(reason.into()))?;
    let protected = view
        .protected
        .as_ref()
        .filter(|(index, _)| *index >= start && *index < end)
        .map(|(_, body)| body.as_str());
    let projected: Vec<_> = view
        .indexed
        .into_iter()
        .filter(|(i, _)| *i >= start && *i < end)
        .map(|(_, m)| m)
        .collect();
    state.compaction.in_flight = true;
    state.channels.compaction_busy.store(true, Ordering::SeqCst);
    let compacted = projected.as_slice();
    let goal = first_user_text(&context.messages);
    let model_preference = state.channels.model_preference_rx.borrow().clone();
    let (result, usage) = summarize_range(
        state,
        CompactionReason::ModelSwitch,
        model_preference,
        &SummaryInput { goal, compacted },
        true,
        protected,
    )
    .await;
    state.compaction.in_flight = false;
    state
        .channels
        .compaction_busy
        .store(false, Ordering::SeqCst);
    if let Some(usage) = usage {
        state.budget.usage(usage);
    }
    let result = result.map_err(|error| CompactionError::PrepareFailed(error.to_string()))?;
    let summary = enforce_max_bytes(&result, settings.max_summary_bytes);
    if summary.is_empty() {
        return Err(CompactionError::SummarizeFailed("empty re-summary".into()));
    }
    let checkpoint_id = format!(
        "ckpt-{}-{}",
        state.caller_run_id(),
        state.compaction.checkpoint_seq
    );
    let mut checkpoint = CompactionCheckpoint {
        id: checkpoint_id.clone(),
        summary: Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: format!("[COMPACTION CHECKPOINT {checkpoint_id}]\n{summary}"),
            }],
        },
        range: (start, end),
        protects_history: true,
    };
    attach_working_memory(state, &mut checkpoint.summary)?;
    let estimated_after = estimate_tokens(&checkpoint.project(&context.messages))
        .saturating_add(state.estimated_tool_tokens());
    let mut prepared = context.clone();
    prepared.apply_checkpoint(checkpoint);
    *context = prepared;
    // Provider usage describes the replaced history and cannot be reused.
    state.compaction.last_usage_estimated_tokens = None;
    state.compaction.checkpoint_seq = state.compaction.checkpoint_seq.saturating_add(1);
    state.compaction.last_estimated_tokens = estimated_after;
    events.push(Event::new(CompactionEvent::Compacted {
        run_id: state.caller_run_id().to_string(),
        reason: CompactionReason::ModelSwitch,
        threshold: settings.threshold,
        context_window_tokens: window,
        window_source,
        estimated_tokens_before: estimated_before,
        estimated_tokens_after: estimated_after,
        compacted_range_start: start,
        compacted_range_end: end,
        checkpoint_id,
        summary,
    }));
    Ok(())
}

async fn preparation_interrupted(cancel: &mut tokio::sync::watch::Receiver<crate::RunInterrupt>) {
    loop {
        if cancel.borrow_and_update().is_interrupted() {
            return;
        }
        if cancel.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

fn committed_context(state: &mut LoopState, events: Vec<Event>) {
    if !events.is_empty() {
        state.todo_context_pending = true;
        state.todo_context_deferred = false;
    }
    state.last_usage = None;
    state.compaction.last_usage_estimated_tokens = None;
    for event in events {
        state.shared.bus.emit(event);
    }
}

fn install_prepared_context(
    state: &mut LoopState,
    prepared: crate::AgentContext,
    replaced_history: bool,
) -> Result<(), CompactionError> {
    if state.interrupted().is_some() {
        return Err(CompactionError::PrepareFailed(
            "context preparation interrupted".into(),
        ));
    }
    let old_directories = state.told_directories.clone();
    let old_instructions = state.allowed_instructions.clone();
    let old_pending = state.boundary_pending;
    if replaced_history {
        state.told_directories.rewrite_pending = true;
        state.allowed_instructions.forget_history();
    }
    state.boundary_pending = true;
    if let Err(error) = crate::restore::persist_prepared_context(state, &prepared) {
        state.told_directories = old_directories;
        state.allowed_instructions = old_instructions;
        state.boundary_pending = old_pending;
        return Err(CompactionError::PrepareFailed(error.to_string()));
    }
    state.context = prepared;
    Ok(())
}

fn attach_working_memory(state: &LoopState, message: &mut Message) -> Result<(), CompactionError> {
    let note = match state.runtime() {
        Some(runtime) => state
            .continuity
            .latest_note(&runtime, state.caller_run_id())
            .map_err(|error| CompactionError::PrepareFailed(error.to_string()))?,
        None => state.continuity.inherited_note.clone(),
    };
    message.content.extend(
        state
            .continuity
            .blocks(note.as_ref())
            .map_err(|error| CompactionError::PrepareFailed(error.to_string()))?,
    );
    Ok(())
}

/// Guard skips (cooldown, below threshold, ...) are routine and not reported; only an
/// attempted compaction whose provider call or summary failed is.
fn failure_diagnostic(state: &LoopState, reason: CompactionReason, error: &str) {
    state.shared.bus.emit(Event::new(DiagnosticEvent {
        source: "compaction".into(),
        severity: DiagnosticSeverity::Warning,
        code: diagnostic_codes::COMPACTION_FAILED.into(),
        detail: format!(
            "{error}\nreason={reason:?} summarizer={:?}",
            state.shared.compaction.summarizer
        ),
        run_id: Some(state.task.run_id.to_string()),
        thread_id: None,
        call_id: None,
    }));
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
            ContentBlock::Compaction { .. } => {
                tracing::warn!("この処理では compaction block をスキップします");
                None
            }
            ContentBlock::Image { .. }
            | ContentBlock::Reasoning { .. }
            | ContentBlock::Anthropic { .. }
            | ContentBlock::ResponsesReasoning { .. }
            | ContentBlock::ToolUse { .. }
            | ContentBlock::ToolResult { .. } => None,
        })
}
