//! Exercise the real delegate path: no child run IDs are prebound in the sidebar.
use std::sync::Arc;

use async_trait::async_trait;
use event_bus::{
    AgentRunPhase, Event, EventBus, EventKind, EventReceiver, LifecycleEvent, ToolEvent,
};
use gui::app::WorkbenchState;
use gui::headless::HeadlessWorkbench;
use gui::model::tasks::AgentRunSource;
use gui::model::transcript::{ToolStatus, TranscriptEntry};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec, Usage};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, Role, RunConfig, RunId, RuntimeError,
};
use tokio::sync::Semaphore;
use tools::ToolExecutor;
use workspace_ui::{PanelId, ProjectId, SidebarState, ThreadId, UiSettings};

struct EvaluatorModel {
    finish: Semaphore,
    root_finish: Semaphore,
}

fn response(content: Vec<ContentBlock>, finish_reason: FinishReason) -> ChatResponse {
    ChatResponse {
        usage_reported: true,
        message: Message {
            role: providers::Role::Assistant,
            content,
        },
        usage: Usage::default(),
        finish_reason,
    }
}

fn call(id: &str, name: &str, input: serde_json::Value) -> ContentBlock {
    ContentBlock::ToolUse {
        id: id.into(),
        name: name.into(),
        input,
    }
}

#[async_trait]
impl AgentModel for EvaluatorModel {
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let has_call = messages
            .iter()
            .filter(|message| message.role == providers::Role::Assistant)
            .flat_map(|message| &message.content)
            .any(|block| matches!(block, ContentBlock::ToolUse { .. }));
        let prompt = messages
            .iter()
            .find(|message| message.role == providers::Role::User)
            .and_then(|message| {
                message.content.iter().find_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
            })
            .expect("delegated prompt");
        let id = format!("call-{}-{has_call}", invocation.run_id);
        match (role, has_call) {
            (Role::Orchestrator, false) => Ok(response(
                vec![call(
                    &id,
                    "delegate",
                    serde_json::json!({
                        "target": {"role": "evaluator"}, "prompt": prompt, "name": "test-evaluator",
                        "background": prompt == "one"
                    }),
                )],
                FinishReason::ToolUse,
            )),
            (Role::Orchestrator, true) => {
                self.root_finish.acquire().await.unwrap().forget();
                Ok(response(
                    vec![call(
                        &id,
                        "finish",
                        serde_json::json!({"result": "root finished"}),
                    )],
                    FinishReason::ToolUse,
                ))
            }
            (Role::Evaluator, false) => Ok(response(
                vec![
                    ContentBlock::Text {
                        text: format!("Evaluator progress {prompt}"),
                    },
                    call(&id, "working_note_read", serde_json::json!({})),
                ],
                FinishReason::ToolUse,
            )),
            (Role::Evaluator, true) => {
                self.finish.acquire().await.unwrap().forget();
                Ok(response(
                    vec![ContentBlock::Text {
                        text: format!("Evaluator result {prompt}"),
                    }],
                    FinishReason::Stop,
                ))
            }
            _ => panic!("unexpected role: {role:?}"),
        }
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test-evaluator".into()
    }
}

fn state<S: AgentRunSource>(source: S, root: &std::path::Path) -> WorkbenchState<S> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "project", root)
        .unwrap();
    sidebar.select_project(&project).unwrap();
    for id in ["one", "two"] {
        sidebar
            .create_thread(ThreadId::new(id), project.clone(), id)
            .unwrap();
    }
    sidebar.switch_thread(&ThreadId::new("one")).unwrap();
    WorkbenchState::new(source, &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
}

async fn through(events: &mut EventReceiver, mut stop: impl FnMut(&Event) -> bool) -> Vec<Event> {
    let mut collected = Vec::new();
    loop {
        let event = events.recv().await.expect("lossless test event stream");
        let done = stop(&event);
        collected.push(event);
        if done {
            return collected;
        }
    }
}

fn project(
    h: &mut HeadlessWorkbench<AgentRuntime>,
    storage: &storage::Storage,
    events: Vec<Event>,
) {
    for event in &events {
        storage.handle().append_event(Some("gui"), event).unwrap();
    }
    h.state_mut().apply_events(events);
}

fn observer(
    root: &std::path::Path,
    config: &storage::StorageConfig,
    stream: &str,
) -> HeadlessWorkbench<gui::fixture::DemoSource> {
    let mut s = state(gui::fixture::DemoSource(Vec::new()), root);
    s.restore_history(&storage::Database::open(config).unwrap())
        .unwrap();
    let s = s
        .with_manual_shared_feed(config.db_path.clone(), stream.into())
        .unwrap();
    HeadlessWorkbench::new(s, [1400.0, 1000.0])
}

fn pane(run: RunId) -> PanelId {
    PanelId::new(format!("agent-{run}"))
}

#[track_caller]
fn assert_content<S: AgentRunSource + 'static>(
    h: &mut HeadlessWorkbench<S>,
    child: RunId,
    prompt: &str,
) {
    let run = child.to_string();
    assert!(
        h.state().dock().find_tab(&pane(child)).is_some(),
        "event projection must dock the pane before explicit navigation"
    );
    h.state_mut().open_agent_pane(&run);
    let path = h
        .state()
        .dock()
        .find_tab(&pane(child))
        .expect("Evaluator pane is docked");
    assert_eq!(
        h.state().dock().leaf(path.node_path()).unwrap().active,
        path.tab
    );
    let entries = h
        .state()
        .transcripts()
        .run(&run)
        .expect("Evaluator transcript")
        .entries();
    assert!(entries.iter().any(
        |entry| matches!(entry, TranscriptEntry::Message { text, run_id: Some(id) }
        if id == &run && text == &format!("Evaluator progress {prompt}"))
    ));
    assert!(entries.iter().any(|entry| matches!(entry, TranscriptEntry::Tool { tool_name, run_id: Some(id), status: ToolStatus::Succeeded, is_error: false, .. }
        if id == &run && tool_name == "working_note_read")));
    h.run();
    assert!(
        h.has_label("✓ Read ledger"),
        "Evaluator tool call is readable in its pane"
    );
    assert!(
        h.has_label(&format!("Evaluator progress {prompt}")),
        "Evaluator text is readable in its pane"
    );
}

#[track_caller]
fn assert_parked<S: AgentRunSource + 'static>(h: &HeadlessWorkbench<S>, child: RunId) {
    let path = h
        .state()
        .dock()
        .find_tab(&pane(child))
        .expect("completed Evaluator remains docked");
    let leaf = h.state().dock().leaf(path.node_path()).unwrap();
    assert_ne!(
        leaf.active, path.tab,
        "completion parks without stealing focus"
    );
    assert_eq!(leaf.tabs[0], PanelId::new("subagents-home"));
}

#[test]
fn delegated_evaluator_is_readable_in_local_shared_and_restored_windows() {
    let temp = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: temp.path().join("events.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let bus = Arc::new(EventBus::new(1024));
    let mut events = bus.subscribe();
    let model = Arc::new(EvaluatorModel {
        finish: Semaphore::new(0),
        root_finish: Semaphore::new(0),
    });
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_sequential_run_ids()
        .with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
    let mut h = HeadlessWorkbench::new(state(runtime.clone(), temp.path()), [1400.0, 1000.0]);
    // Independent display state follows the same DB, with no prebound run IDs.
    let mut b = observer(temp.path(), &config, "observer");
    let mut children = Vec::new();
    for thread in ["one", "two"] {
        let root = {
            let _guard = rt.enter();
            runtime
                .delegate_chat(
                    thread,
                    Role::Orchestrator,
                    thread.into(),
                    RunConfig::default(),
                )
                .unwrap()
        };
        let started = rt.block_on(through(&mut events, |event| matches!(&event.kind,
            EventKind::Tool(ToolEvent::ToolCompleted { tool_name, .. }) if tool_name == "working_note_read")));
        let child: RunId = started
            .iter()
            .find_map(|event| match &event.kind {
                EventKind::Lifecycle(LifecycleEvent::AgentRunStarted {
                    run_id,
                    parent_run_id: Some(parent),
                    role,
                    ..
                }) if parent == &root.to_string() && role == "evaluator" => {
                    Some(run_id.parse().unwrap())
                }
                _ => None,
            })
            .expect("real Evaluator delegate emitted its parent link");
        project(&mut h, &storage, started);
        assert!(b.state_mut().poll_shared_feed() > 0);
        let owner = h
            .state()
            .sidebar()
            .threads
            .iter()
            .find(|owner| owner.id == ThreadId::new(thread))
            .unwrap();
        assert!(
            owner.run_ids.contains(&child.to_string()),
            "event projection binds the child to its owner"
        );
        if thread == "one" {
            assert_content(&mut h, child, thread);
            assert_content(&mut b, child, thread);
        } else {
            assert!(
                h.state().dock().find_tab(&pane(child)).is_none(),
                "inactive thread must not leak a pane"
            );
            assert!(b.state().dock().find_tab(&pane(child)).is_none());
        }
        // A restarted observer restores a running pane, then follows its
        // completion rather than relying on another cold replay to park it.
        let mut c = observer(temp.path(), &config, "restarted-observer");
        if thread == "one" {
            assert_content(&mut c, child, thread);
        } else {
            assert!(c.state().dock().find_tab(&pane(child)).is_none());
        }
        model.finish.add_permits(1);
        assert_eq!(
            rt.block_on(runtime.wait(child)).unwrap(),
            AgentRunPhase::Done
        );
        model.root_finish.add_permits(1);
        assert_eq!(
            rt.block_on(runtime.wait(root)).unwrap(),
            AgentRunPhase::Done
        );
        let finished = rt.block_on(through(&mut events, |event| matches!(&event.kind,
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to: AgentRunPhase::Done, .. }) if run_id == &root.to_string())));
        project(&mut h, &storage, finished);
        assert!(b.state_mut().poll_shared_feed() > 0);
        assert!(c.state_mut().poll_shared_feed() > 0);
        if thread == "one" {
            assert_parked(&b, child);
            assert_parked(&c, child);
            // Display-only lifecycle projection must also unpark a resumed ID.
            for to in [AgentRunPhase::Running, AgentRunPhase::Done] {
                let from = if to == AgentRunPhase::Running {
                    AgentRunPhase::Done
                } else {
                    AgentRunPhase::Running
                };
                project(
                    &mut h,
                    &storage,
                    vec![Event::new(LifecycleEvent::AgentRunStateChanged {
                        run_id: child.to_string(),
                        from,
                        to,
                        reason: None,
                    })],
                );
                assert!(b.state_mut().poll_shared_feed() > 0);
                assert!(c.state_mut().poll_shared_feed() > 0);
                for window in [&b, &c] {
                    if to == AgentRunPhase::Done {
                        assert_parked(window, child);
                    } else {
                        let path = window.state().dock().find_tab(&pane(child)).unwrap();
                        assert_eq!(
                            window.state().dock().leaf(path.node_path()).unwrap().active,
                            path.tab,
                            "resumed run gets its own active pane"
                        );
                    }
                }
            }
        } else {
            assert!(b.state().dock().find_tab(&pane(child)).is_none());
            assert!(c.state().dock().find_tab(&pane(child)).is_none());
        }
        children.push(child);
    }
    assert_parked(&h, children[0]);
    h.state_mut().switch_thread(ThreadId::new("two")).unwrap();
    assert!(h.state().dock().find_tab(&pane(children[0])).is_none());
    assert_parked(&h, children[1]);
    assert_content(&mut h, children[1], "two");
    h.state_mut().switch_thread(ThreadId::new("one")).unwrap();
    assert!(h.state().dock().find_tab(&pane(children[1])).is_none());
    assert_content(&mut h, children[0], "one");
    for index in [1, 0] {
        b.state_mut()
            .switch_thread(ThreadId::new(if index == 0 { "one" } else { "two" }))
            .unwrap();
        assert!(
            b.state()
                .dock()
                .find_tab(&pane(children[1 - index]))
                .is_none()
        );
        assert_parked(&b, children[index]);
        assert_content(
            &mut b,
            children[index],
            if index == 0 { "one" } else { "two" },
        );
    }

    // Cold restoration: no saved pane registry or prepopulated child run IDs.
    let mut restored = state(gui::fixture::DemoSource(Vec::new()), temp.path());
    restored
        .restore_history(&storage::Database::open(&config).unwrap())
        .unwrap();
    let mut h = HeadlessWorkbench::new(restored, [1400.0, 1000.0]);
    assert_parked(&h, children[0]);
    assert!(h.state().dock().find_tab(&pane(children[1])).is_none());
    assert_content(&mut h, children[0], "one");
    h.state_mut().switch_thread(ThreadId::new("two")).unwrap();
    assert!(h.state().dock().find_tab(&pane(children[0])).is_none());
    assert_parked(&h, children[1]);
    assert_content(&mut h, children[1], "two");
}

#[test]
fn history_restores_latest_child_phase_without_inventing_an_owner() {
    let temp = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: temp.path().join("events.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let mut events = vec![Event::new(LifecycleEvent::AgentRunStarted {
        run_id: "run-10".into(),
        parent_run_id: None,
        agent_name: "chat:Orchestrator:one".into(),
        role: "orchestrator".into(),
    })];
    for (id, phase) in [
        (20, AgentRunPhase::Done),
        (21, AgentRunPhase::Error),
        (22, AgentRunPhase::Stopped),
        (23, AgentRunPhase::Running),
        (24, AgentRunPhase::Waiting),
        (30, AgentRunPhase::Done),
    ] {
        let run_id = RunId::new(id).to_string();
        events.push(Event::new(LifecycleEvent::AgentRunStarted {
            run_id: run_id.clone(),
            parent_run_id: Some(if id == 30 { "run-999" } else { "run-10" }.into()),
            agent_name: format!("evaluator-{id}"),
            role: "evaluator".into(),
        }));
        // Every run reaches Running first; the latest phase, not an earlier
        // startup event or the presence of text, controls parking on restore.
        events.push(Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: run_id.clone(),
            from: AgentRunPhase::Pending,
            to: AgentRunPhase::Running,
            reason: None,
        }));
        events.push(Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id,
            from: AgentRunPhase::Running,
            to: phase,
            reason: None,
        }));
    }
    for event in &events {
        storage.handle().append_event(Some("gui"), event).unwrap();
    }
    let db = storage::Database::open(&config).unwrap();
    let mut restored = state(gui::fixture::DemoSource(Vec::new()), temp.path());
    restored.restore_history(&db).unwrap();
    // Replaying into a retained layout must not duplicate any pane.
    restored.restore_history(&db).unwrap();
    let h = HeadlessWorkbench::new(restored, [1400.0, 1000.0]);
    for id in [20, 21, 22] {
        assert_parked(&h, RunId::new(id));
    }
    for id in [23, 24] {
        let panel = pane(RunId::new(id));
        let path = h
            .state()
            .dock()
            .find_tab(&panel)
            .expect("unfinished child is docked");
        assert_eq!(
            h.state().dock().leaf(path.node_path()).unwrap().active,
            path.tab
        );
    }
    for id in 20..=24 {
        let panel = pane(RunId::new(id));
        assert_eq!(
            h.state()
                .dock()
                .iter_all_tabs()
                .filter(|(_, id)| **id == panel)
                .count(),
            1
        );
    }
    for id in [10, 30] {
        assert!(
            h.state().dock().find_tab(&pane(RunId::new(id))).is_none(),
            "neither conversation root nor ownerless Evaluator opens a child pane"
        );
    }
}
