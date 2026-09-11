use crate::{ArenaConfig, ArenaSpec, EvalStep, EvalTrace, FailureAttribution, Runner};
use providers::{ChatRequest, ContentBlock, Message, Role};

pub(crate) async fn execute(
    spec: &ArenaSpec,
    context: (&ArenaConfig, &Runner<'_>, u64),
    trace: &mut EvalTrace,
) -> Result<(), FailureAttribution> {
    let (config, runner, budget) = context;
    let mut messages = Vec::new();
    if let Some(prompt) = &config.variant.prompt {
        messages.push(Message {
            role: Role::System,
            content: vec![ContentBlock::Text {
                text: prompt.system.clone(),
            }],
        });
    }
    messages.push(Message {
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: spec.task.prompt.clone(),
        }],
    });
    for role in config.roles() {
        let bytes = messages
            .iter()
            .try_fold(0u64, |total, message| {
                message.content.iter().try_fold(
                    total.saturating_add(32),
                    |sum, block| match block {
                        ContentBlock::Text { text } => u64::try_from(text.len())
                            .ok()
                            .and_then(|n| sum.checked_add(n)),
                        _ => None,
                    },
                )
            })
            .ok_or(FailureAttribution::BudgetExceeded)?;
        let used = trace.input_tokens.saturating_add(trace.output_tokens);
        if bytes.saturating_add(spec.max_output_tokens) > budget.saturating_sub(used) {
            return Err(FailureAttribution::BudgetExceeded);
        }
        let request = ChatRequest {
            model: config.model_for(*role).into(),
            messages: messages.clone(),
            tools: Vec::new(),
            temperature: Some(0.0),
            max_tokens: Some(spec.max_output_tokens),
            observation: None,
        };
        let execution = trace
            .execution
            .as_mut()
            .ok_or(FailureAttribution::IncompleteResponse)?;
        execution.steps.push(EvalStep {
            role: *role,
            model: request.model.clone(),
            output: String::new(),
            input_tokens: 0,
            output_tokens: 0,
        });
        let step = execution
            .steps
            .last_mut()
            .ok_or(FailureAttribution::IncompleteResponse)?;
        let result = crate::runner::evaluate(runner, &request, step).await;
        trace.input_tokens = trace.input_tokens.saturating_add(step.input_tokens);
        trace.output_tokens = trace.output_tokens.saturating_add(step.output_tokens);
        trace.output.clone_from(&step.output);
        messages.push(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: step.output.clone(),
            }],
        });
        let exceeds_output = step.output_tokens > spec.max_output_tokens;
        result?;
        if exceeds_output || trace.input_tokens.saturating_add(trace.output_tokens) > budget {
            return Err(FailureAttribution::BudgetExceeded);
        }
    }
    Ok(())
}
