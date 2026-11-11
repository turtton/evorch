//! Execution namespace adapter: stopping runs, goals and execution settings.
use workbench_protocol::commands::CommandReply;
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::namespaces::ExecutionCommand;

use crate::commands::HostCommand;
use crate::ingress::CommandHost;

impl CommandHost {
    pub(in crate::ingress) fn execution_command(
        &mut self,
        command: ExecutionCommand,
        provenance: &Provenance,
    ) -> Result<CommandReply, WorkbenchError> {
        let host_command = match command {
            ExecutionCommand::StopChat(target) => HostCommand::StopChat {
                thread_id: target.thread.as_str().to_owned(),
            },
            ExecutionCommand::StopRun(stop) => HostCommand::StopRun {
                thread_id: stop.thread.as_str().to_owned(),
                run_id: stop.run.as_str().to_owned(),
            },
            ExecutionCommand::SubmitGoal(goal) => HostCommand::SubmitGoal(self.goal_input(&goal)?),
            ExecutionCommand::SetGoalReview(set) => HostCommand::SetGoalReview {
                thread_id: set.thread.as_str().to_owned(),
                goal_id: set.goal.as_str().to_owned(),
                enabled: set.enabled,
            },
            ExecutionCommand::SetGoalChecksPaused(set) => HostCommand::SetGoalChecksPaused {
                thread_id: set.thread.as_str().to_owned(),
                goal_id: set.goal.as_str().to_owned(),
                paused: set.paused,
            },
            ExecutionCommand::SetAgentWebToolsEnabled(set) => HostCommand::SetWebToolsEnabled {
                enabled: set.enabled,
            },
        };
        self.run_host_command(host_command, provenance)
    }
}
