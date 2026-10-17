use crate::RunId;
use providers::RequestTurnContext;

#[derive(Clone, Copy, Default)]
pub(crate) struct RequestAncestry {
    pub(crate) is_subagent: bool,
    pub(crate) parent_subagent_id: Option<RunId>,
}

impl RequestAncestry {
    pub(crate) fn new(parent: Option<RunId>, parent_is_subagent: bool) -> Self {
        Self {
            is_subagent: parent.is_some(),
            parent_subagent_id: parent.filter(|_| parent_is_subagent),
        }
    }
    pub(crate) fn resolved(
        parent: Option<RunId>,
        live: Option<bool>,
        durable: Option<(RunId, bool)>,
    ) -> Self {
        Self::new(
            parent,
            live.or_else(|| {
                durable
                    .filter(|(id, _)| Some(*id) == parent)
                    .map(|(_, child)| child)
            })
            .unwrap_or(false),
        )
    }
}

pub(crate) fn internal(ancestry: RequestAncestry) -> RequestTurnContext {
    RequestTurnContext {
        parent_subagent_id: ancestry.parent_subagent_id.map(|parent| parent.to_string()),
        ..RequestTurnContext::unattributed(ancestry.is_subagent)
    }
}

pub(crate) fn initial(
    ancestry: RequestAncestry,
    human_input: bool,
    fresh_history: bool,
    resume: bool,
    saved: Option<&RequestTurnContext>,
) -> RequestTurnContext {
    if human_input {
        human(internal(ancestry).prompt_id, ancestry, fresh_history, saved)
    } else if resume && let Some(saved) = saved {
        let mut saved = saved.clone();
        saved.is_subagent = ancestry.is_subagent;
        saved.parent_subagent_id = ancestry.parent_subagent_id.map(|parent| parent.to_string());
        saved
    } else {
        internal(ancestry)
    }
}

pub(crate) fn human(
    prompt_id: String,
    ancestry: RequestAncestry,
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
        is_subagent: ancestry.is_subagent,
        parent_subagent_id: ancestry.parent_subagent_id.map(|parent| parent.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_human_counts_and_unknown_history_are_not_confused() {
        let first = initial(RequestAncestry::default(), true, true, false, None);
        assert_eq!(
            (
                first.origin.as_deref(),
                first.prompt_index,
                first.turn_index
            ),
            (Some("human"), Some(1), Some(1))
        );
        let next = initial(RequestAncestry::default(), true, false, false, Some(&first));
        assert_ne!(next.prompt_id, first.prompt_id);
        assert_eq!((next.prompt_index, next.turn_index), (Some(2), Some(2)));
        assert_eq!(
            initial(RequestAncestry::default(), false, false, true, Some(&next)),
            next
        );
        let reparented = initial(
            RequestAncestry::new(Some(RunId::new(2)), true),
            false,
            false,
            true,
            Some(&next),
        );
        assert_eq!(reparented.prompt_id, next.prompt_id);
        assert_eq!(reparented.origin, next.origin);
        assert_eq!(
            (reparented.prompt_index, reparented.turn_index),
            (Some(2), Some(2))
        );
        assert!(reparented.is_subagent);
        assert_eq!(reparented.parent_subagent_id.as_deref(), Some("run-2"));
        assert_eq!(
            initial(
                RequestAncestry::default(),
                false,
                false,
                true,
                Some(&reparented)
            ),
            next
        );
        let human_child = initial(
            RequestAncestry::new(Some(RunId::new(3)), true),
            true,
            false,
            false,
            Some(&reparented),
        );
        assert_eq!(human_child.parent_subagent_id.as_deref(), Some("run-3"));
        assert_eq!(
            (human_child.prompt_index, human_child.turn_index),
            (Some(3), Some(3))
        );
        let unknown = initial(
            RequestAncestry::new(Some(RunId::new(1)), false),
            false,
            false,
            false,
            Some(&next),
        );
        assert!(unknown.is_subagent);
        assert_eq!(unknown.parent_subagent_id, None);
        let parent = Some(RunId::new(1));
        assert_eq!(
            RequestAncestry::resolved(parent, Some(false), Some((RunId::new(1), true)))
                .parent_subagent_id,
            None
        );
        assert_eq!(
            RequestAncestry::resolved(parent, None, Some((RunId::new(2), true))).parent_subagent_id,
            None
        );
        assert_eq!(
            RequestAncestry::resolved(parent, None, Some((RunId::new(1), true))).parent_subagent_id,
            parent
        );
        assert_eq!(
            (unknown.origin, unknown.prompt_index, unknown.turn_index),
            (None, None, None)
        );
        let later_human = initial(RequestAncestry::default(), true, false, false, None);
        assert_eq!(later_human.origin.as_deref(), Some("human"));
        assert_eq!(
            (later_human.prompt_index, later_human.turn_index),
            (None, None)
        );
    }
}
