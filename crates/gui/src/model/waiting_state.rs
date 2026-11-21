//! Why a run is Waiting, as its latest `Waiting` event recorded it. The value is the
//! event's own cause and time; nothing is recomputed from live state, and a later
//! state change or registration replaces it.

use std::time::SystemTime;

use event_bus::{WaitingCause, WaitingMode, WaitingRelation};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitingState {
    /// `None` for a `Waiting` event recorded before causes existed.
    pub cause: Option<WaitingCause>,
    /// `meta.wall_clock` of the event that entered Waiting.
    pub since: SystemTime,
}

impl WaitingState {
    /// Short wording shown next to the Waiting phase.
    pub fn label(&self) -> &'static str {
        match &self.cause {
            Some(WaitingCause::WaitTool { .. }) => "wait tool",
            Some(WaitingCause::Delegates { .. }) => "delegate result",
            Some(WaitingCause::BlockingQuestions) => "required question",
            Some(WaitingCause::TurnCompleted) => "turn complete, awaiting input",
            Some(WaitingCause::InputBoundary) => "awaiting input",
            None => "reason not recorded",
        }
    }

    /// Everything the event recorded, for a tooltip or the diagnostics window.
    pub fn detail(&self) -> String {
        let mut lines = vec![format!(
            "Waiting since {}",
            crate::format::utc_timestamp(self.since.into())
        )];
        match &self.cause {
            None => lines.push("Recorded before waiting reasons existed.".into()),
            Some(WaitingCause::WaitTool {
                call_id,
                explicit,
                mode,
                runs,
            }) => {
                lines.push(format!(
                    "wait tool{}: {} targets, mode {}",
                    call_id
                        .as_deref()
                        .map(|id| format!(" call {id}"))
                        .unwrap_or_default(),
                    if *explicit { "explicit" } else { "automatic" },
                    match mode {
                        WaitingMode::Any => "any",
                        WaitingMode::All => "all",
                    }
                ));
                lines
                    .push("Runs as seen when the wait started (not updated while waiting):".into());
                if runs.is_empty() {
                    lines.push("  none were visible then".into());
                }
                for run in runs {
                    lines.push(format!(
                        "  {} ({})",
                        run.run_id,
                        match run.relation {
                            WaitingRelation::Parent => "parent",
                            WaitingRelation::Child => "child",
                        }
                    ));
                }
            }
            Some(WaitingCause::Delegates { delegates }) => {
                lines.push("Waiting for delegated runs to finish:".into());
                for delegate in delegates {
                    lines.push(format!("  {} → {}", delegate.call_id, delegate.run_id));
                }
            }
            Some(WaitingCause::BlockingQuestions) => {
                lines.push("Waiting for an answer to a required question.".into());
            }
            Some(WaitingCause::TurnCompleted) => {
                lines.push("The turn completed; waiting for the next input.".into());
            }
            Some(WaitingCause::InputBoundary) => {
                lines.push("Waiting for input at a boundary without a completed turn.".into());
            }
        }
        lines.join("\n")
    }
}
