//! Thread goal controls belong to the composer, independently of worker Stop.

use crate::model::commands::WorkbenchCommand;
pub use crate::panes::goal_widgets::GoalAction;

impl GoalAction {
    pub fn into_command(self) -> WorkbenchCommand {
        match self {
            Self::Review {
                thread_id,
                goal_id,
                enabled,
            } => WorkbenchCommand::SetGoalReview {
                thread_id,
                goal_id,
                enabled,
            },
            Self::ChecksPaused {
                thread_id,
                goal_id,
                paused,
            } => WorkbenchCommand::SetGoalChecksPaused {
                thread_id,
                goal_id,
                paused,
            },
        }
    }
}
