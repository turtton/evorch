use crate::{ArenaError, ArenaReport, ArenaSpec, EvalTrace, FailureAttribution};
use futures_util::StreamExt;
use providers::{
    ChatRequest, ContentBlock, FinishReason, Message, ProviderAuth, ProviderClient, Role,
    StreamEvent,
};
use std::time::Duration;

pub struct Runner<'a> {
    pub client: &'a dyn ProviderClient,
    pub auth: &'a ProviderAuth,
    pub storage: storage::StorageHandle,
}

pub async fn run(spec: &ArenaSpec, runner: &Runner<'_>) -> Result<ArenaReport, ArenaError> {
    spec.validate()?;
    // Persist the complete comparison manifest so a partial ledger cannot be promoted.
    let task_spec =
        serde_json::to_string(spec).map_err(|_| ArenaError::InvalidSpec("task serialization"))?;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(spec.timeout_ms);
    let count = u64::try_from(spec.configs.len())
        .map_err(|_| ArenaError::InvalidSpec("configuration count"))?;
    let candidate_budget = spec.total_token_budget / count;
    let candidate_timeout = Duration::from_millis(spec.timeout_ms / count);
    let mut traces = Vec::with_capacity(spec.configs.len());
    // Reserve prompt bytes plus chat framing before dispatch; unknown usage consumes the reservation.
    let reservation = u64::try_from(spec.task.prompt.len())
        .ok()
        .and_then(|n| n.checked_add(32))
        .and_then(|n| n.checked_add(spec.max_output_tokens))
        .ok_or(ArenaError::InvalidSpec("token reservation overflow"))?;
    for config in &spec.configs {
        let mut trace = EvalTrace {
            id: serde_json::to_string(&(&spec.id, &config.id))
                .map_err(|_| ArenaError::InvalidSpec("identity"))?,
            arena_id: spec.id.clone(),
            project: spec.project.clone(),
            task_id: spec.task.id.clone(),
            task_spec: task_spec.clone(),
            config_id: config.id.clone(),
            profile: config.profile.clone(),
            model: config.model.clone(),
            attribution: config.attribution,
            output: String::new(),
            input_tokens: 0,
            output_tokens: 0,
            elapsed_ms: 0,
            failure: None,
        };
        if tokio::time::Instant::now() >= deadline {
            trace.failure = Some(FailureAttribution::Timeout);
        } else if candidate_budget < reservation {
            trace.failure = Some(FailureAttribution::BudgetExceeded);
        } else {
            let start = tokio::time::Instant::now();
            let request = ChatRequest {
                model: config.model.clone(),
                messages: vec![Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: spec.task.prompt.clone(),
                    }],
                }],
                tools: Vec::new(),
                temperature: Some(0.0),
                max_tokens: Some(spec.max_output_tokens),
                observation: None,
            };
            let candidate_deadline = (start + candidate_timeout).min(deadline);
            let result =
                tokio::time::timeout_at(candidate_deadline, evaluate(runner, &request, &mut trace))
                    .await;
            trace.elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
            trace.failure = match result {
                Err(_) => Some(FailureAttribution::Timeout),
                Ok(Err(failure)) => Some(failure),
                Ok(Ok(())) => None,
            };
            let used = trace.input_tokens.checked_add(trace.output_tokens);
            if used.is_none_or(|n| n > candidate_budget)
                || trace.output_tokens > spec.max_output_tokens
            {
                trace.failure = Some(FailureAttribution::BudgetExceeded);
            }
            if trace.failure.is_none() && trace.output != spec.task.expected_output {
                trace.failure = Some(FailureAttribution::OutputMismatch);
            }
        }
        let handle = runner.storage.clone();
        let persisted = trace.clone();
        tokio::task::spawn_blocking(move || handle.append_eval_trace(&persisted)).await??;
        traces.push(trace);
    }
    ArenaReport::from_traces(traces)
}

async fn evaluate(
    runner: &Runner<'_>,
    request: &ChatRequest,
    trace: &mut EvalTrace,
) -> Result<(), FailureAttribution> {
    let mut stream = runner
        .client
        .stream(runner.auth, request)
        .await
        .map_err(|_| FailureAttribution::Provider)?;
    let mut bytes = 0u64;
    while let Some(event) = stream.next().await {
        match event.map_err(|_| FailureAttribution::Provider)? {
            StreamEvent::TextDelta { text } => {
                bytes = bytes.saturating_add(u64::try_from(text.len()).unwrap_or(u64::MAX));
                if bytes > 1_048_576 {
                    return Err(FailureAttribution::BudgetExceeded);
                }
                trace.output.push_str(&text);
            }
            StreamEvent::ReasoningDelta { text } => {
                bytes = bytes.saturating_add(u64::try_from(text.len()).unwrap_or(u64::MAX));
                if bytes > 1_048_576 {
                    return Err(FailureAttribution::BudgetExceeded);
                }
            }
            StreamEvent::ToolCallDelta { .. } => {
                return Err(FailureAttribution::IncompleteResponse);
            }
            StreamEvent::Completed { response } => {
                trace.input_tokens = response.usage.input_tokens;
                trace.output_tokens = response.usage.output_tokens;
                if trace.input_tokens == 0 || trace.output_tokens == 0 {
                    return Err(FailureAttribution::MissingUsage);
                }
                return match response.finish_reason {
                    FinishReason::Stop => Ok(()),
                    FinishReason::Length => Err(FailureAttribution::BudgetExceeded),
                    FinishReason::ToolUse
                    | FinishReason::ContentFilter
                    | FinishReason::Other(_) => Err(FailureAttribution::IncompleteResponse),
                };
            }
        }
    }
    Err(FailureAttribution::IncompleteResponse)
}
