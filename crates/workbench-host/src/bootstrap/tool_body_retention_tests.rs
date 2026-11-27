//! Explicit lifecycle signals and between-snapshot SQL mutations; synthetic DB only.
use super::super::*;
use event_bus::{AgentRunPhase, Event, ToolEvent};
use providers::{ChatResponse, ContentBlock, FinishReason, Message};
use runtime::{AgentInvocationContext, AgentModel, Role, RuntimeError};
use workspace_state::{SidebarDocument, SidebarStore, ThreadRecord};
const NOW: i64 = 8_000_000_000_000_000_000;
#[derive(Default)]
struct Pause {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl AgentModel for Pause {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "fixture".into()
    }
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[providers::ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(ChatResponse {
            message: Message {
                role: providers::Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "done".into(),
                }],
            },
            usage: Default::default(),
            usage_reported: true,
            finish_reason: FinishReason::Stop,
        })
    }
}
struct Direct;
impl runtime::SandboxFactory for Direct {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}
#[tokio::test(flavor = "multi_thread")]
async fn tool_body_retention_running_waiting_and_snapshot_races_do_not_delete() {
    let dir = test_socket_dir::SplitDir::new();
    std::fs::write(dir.path().join("config.toml"), "[storage.thread_retention]\nauto_archive_after_days=0\narchived_delete_after_days=0\ntool_body_delete_after_days=7\n").unwrap();
    let store = SidebarStore::new(dir.path().join("sidebar.json"));
    let mut doc = SidebarDocument::default();
    doc.sidebar
        .add_project(workspace_state::ProjectId::new("p"), "p", dir.path())
        .unwrap();
    doc.sidebar.threads.push(ThreadRecord::new(
        workspace_state::ThreadId::new("t"),
        workspace_state::ProjectId::new("p"),
        "t",
    ));
    doc.sidebar.refresh_derived();
    store.write(&store.lock().unwrap(), &doc).unwrap();
    let pause = Arc::new(Pause::default());
    let mut core = BootstrappedHost::open_with_test_seams(
        BootstrapOptions {
            config_dir: dir.path().into(),
            project_root: dir.path().into(),
            ownership_root: dir.sockets().into(),
        },
        TestSeams {
            model: pause.clone(),
            sandbox: Arc::new(Direct),
        },
    )
    .await
    .unwrap();
    let mut events = core.runtime.event_bus().subscribe();
    use workbench_protocol::{commands::*, ids::*, namespaces::ConversationCommand};
    let input = SendChat {
        project: ProjectId::new("p").unwrap(),
        thread: ThreadId::new("t").unwrap(),
        submission_id: SubmissionId::new("s").unwrap(),
        composer_role: ComposerRole::Worker,
        text: "start".into(),
        skills: vec![],
        attachments: vec![],
        model_preference: None,
        fork_seed: None,
    };
    let run: runtime::RunId = {
        let mut h = core.host.lock().unwrap();
        h.reserve_new_chat_thread(&input).unwrap();
        h.handle_command(
            Command::Conversation(ConversationCommand::SendChat(input)),
            crate::provenance::local_desktop(),
        )
        .unwrap()
        .run_id
        .unwrap()
        .as_str()
        .parse()
        .unwrap()
    };
    pause.entered.notified().await;
    core.storage_writer()
        .append_event(
            None,
            &Event::new(ToolEvent::ToolCompleted {
                tool_name: "fixture".into(),
                call_id: "c".into(),
                is_error: false,
                output: Some("long".repeat(1024)),
                detail: None,
                run_id: Some(run.to_string()),
                parent_call_id: None,
            }),
        )
        .unwrap();
    let leases = core.writer_leases.clone();
    let authority = leases.authority().unwrap();
    let make = || {
        crate::thread_retention::AutoArchiveCoordinator::new(
            core.storage_config().clone(),
            dir.sockets().into(),
            storage::thread_retention::AutoArchivePolicy { after_days: 0 },
        )
        .with_tool_body_policy(7)
    };
    let mut c = make();
    let r = c.tick(&core.host, Some(&authority), NOW);
    assert!(r.holds.contains_key("ToolBodyLiveRun"), "{r:?}");
    assert_eq!(r.stopped_runs, 0);
    pause.release.notify_one();
    loop {
        let e = events.recv().await.unwrap();
        if matches!(e.kind,event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::TurnCompleted{run_id,..}) if run_id==run.to_string())
        {
            break;
        }
    }
    core.flush_retention_events_for_test().unwrap();
    assert_eq!(
        core.runtime.inspect_agent(run).unwrap().phase,
        AgentRunPhase::Waiting
    );
    let r = c.tick(&core.host, Some(&authority), NOW);
    assert_eq!(r.tool_bodies.as_ref().map_or(0, |p| p.changed), 0);
    assert_eq!(r.stopped_runs, 0);
    core.runtime
        .stop(run, runtime::StopScope::SelfOnly)
        .unwrap();
    core.runtime.wait(run).await.unwrap();
    core.runtime.wait_retention_task(run).await.unwrap();
    core.flush_retention_events_for_test().unwrap();
    let db = core.storage_config().db_path.clone();
    let pin_revision: Option<i64> = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT pin_revision FROM threads WHERE thread_id='t'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    // Each hook runs after the short host snapshot but before writer IMMEDIATE.
    // A wholly unrelated context update is deliberately not a global fence.
    let lane_insert = format!(
        "INSERT INTO lanes(lane_id,thread_id,origin_thread_id,origin_run_id,creation_key,request_hash,scope,lead_thread_id,policy,management_revision,management_projection_revision,brief_json,brief_revision,start_seed,root_run_id,generation,created_at_ns,updated_at_ns,last_activity_at_ns) VALUES('lane','t','lead','r0','c','h','lane','lead','attached',1,0,'{}',1,'{{}}','{run}',1,0,0,0)",
        serde_json::json!({"objective":"o","inputs":[],"deliverables":[],"acceptance_criteria":[],"constraints":[],"report_policy":"attached"})
    );
    for (mutation, cleanup) in [
        (lane_insert,"DELETE FROM lanes".into()),
        (
            "UPDATE threads SET pin_revision=coalesce(pin_revision,0)+1 WHERE thread_id='t'"
                .to_owned(),
            format!("UPDATE threads SET pin_revision={} WHERE thread_id='t'", pin_revision.map_or_else(|| "NULL".into(), |n| n.to_string())),
        ),
        (
            format!(
                "INSERT INTO user_questions(id,run_id,root_run_id,root_name,payload) VALUES('q','{run}','{run}','r','{{}}')"
            ),
            "DELETE FROM user_questions".into(),
        ),
        (
            format!(
                "INSERT INTO host_input_receipts(prompt_id,run_id,root_run_id,text,status,accepted_at_ns,status_at_ns) VALUES('late','{run}','{run}','new','delivered',{NOW},{NOW})"
            ),
            "DELETE FROM host_input_receipts WHERE prompt_id='late'".into(),
        ),
        (
            "UPDATE thread_start_states SET replayable=1,reason=NULL,repo_prefix='',head_commit='fixture',diff=X'' WHERE thread_id='t'; INSERT INTO eval_samples(thread_id,problem,focus_roles,created_at_ns,updated_at_ns) VALUES('t','p','[]',0,0)".into(),
            "DELETE FROM eval_samples".into(),
        ),
    ] {
        let mut c = crate::thread_retention::AutoArchiveCoordinator::new(
            core.storage_config().clone(),
            dir.sockets().into(),
            storage::thread_retention::AutoArchivePolicy { after_days: 0 },
        )
        .with_tool_body_policy(7);
        let path = db.clone();
        c.on_decision(move || {
            rusqlite::Connection::open(&path)
                .unwrap()
                .execute_batch(&mutation)
                .unwrap();
        });
        let r = c.tick(&core.host, Some(&authority), NOW);
        assert_eq!(r.tool_bodies.as_ref().map_or(0, |p| p.changed), 0, "{r:?}");
        assert!(
            r.tool_bodies.as_ref().is_some_and(|p| p.hold.is_some()),
            "writer recheck: {r:?}"
        );
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch(&cleanup)
            .unwrap();
    }
    let mut c = crate::thread_retention::AutoArchiveCoordinator::new(
        core.storage_config().clone(),
        dir.sockets().into(),
        storage::thread_retention::AutoArchivePolicy { after_days: 0 },
    )
    .with_tool_body_policy(7);
    let writer = core.storage_writer().clone();
    c.on_decision(move || {
        for sequence in 1..=8 {
            writer
                .upsert_run_context(&storage::RunContextRecord {
                    run_id: "unrelated-context".into(),
                    role: "worker".into(),
                    name: "other thread".into(),
                    parent_run_id: None,
                    config_json: "{}".into(),
                    messages_json: "[]".into(),
                    checkpoints_json: "[]".into(),
                    terminal_phase: "Checkpoint".into(),
                    restorable: true,
                    updated_at_ns: sequence,
                    host_inputs_json: "[]".into(),
                })
                .unwrap();
        }
    });
    let r = c.tick(&core.host, Some(&authority), NOW);
    assert_eq!(r.tool_bodies.unwrap().changed, 1);
    assert_eq!(r.stopped_runs, 0);
    assert_eq!(r.published, 0, "archive disabled");
    core.shutdown().await.unwrap();
}
