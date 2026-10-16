use providers::RequestTurnContext;

pub(crate) fn initial(
    is_subagent: bool,
    human_input: bool,
    fresh_history: bool,
    resume: bool,
    saved: Option<&RequestTurnContext>,
) -> RequestTurnContext {
    if human_input {
        human(
            RequestTurnContext::unattributed(is_subagent).prompt_id,
            is_subagent,
            fresh_history,
            saved,
        )
    } else if resume && let Some(saved) = saved {
        let mut saved = saved.clone();
        saved.is_subagent = is_subagent;
        saved
    } else {
        RequestTurnContext::unattributed(is_subagent)
    }
}

pub(crate) fn human(
    prompt_id: String,
    is_subagent: bool,
    fresh_history: bool,
    previous: Option<&RequestTurnContext>,
) -> RequestTurnContext {
    RequestTurnContext {
        prompt_id,
        origin: Some("human".into()),
        prompt_index: previous
            .and_then(|turn| turn.prompt_index.and_then(|index| index.checked_add(1)))
            .or(fresh_history.then_some(1)),
        turn_index: previous
            .and_then(|turn| turn.turn_index.and_then(|index| index.checked_add(1)))
            .or(fresh_history.then_some(1)),
        is_subagent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_human_counts_and_unknown_history_are_not_confused() {
        let first = initial(false, true, true, false, None);
        assert_eq!(
            (
                first.origin.as_deref(),
                first.prompt_index,
                first.turn_index
            ),
            (Some("human"), Some(1), Some(1))
        );
        let next = initial(false, true, false, false, Some(&first));
        assert_ne!(next.prompt_id, first.prompt_id);
        assert_eq!((next.prompt_index, next.turn_index), (Some(2), Some(2)));
        assert_eq!(initial(false, false, false, true, Some(&next)), next);
        let unknown = initial(true, false, false, false, Some(&next));
        assert!(unknown.is_subagent);
        assert_eq!(
            (unknown.origin, unknown.prompt_index, unknown.turn_index),
            (None, None, None)
        );
        let later_human = initial(false, true, false, false, None);
        assert_eq!(later_human.origin.as_deref(), Some("human"));
        assert_eq!(
            (later_human.prompt_index, later_human.turn_index),
            (None, None)
        );
    }
}
