//! Real desktop actor -> Unix host and Web -> HTTP host, sharing a fake
//! provider. Every wait observes an explicit state, request, or terminal event.
use async_trait::async_trait;
use gui::core_client::{
    Shared,
    connection::{Action, serve},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use workbench_protocol::{
    commands::*, conversation::ConversationCommand, envelope::*, ids::*, queries::*,
};

struct Provider {
    requests: tokio::sync::mpsc::UnboundedSender<(
        Vec<providers::Message>,
        tokio::sync::oneshot::Sender<()>,
    )>,
}
#[async_trait]
impl runtime::AgentModel for Provider {
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
        messages: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.requests.send((messages.to_vec(), tx)).unwrap();
        let _ = rx.await;
        Ok(providers::ChatResponse {
            usage_reported: true,
            message: providers::Message {
                role: providers::Role::Assistant,
                content: vec![providers::ContentBlock::Text {
                    text: "confirmed reply".repeat(2000),
                }],
            },
            usage: Default::default(),
            finish_reason: providers::FinishReason::Stop,
        })
    }
    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "offline-provider".into()
    }
}

async fn changed(shared: &Arc<Mutex<Shared>>, condition: impl Fn(&Shared) -> bool) {
    let changed = shared.lock().unwrap().changed.clone();
    loop {
        let notification = changed.notified();
        tokio::pin!(notification);
        notification.as_mut().enable();
        if condition(&shared.lock().unwrap()) {
            return;
        }
        notification.await;
    }
}
fn send(id: &str, text: &str) -> SendChat {
    SendChat {
        project: ProjectId::new("p").unwrap(),
        thread: ThreadId::new("t").unwrap(),
        submission_id: SubmissionId::new(id).unwrap(),
        composer_role: ComposerRole::Worker,
        text: text.into(),
        skills: vec![],
        images: vec![],
        model_preference: None,
        fork_seed: None,
    }
}
async fn web(http: &reqwest::Client, root: &str, id: &str, body: RequestBody) -> ReplyPayload {
    let reply: ReplyEnvelope = http
        .post(format!("{root}/api/v1/request"))
        .header("Origin", root)
        .json(&RequestEnvelope {
            request_id: RequestId::new(id).unwrap(),
            session_id: SessionId::new("web").unwrap(),
            body,
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    match reply.result {
        ReplyResult::Success(reply) => reply,
        other => panic!("{other:?}"),
    }
}

/// Forward real length-prefixed frames, but lose the first execution's reply
/// only after the core has produced it (and therefore recorded its receipt).
async fn proxy(
    stream: tokio::net::UnixStream,
    backend: Arc<evorch_core::backend::HostBackend>,
    stopped: tokio::sync::watch::Receiver<bool>,
    lose_reply: bool,
    observed: Arc<Mutex<Vec<RequestEnvelope>>>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    async fn packet(read: &mut (impl tokio::io::AsyncRead + Unpin)) -> std::io::Result<Vec<u8>> {
        let len = read.read_u32().await?;
        assert!(len <= workbench_protocol::limits::DEFAULT_FRAME_BYTES);
        let mut bytes = vec![0; len as usize];
        read.read_exact(&mut bytes).await?;
        Ok(bytes)
    }
    let (upstream, core) = tokio::net::UnixStream::pair().unwrap();
    let core = tokio::spawn(evorch_core::unix::connection(core, backend, stopped));
    let target = Arc::new(Mutex::new(None::<RequestId>));
    let lost = {
        let (mut client_read, mut client_write) = stream.into_split();
        let (mut core_read, mut core_write) = upstream.into_split();
        let inbound = async {
            while let Ok(bytes) = packet(&mut client_read).await {
                if let Ok(request) = serde_json::from_slice::<RequestEnvelope>(&bytes) {
                    if matches!(
                        request.body,
                        RequestBody::Command(Command::Conversation(
                            ConversationCommand::CreateExecution(_)
                        ))
                    ) {
                        *target.lock().unwrap() = Some(request.request_id.clone());
                    }
                    observed.lock().unwrap().push(request);
                }
                core_write.write_u32(bytes.len() as u32).await?;
                core_write.write_all(&bytes).await?;
            }
            Ok::<_, std::io::Error>(false)
        };
        let outbound = async {
            while let Ok(bytes) = packet(&mut core_read).await {
                if lose_reply
                    && let Ok(ServerFrame::Reply(reply)) = serde_json::from_slice(&bytes)
                    && target.lock().unwrap().as_ref() == Some(&reply.request_id)
                {
                    return Ok::<_, std::io::Error>(true);
                }
                client_write.write_u32(bytes.len() as u32).await?;
                client_write.write_all(&bytes).await?;
            }
            Ok(false)
        };
        // EOF/reset is normal when the window closes. An early close before
        // the deliberately lost reply cannot satisfy this test's assertions.
        tokio::select! {
            result = inbound => result.unwrap_or(false),
            result = outbound => result.unwrap_or(false),
        }
    };
    assert_eq!(lost, lose_reply);
    // The connection was deliberately cut; only the independent core runtime
    // owns the run. Joining this socket handler must not stop that run.
    let _ = core.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn desktop_and_web_share_images_order_questions_approvals_and_runs_outlive_desktop() {
    use base64::Engine;
    let dir = tempfile::Builder::new()
        .prefix("p08-")
        .tempdir_in("/tmp")
        .unwrap();
    let bus = Arc::new(event_bus::EventBus::new(1024));
    let (requests, mut provider) = tokio::sync::mpsc::unbounded_channel();
    let config = storage::StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let store = storage::Storage::open(config.clone()).unwrap();
    let mut bridge = recorder::storage_bridge::OwnedStorageBridge::spawn(
        bus.clone(),
        store,
        |h| recorder::storage_bridge::StorageBridge::new(h, "p08"),
        Duration::from_secs(3600),
    )
    .unwrap();
    let runtime = runtime::AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus.clone())),
        Arc::new(Provider { requests }),
    )
    .with_run_store(runtime::RunStore::open(&config, bridge.handle()).unwrap());
    let mut projects = workbench_host::projects::StaticProjects::default();
    projects.insert(&ProjectId::new("p").unwrap(), dir.path().into());
    let extra_projects = workbench_protocol::limits::DEFAULT_PAGE_SIZE as usize + 1;
    for n in 0..extra_projects {
        let root = dir.path().join(format!("project-{n}"));
        std::fs::create_dir(&root).unwrap();
        projects.insert(&ProjectId::new(format!("q{n}")).unwrap(), root);
    }
    let host = Arc::new(Mutex::new(
        workbench_host::CommandHost::new(
            workbench_host::commands::CommandService::new(
                runtime.clone(),
                tokio::runtime::Handle::current(),
            )
            .with_event_bus(bus.clone()),
        )
        .with_projects(projects),
    ));
    let identity = core_instance::ConfigIdentity::new(&dir.path().join("config")).unwrap();
    let instance = evorch_core::instance::Instance::acquire(
        &core_instance::LaunchGuard::acquire(&identity).unwrap(),
        dir.path(),
    )
    .unwrap();
    let listener = instance.bind().unwrap();
    listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::UnixListener::from_std(listener).unwrap();
    let backend = Arc::new(evorch_core::backend::HostBackend::new(
        host.clone(),
        instance.identity.clone(),
    ));
    let (_stop, stopped) = tokio::sync::watch::channel(false);
    let socket_backend = backend.clone();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let trace = observed.clone();
    let socket = tokio::spawn(async move {
        for connection in 0..3 {
            let (stream, _) = listener.accept().await.unwrap();
            proxy(
                stream,
                socket_backend.clone(),
                stopped.clone(),
                connection == 0,
                trace.clone(),
            )
            .await;
        }
    });
    let launcher = core_launcher::Launcher::with_socket(
        identity.clone(),
        core_launcher::CoreBinary::explicit(&std::env::current_exe().unwrap()).unwrap(),
        gui::core_client::client_config(),
        instance.socket.clone(),
    )
    .unwrap();
    let (actions, rx) = tokio::sync::mpsc::channel(64);
    let shared = Arc::new(Mutex::new(Shared::default()));
    let desktop = tokio::spawn(serve(
        launcher,
        rx,
        shared.clone(),
        egui::Context::default(),
    ));
    changed(&shared, |s| {
        s.connected && s.cache.projects.len() == extra_projects + 1
    })
    .await;
    assert!(
        observed
            .lock()
            .unwrap()
            .iter()
            .any(|request| { matches!(request.body, RequestBody::Query(Query::ReadSnapshot(_))) })
    );
    let web_server = workbench_web::WebServer::start(
        workbench_web::WebServerConfig {
            enabled: true,
            bind: ([127, 0, 0, 1], 0).into(),
            ..Default::default()
        },
        backend.clone(),
    )
    .await
    .unwrap()
    .unwrap();
    let root = format!("http://{}", web_server.local_addr());
    let http = reqwest::Client::new();
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(1, 1)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    actions
        .send(Action::Upload {
            project: ProjectId::new("p").unwrap(),
            thread: ThreadId::new("t").unwrap(),
            mime: "image/png".into(),
            data: base64::engine::general_purpose::STANDARD.encode(png.into_inner()),
        })
        .await
        .unwrap();
    changed(&shared, |s| {
        s.image_drafts
            .get("t")
            .is_some_and(|i| i[0].reference.is_some())
    })
    .await;
    let image = shared.lock().unwrap().image_drafts["t"][0]
        .reference
        .clone()
        .unwrap();
    let mut initial = send("initial", "desktop initial");
    initial.images.push(image);
    {
        let mut state = shared.lock().unwrap();
        state.drafts.insert(
            "t".into(),
            gui::core_client::Draft {
                text: initial.text.clone(),
                revision: 1,
            },
        );
        state.sending.insert("t".into());
    }
    actions
        .send(Action::Command {
            command: Command::Conversation(ConversationCommand::CreateExecution(initial)),
            draft: Some(("t".into(), 1)),
        })
        .await
        .unwrap();
    let (first_messages, finish_first) = provider.recv().await.unwrap();
    assert!(
        serde_json::to_string(&first_messages)
            .unwrap()
            .contains("image/png")
    );
    changed(&shared, |s| !s.connected && !s.outcome_unknown.is_empty()).await;
    assert_eq!(shared.lock().unwrap().drafts["t"].text, "desktop initial");
    let unknown = shared
        .lock()
        .unwrap()
        .outcome_unknown
        .iter()
        .next()
        .unwrap()
        .clone();
    actions.send(Action::Reconnect).await.unwrap();
    changed(&shared, |s| {
        s.connected
            && s.outcome_unknown.is_empty()
            && s.drafts["t"].text.is_empty()
            && s.cache.runs.values().any(|r| r.thread.as_str() == "t")
    })
    .await;
    {
        let trace = observed.lock().unwrap();
        let creations: Vec<_> = trace
            .iter()
            .filter(|request| {
                matches!(
                    request.body,
                    RequestBody::Command(Command::Conversation(
                        ConversationCommand::CreateExecution(_)
                    ))
                )
            })
            .collect();
        assert_eq!(
            creations.len(),
            1,
            "an unknown command was blindly replayed"
        );
        let receipt = trace
            .iter()
            .find(|request| {
                matches!(&request.body, RequestBody::Query(Query::GetCommandReceipt { request_id }) if request_id.as_str() == unknown)
            })
            .expect("desktop must query the exact lost request");
        assert_eq!(receipt.session_id, creations[0].session_id);
    }
    let run = runtime.list_agents()[0].run_id;
    // Two frontends reach the same queue; verify the provider's order against
    // the acceptance order, not a scheduler-dependent preferred winner.
    actions
        .send(Action::Command {
            command: Command::SendChat(send("desktop-followup", "desktop queued")),
            draft: None,
        })
        .await
        .unwrap();
    web(
        &http,
        &root,
        "web-followup",
        RequestBody::Command(Command::SendChat(send("web-followup", "web queued"))),
    )
    .await;
    changed(&shared, |s| {
        s.cache.receipts.contains_key("desktop-followup")
            && s.cache.receipts.contains_key("web-followup")
    })
    .await;
    let db = rusqlite::Connection::open_with_flags(
        &config.db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let order: Vec<String> = db.prepare("SELECT prompt_id FROM host_input_receipts WHERE prompt_id IN ('desktop-followup','web-followup') ORDER BY acceptance_seq").unwrap().query_map([], |row| row.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
    assert_eq!(order.len(), 2);
    let question = runtime
        .request_user_question(run, "format?".into(), vec!["JSON".into()], true)
        .unwrap();
    bridge.flush().unwrap();
    let query = Query::ListQuestions(PendingQuery {
        thread: Some(ThreadId::new("t").unwrap()),
        pending_only: true,
        page: Default::default(),
    });
    let ReplyPayload::Query(QueryReply::Questions(page)) =
        web(&http, &root, "questions", RequestBody::Query(query)).await
    else {
        panic!()
    };
    let q = page
        .items
        .iter()
        .find(|q| q.id.as_str() == question.id)
        .unwrap();
    web(
        &http,
        &root,
        "answer",
        RequestBody::Command(Command::AnswerUserQuestion(AnswerUserQuestion {
            thread: ThreadId::new("t").unwrap(),
            question_id: q.id.clone(),
            answer: "JSON".into(),
            question_revision: q.routing_revision,
        })),
    )
    .await;
    assert_eq!(
        host.lock()
            .unwrap()
            .answer_provenance(&question.id)
            .unwrap()
            .surface,
        Surface::Web
    );
    let registry = sandbox::approval::ApprovalRegistry::for_bus(&bus);
    let pending = registry.register(
        sandbox::approval::ApprovalRequest {
            run_id: &run.to_string(),
            thread_id: Some("t"),
            tool_name: "shell",
            call_id: "call",
            input: Some(serde_json::json!({"command":"true"})),
        },
        Duration::from_secs(3600),
    );
    let query = Query::ListApprovals(PendingQuery {
        thread: Some(ThreadId::new("t").unwrap()),
        pending_only: true,
        page: Default::default(),
    });
    let ReplyPayload::Query(QueryReply::Approvals(page)) =
        web(&http, &root, "approvals", RequestBody::Query(query)).await
    else {
        panic!()
    };
    let approval = &page.items[0];
    web(
        &http,
        &root,
        "approve",
        RequestBody::Command(Command::DecideToolApproval(DecideToolApproval {
            approval_id: approval.id.clone(),
            core_epoch: approval.core_epoch,
            approval_revision: approval.revision,
            decision: ApprovalDecision::Approve,
        })),
    )
    .await;
    assert_eq!(
        pending.wait().await,
        sandbox::approval::ApprovalState::Approved
    );
    assert_eq!(
        registry
            .get(&sandbox::approval::ApprovalId {
                value: approval.id.to_string(),
                core_epoch: approval.core_epoch.get()
            })
            .unwrap()
            .decided_by
            .unwrap()
            .surface,
        sandbox::approval::ApprovalSurface::Web
    );
    // Closing only the desktop actor (like dropping the window) cannot stop run.
    drop(actions);
    desktop.await.unwrap();
    assert!(matches!(
        runtime.inspect_agent(run).unwrap().phase,
        event_bus::AgentRunPhase::Running | event_bus::AgentRunPhase::Waiting
    ));
    let launcher = core_launcher::Launcher::with_socket(
        identity,
        core_launcher::CoreBinary::explicit(&std::env::current_exe().unwrap()).unwrap(),
        gui::core_client::client_config(),
        instance.socket.clone(),
    )
    .unwrap();
    let (reopened_actions, rx) = tokio::sync::mpsc::channel(64);
    let reopened = Arc::new(Mutex::new(Shared::default()));
    let desktop = tokio::spawn(serve(
        launcher,
        rx,
        reopened.clone(),
        egui::Context::default(),
    ));
    changed(&reopened, |s| {
        s.connected && s.cache.runs.contains_key(&run.to_string())
    })
    .await;
    finish_first.send(()).unwrap();
    let labels = |id: &str| {
        if id == "desktop-followup" {
            "desktop queued"
        } else {
            "web queued"
        }
    };
    loop {
        let (continued_messages, finish) = provider.recv().await.unwrap();
        assert_eq!(
            &continued_messages[..first_messages.len()],
            first_messages.as_slice()
        );
        let wire = serde_json::to_string(&continued_messages).unwrap();
        if let Some(second) = wire.find(labels(&order[1])) {
            assert!(
                wire.find(labels(&order[0]))
                    .expect("second input overtook first")
                    < second
            );
            finish.send(()).unwrap();
            break;
        }
        finish.send(()).unwrap();
    }
    runtime.cancel(run).unwrap();
    runtime.wait(run).await.unwrap();
    bridge.flush().unwrap();
    reopened_actions
        .send(Action::History(ThreadId::new("t").unwrap(), false))
        .await
        .unwrap();
    changed(&reopened, |s| {
        s.cache.history.get("t").is_some_and(|messages| {
            messages.iter().any(|message| {
                matches!(
                    message.content,
                    workbench_protocol::views::MessageContent::Body(_)
                )
            })
        })
    })
    .await;
    let body = reopened.lock().unwrap().cache.history["t"]
        .iter()
        .find_map(|message| match &message.content {
            workbench_protocol::views::MessageContent::Body(body) => Some(body.body_id.clone()),
            _ => None,
        })
        .unwrap();
    reopened_actions
        .send(Action::Body(body.clone()))
        .await
        .unwrap();
    changed(&reopened, |s| s.cache.bodies.contains_key(body.as_str())).await;
    assert_eq!(
        reopened.lock().unwrap().cache.bodies[body.as_str()],
        "confirmed reply".repeat(2000)
    );
    drop(reopened_actions);
    desktop.await.unwrap();
    socket.await.unwrap();
    web_server.shutdown().await.unwrap();
    bridge.shutdown();
}
