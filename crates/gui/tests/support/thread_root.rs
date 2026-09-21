use gui::{app::WorkbenchState, model::tasks::AgentRunSource};

pub fn bind_root<S: AgentRunSource>(state: &mut WorkbenchState<S>, run: &str) {
    let thread = match &state.sidebar().active_thread {
        Some(thread) => thread.to_string(),
        None => {
            state
                .add_project(std::env::current_dir().expect("cwd"))
                .expect("project");
            state
                .create_thread("conversation")
                .expect("thread")
                .to_string()
        }
    };
    state.apply_events([event_bus::Event::new(
        event_bus::LifecycleEvent::AgentRunStarted {
            run_id: run.into(),
            parent_run_id: None,
            agent_name: format!("chat:{thread}"),
            role: "orchestrator".into(),
        },
    )]);
}
