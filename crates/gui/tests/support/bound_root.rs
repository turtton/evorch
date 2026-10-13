//! Binds a thread to a root run the way an escalation handoff does, then starts
//! the root. The user-facing `/goal` entry point tracks a thread goal in the
//! normal conversation instead.
use gui::model::commands::{CommandSink, GoalSubmission};
use gui::runtime_sink::{RuntimeCommandSink, render_entry_prompt};
use runtime::{AgentRuntime, Role, RunConfig, RunId};

pub fn start(
    runtime: &AgentRuntime,
    sink: &mut RuntimeCommandSink,
    submission: GoalSubmission,
    role: Role,
    config: RunConfig,
) -> RunId {
    let root = runtime.reserve_run_id();
    sink.bind_goal_context(
        &submission.thread_id,
        &submission.project_id,
        &root.to_string(),
    );
    runtime.spawn_reserved(root, None, role, render_entry_prompt(&submission), config);
    root
}
