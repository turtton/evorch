//! What the user saw of a replay or a recorded thread, in order: the input sent
//! to the root run, the root's assistant text, and its user-visible questions
//! with their answers.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Entry {
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    Question {
        id: String,
        title: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        options: Vec<String>,
    },
    Answer {
        question_id: String,
        title: String,
        text: String,
    },
}

/// What the user saw of a recorded thread: their turns, the assistant text of
/// the `chat_runs` and the questions shown to them, in time order.
pub fn recorded(
    turns: &[crate::source::RecordedTurn],
    events: &[event_bus::Event],
    chat_runs: &[String],
) -> Vec<Entry> {
    use event_bus::{EventKind, MessageEvent, ToolEvent};

    use crate::source::UserTurnKind;

    let mut entries: Vec<(i64, Entry)> = turns
        .iter()
        .map(|recorded| {
            let turn = &recorded.turn;
            let entry = match turn.kind {
                UserTurnKind::Answer => Entry::Answer {
                    question_id: recorded.question_id.clone().unwrap_or_default(),
                    title: turn.question.clone().unwrap_or_default(),
                    text: turn.text.clone(),
                },
                UserTurnKind::InitialPrompt | UserTurnKind::Message => Entry::User {
                    text: turn.text.clone(),
                },
            };
            (recorded.at_ns, entry)
        })
        .collect();
    let mut asked = std::collections::HashSet::new();
    let mut parts = event_bus::MessagePartAssembler::new();
    for event in events {
        let Some(event) = parts.accept(event) else {
            continue;
        };
        let entry = match &event.kind {
            EventKind::Message(MessageEvent::MessageCompleted { run_id, text })
                if chat_runs.contains(run_id) && !text.trim().is_empty() =>
            {
                Entry::Assistant { text: text.clone() }
            }
            EventKind::Tool(ToolEvent::UserQuestionUpdated { question })
                if question.is_user_visible()
                    && question.answer.is_none()
                    && chat_runs.contains(&question.root_run_id)
                    && asked.insert(question.id.clone()) =>
            {
                Entry::Question {
                    id: question.id.clone(),
                    title: question.title.clone(),
                    options: question.options.clone(),
                }
            }
            _ => continue,
        };
        let at_ns = storage::system_time_to_ns(event.meta.wall_clock).unwrap_or_default();
        entries.push((at_ns, entry));
    }
    entries.sort_by_key(|(at_ns, _)| *at_ns);
    entries.into_iter().map(|(_, entry)| entry).collect()
}
