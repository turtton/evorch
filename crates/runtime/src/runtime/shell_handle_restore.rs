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
    // Scan the entire restored provider prefix, including tool inputs, result
    // text, compacted summaries and inherited history. Restricting this to a
    // start result misses handles mentioned by a parent or before compaction.
    let value = serde_json::to_value(messages).map_err(|error| error.to_string())?;
    let pattern = regex::Regex::new(r"\bjob-([0-9]+)\b").expect("constant handle regex");
    fn visit(
        value: &serde_json::Value,
        pattern: &regex::Regex,
        largest: &mut Option<u64>,
    ) -> Result<(), String> {
        match value {
            serde_json::Value::String(text) => {
                for captures in pattern.captures_iter(text) {
                    let number = captures[1]
                        .parse::<u64>()
                        .map_err(|_| "restored shell handle exceeds u64".to_owned())?;
                    let next = number
                        .checked_add(1)
                        .ok_or_else(|| "restored shell handle capacity exhausted".to_owned())?;
                    *largest = Some(largest.map_or(next, |previous| previous.max(next)));
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    visit(value, pattern, largest)?;
                }
            }
            serde_json::Value::Object(values) => {
                for value in values.values() {
                    visit(value, pattern, largest)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    let mut largest = None;
    visit(&value, &pattern, &mut largest)?;
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
            messages: history("raw job-1"),
            checkpoints: vec![crate::CompactionCheckpoint {
                id: "checkpoint".into(),
                summary: history("summary job-999").remove(0),
                range: (0, 1),
            }],
            trigger: None,
            turn_completed: true,
        };
        assert_eq!(restored_floor(&restored).unwrap(), Some(1000));
    }

    #[test]
    fn old_history_reserves_the_entire_numeric_handle_without_rewriting_it() {
        let messages = history("job-9, job-100,\njob-18446744073709551614");
        let before = serde_json::to_string(&messages).unwrap();
        assert_eq!(history_floor(&messages).unwrap(), Some(u64::MAX));
        assert_eq!(serde_json::to_string(&messages).unwrap(), before);
        assert!(history_floor(&history("job-18446744073709551615")).is_err());
        assert!(history_floor(&history("job-184467440737095516160")).is_err());
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
