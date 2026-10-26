//! Reserve old short handles before restored history can request a new job.
use providers::Message;

use super::*;

impl AgentRuntime {
    pub(super) fn reserve_restored_shell_handles(
        &self,
        run: RunId,
        restored: &crate::restore::RestoredState,
    ) -> Result<(), RuntimeError> {
        let fail = |detail| RuntimeError::RunRestoreFailed {
            run_id: run.to_string(),
            reason: crate::RunRestoreFailure::CorruptContext(detail),
        };
        if let Some(next) = restored_floor(restored).map_err(fail)? {
            self.shared
                .executor
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .reserve_shell_job_handles(next)
                .map_err(|error| fail(error.to_string()))?;
        }
        Ok(())
    }
}

fn restored_floor(restored: &crate::restore::RestoredState) -> Result<Option<u64>, String> {
    let raw = history_floor(&restored.messages)?;
    let summaries = history_floor(
        &restored
            .checkpoints
            .iter()
            .map(|checkpoint| checkpoint.summary.clone())
            .collect::<Vec<_>>(),
    )?;
    Ok(raw.max(summaries))
}

fn history_floor(messages: &[Message]) -> Result<Option<u64>, String> {
    // Scan the entire prefix, including parent references and compacted history.
    // These strings are reservation hints, not evidence of issued handles: code
    // and reviews may quote arbitrary numbers. The durable counter protects
    // already-issued high handles without trusting those strings.
    let value = serde_json::to_value(messages).map_err(|error| error.to_string())?;
    let pattern = regex::Regex::new(r"\bjob-([0-9]+)\b").expect("constant handle regex");
    fn visit(value: &serde_json::Value, pattern: &regex::Regex, largest: &mut Option<u64>) {
        match value {
            serde_json::Value::String(text) => {
                for captures in pattern.captures_iter(text) {
                    let Some(next) = captures[1]
                        .parse::<u64>()
                        .ok()
                        .and_then(|number| number.checked_add(1))
                        .filter(|next| *next <= tools::MAX_RESTORED_HANDLE_FLOOR)
                    else {
                        continue;
                    };
                    *largest = Some(largest.map_or(next, |previous| previous.max(next)));
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    visit(value, pattern, largest);
                }
            }
            serde_json::Value::Object(values) => {
                for value in values.values() {
                    visit(value, pattern, largest);
                }
            }
            _ => {}
        }
    }
    let mut largest = None;
    visit(&value, &pattern, &mut largest);
    Ok(largest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use providers::{ContentBlock, Role};

    fn history(text: &str) -> Vec<Message> {
        vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }]
    }

    #[test]
    fn compaction_summaries_outside_raw_messages_are_reserved_too() {
        let restored = crate::restore::RestoredState {
            responses_replay: Default::default(),
            admission_reservation: None,
            turn: None,
            boundary_pending: false,
            continuity: Default::default(),
            pending_reference_id: None,
            messages: history("raw job-1"),
            checkpoints: vec![crate::CompactionCheckpoint {
                id: "checkpoint".into(),
                summary: history("summary job-999; quoted job-18446744073709551614").remove(0),
                range: (0, 1),
                protects_history: false,
            }],
            trigger: None,
            turn_completed: true,
            allowed_directories: Default::default(),
            allowed_instructions: Default::default(),
            host_inputs: Vec::new(),
        };
        assert_eq!(restored_floor(&restored).unwrap(), Some(1000));
    }

    #[test]
    fn history_reserves_bounded_hints_without_rewriting_quoted_numbers() {
        for (text, expected) in [
            ("job-4294967294", Some(u64::from(u32::MAX))),
            ("job-4294967295", None),
            ("job-18446744073709551614", None),
            ("job-18446744073709551615", None),
            ("job-184467440737095516160", None),
        ] {
            let messages = history(text);
            let before = serde_json::to_string(&messages).unwrap();
            assert_eq!(history_floor(&messages).unwrap(), expected, "{text}");
            assert_eq!(serde_json::to_string(&messages).unwrap(), before);
            let mixed = history(&format!("job-9, {text}, job-100"));
            assert_eq!(history_floor(&mixed).unwrap(), expected.max(Some(101)));
        }
        assert_eq!(
            history_floor(&history("no shell job references")).unwrap(),
            None
        );
        assert_eq!(
            history_floor(&history("notjob-42 job-42suffix")).unwrap(),
            None
        );
    }
}
