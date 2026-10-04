//! Legacy PR-supervisor scenarios register their pipeline directly. The user-facing
//! `/goal` entry point now tracks a generic objective in the normal conversation.
use gui::model::commands::GoalSubmission;
use gui::runtime_sink::{RuntimeCommandSink, STORAGE_SESSION_ID, render_entry_prompt};
use runtime::{AgentRuntime, GoalSpec, Role, RunConfig, RunId, SupervisorHandle};

pub fn start(
    runtime: &AgentRuntime,
    supervisor: &SupervisorHandle,
    sink: &mut RuntimeCommandSink,
    submission: GoalSubmission,
    role: Role,
    config: RunConfig,
) -> (String, RunId) {
    let root = runtime.reserve_run_id();
    let goal_id = supervisor.create_goal(
        GoalSpec {
            session_id: STORAGE_SESSION_ID.into(),
            project_id: submission.project_id.clone(),
            thread_id: submission.thread_id.clone(),
            goal: submission.goal.clone(),
            references: vec![],
            constraints: submission.constraints.clone(),
            repo: "turtton/evorch".into(),
            base_ref: "main".into(),
        },
        root,
    );
    sink.bind_goal_context(
        &submission.thread_id,
        &submission.project_id,
        &root.to_string(),
    );
    sink.bind_goal_id(&submission.thread_id, &goal_id);
    runtime.spawn_reserved(root, None, role, render_entry_prompt(&submission), config);
    (goal_id, root)
}
use gui::model::commands::CommandSink;
