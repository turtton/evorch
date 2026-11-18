//! Real CEF through the production core owner, with test-only subreaper checks.
#![cfg(all(target_os = "linux", feature = "renderer-test-helper"))]
#[path = "renderer_support/mod.rs"]
mod support;
use renderer_ipc::{OpenArtifact, SessionId, TabId, Viewport};
use workbench_host::areas::{
    AreaTasks,
    browser::renderer::{OwnerConfig, RendererOwner},
};
fn executable() -> std::path::PathBuf {
    let exe = std::env::var_os("EVORCH_RENDERER_EXE")
        .map(std::path::PathBuf::from)
        .expect("cef-e2e must supply EVORCH_RENDERER_EXE");
    assert!(exe.is_absolute() && exe.is_file());
    assert!(exe.with_file_name("evorch-cef-helper").is_file());
    exe
}
#[tokio::test]
#[ignore = "requires actual CEF; mandatory cef-e2e supplies EVORCH_RENDERER_EXE"]
async fn real_owner_reclaims_graceful_and_forced_groups_without_touching_unrelated_child() {
    // Baseline child stays alive across both renderer generations.
    let mut unrelated = tokio::process::Command::new("sh")
        .args(["-c", "read line"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut tree = support::Tree::new();
    let root = tempfile::tempdir().unwrap();
    let tasks = AreaTasks::new(tokio::runtime::Handle::current());
    let ledger = sandbox::SpawnLedger::new();
    let owner = RendererOwner::new(
        &tasks,
        ledger.clone(),
        OwnerConfig::new(executable(), root.path().join("profiles")),
    )
    .unwrap();
    for graceful in [true, false] {
        let (open, body) = OpenArtifact::with_files(
            SessionId(1),
            TabId(1),
            Viewport {
                width: 1024,
                height: 768,
            },
            30,
            30_000,
            vec![(
                "index.html".into(),
                "text/html".into(),
                b"<canvas>renderer owner</canvas>".to_vec(),
            )],
        );
        let tab = owner.open(open, body).await.unwrap();
        tree.track(&ledger);
        owner.set_frame_rate(tab, 10).await.unwrap();
        owner.set_streaming(tab, false).await.unwrap();
        owner.set_streaming(tab, true).await.unwrap();
        if graceful {
            owner.close(tab).await.unwrap();
            owner.stop().await.unwrap();
        } else {
            ledger.stop_all_children();
            let mut view = owner.subscribe();
            view.wait_for(|s| s.phase == workbench_host::areas::browser::renderer::Phase::Failed)
                .await
                .unwrap();
        }
        assert!(ledger.children().is_empty());
        assert!(unrelated.try_wait().unwrap().is_none());
    }
    ledger.close_fence();
    tasks.request_stop();
    tasks.shutdown().await.unwrap();
    ledger.reap_all_children().await.unwrap();
    ledger.join_workers().await.unwrap();
    tree.assert_clean();
    drop(unrelated.stdin.take());
    unrelated.wait().await.unwrap();
    assert_eq!(
        std::fs::read_dir(root.path().join("profiles"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
#[ignore = "requires actual CEF; mandatory cef-e2e supplies EVORCH_RENDERER_EXE"]
async fn abrupt_real_core_exit_ends_helper_by_eof_and_collects_the_old_profile() {
    let dir = tempfile::tempdir().unwrap();
    support::core_eof(executable(), &dir.path().join("profiles")).await;
}

#[tokio::test]
#[ignore = "requires actual CEF; mandatory cef-e2e supplies EVORCH_RENDERER_EXE"]
async fn inspect_artifact_pages_open_through_the_core_owner_with_the_owned_record() {
    use runtime::artifacts::{ArtifactPages as _, ArtifactRecord};
    use workbench_host::areas::browser::run_pages::RuntimePages;
    let root = tempfile::tempdir().unwrap();
    let tasks = AreaTasks::new(tokio::runtime::Handle::current());
    let ledger = sandbox::SpawnLedger::new();
    let owner = RendererOwner::new(
        &tasks,
        ledger,
        OwnerConfig::new(executable(), root.path().join("profiles")),
    )
    .unwrap();
    let pages = RuntimePages::new(Some(owner.clone()));
    let record = ArtifactRecord {
        artifact_id: "artifact-1".into(),
        sha256: "0".repeat(64),
        media_type: "text/html".into(),
        extension: "html".into(),
        title: "page".into(),
        caption: None,
        source_path: "/unused".into(),
        byte_len: 0,
        run_id: "run-1".into(),
        root_run_id: "run-1".into(),
        thread_id: Some("thread-1".into()),
        inlined_assets: Vec::new(),
    };
    let page = pages
        .open_artifact("run-1", b"<button>inspect me</button>".to_vec(), &record)
        .await
        .unwrap();
    assert!(pages.page("run-1").is_some());
    let png = page.agent().screenshot().await.unwrap();
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    // IHDR: the screenshot is the 1024x768 viewport.
    assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 1024);
    assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 768);
    assert!(pages.close("run-1"));
    owner.stop().await.unwrap();
}

/// Drives the model through the real `inspect_artifact` tool of a bootstrapped
/// core, so the provider the core injects is what reaches real CEF.
struct InspectScript {
    step: std::sync::Mutex<usize>,
    results: std::sync::Arc<std::sync::Mutex<Vec<(bool, String)>>>,
}
fn tool_texts(messages: &[providers::Message]) -> Vec<(bool, String)> {
    use providers::{ContentBlock, ToolResultContent};
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                is_error, content, ..
            } => Some((
                *is_error,
                content
                    .iter()
                    .map(|part| match part {
                        ToolResultContent::Text { text } => text.clone(),
                        ToolResultContent::Image { media_type, .. } => {
                            format!("[image {media_type}]")
                        }
                    })
                    .collect(),
            )),
            _ => None,
        })
        .collect()
}
#[async_trait::async_trait]
impl runtime::AgentModel for InspectScript {
    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "inspect-script".into()
    }
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
        messages: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        use serde_json::json;
        let results = tool_texts(messages);
        *self.results.lock().unwrap() = results.clone();
        let latest = results.last().map(|(_, text)| text.clone());
        let mut step = self.step.lock().unwrap();
        let call = match *step {
            0 => Some((
                "render_artifact",
                json!({"path": "page.html", "title": "page"}),
            )),
            1 => {
                let id = serde_json::from_str::<serde_json::Value>(&latest.unwrap()).unwrap()
                    ["artifact_id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                Some((
                    "inspect_artifact",
                    json!({"action": "open", "artifact_id": id}),
                ))
            }
            2 => Some(("inspect_artifact", json!({"action": "snapshot"}))),
            3 => {
                let outline = latest.unwrap();
                let at = outline.find("[ref=").expect("an outline ref") + "[ref=".len();
                let reference = outline[at..].split(']').next().unwrap().to_owned();
                Some((
                    "inspect_artifact",
                    json!({"action": "click", "ref": reference}),
                ))
            }
            4 => Some(("inspect_artifact", json!({"action": "screenshot"}))),
            5 => Some(("inspect_artifact", json!({"action": "close"}))),
            _ => None,
        };
        *step += 1;
        let (content, finish_reason) = match call {
            Some((name, input)) => (
                vec![providers::ContentBlock::ToolUse {
                    id: format!("call-{}", *step),
                    name: name.into(),
                    input,
                }],
                providers::FinishReason::ToolUse,
            ),
            None => (
                vec![providers::ContentBlock::Text {
                    text: "inspected".into(),
                }],
                providers::FinishReason::Stop,
            ),
        };
        Ok(providers::ChatResponse {
            usage_reported: true,
            message: providers::Message {
                role: providers::Role::Assistant,
                content,
            },
            finish_reason,
            usage: providers::Usage::default(),
        })
    }
}
struct Unisolated;
impl runtime::SandboxFactory for Unisolated {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<std::sync::Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(std::sync::Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires actual CEF; mandatory cef-e2e supplies EVORCH_RENDERER_EXE"]
async fn the_core_inspect_artifact_tool_runs_open_snapshot_click_screenshot_close_in_real_cef() {
    use event_bus::{AgentRunPhase, EventKind, LifecycleEvent};
    use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
    use workbench_protocol::commands::{Command, ComposerRole, SendChat};
    use workbench_protocol::ids::{ProjectId, SubmissionId, ThreadId};
    use workspace_state::{
        ProjectId as StateProject, SidebarDocument, SidebarStore, ThreadId as StateThread,
        ThreadRecord,
    };

    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path().canonicalize().unwrap();
    let project = root.join("repo");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("page.html"),
        "<!doctype html><button>Add</button>",
    )
    .unwrap();
    let store = SidebarStore::new(root.join("sidebar.json"));
    let mut document = SidebarDocument::default();
    document
        .sidebar
        .add_project(StateProject::new("p"), "repo", &project)
        .unwrap();
    document.sidebar.threads.push(ThreadRecord::new(
        StateThread::new("t1"),
        StateProject::new("p"),
        "first",
    ));
    document.sidebar.refresh_derived();
    {
        let lock = store.lock().unwrap();
        store.write(&lock, &document).unwrap();
    }
    // The core resolves its helper from this variable at bootstrap; the only
    // value the process ever sets is the real renderer.
    // SAFETY: set once, before the core and its threads exist.
    unsafe { std::env::set_var("EVORCH_RENDERER_EXEC", executable()) };
    let results = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut core = BootstrappedHost::open_with_test_seams(
        BootstrapOptions {
            config_dir: root.clone(),
            project_root: project.clone(),
            ownership_root: dir.sockets().into(),
        },
        TestSeams {
            model: std::sync::Arc::new(InspectScript {
                step: std::sync::Mutex::new(0),
                results: results.clone(),
            }),
            sandbox: std::sync::Arc::new(Unisolated),
        },
    )
    .await
    .unwrap();
    let mut events = core.runtime.event_bus().subscribe();
    let send = SendChat {
        project: ProjectId::new("p").unwrap(),
        thread: ThreadId::new("t1").unwrap(),
        submission_id: SubmissionId::new("00000000-0000-4000-8000-000000000009").unwrap(),
        composer_role: ComposerRole::Worker,
        text: "Inspect the page".into(),
        skills: vec![],
        attachments: vec![],
        model_preference: None,
        fork_seed: None,
    };
    let run: runtime::RunId = {
        let mut host = core.host.lock().unwrap();
        host.reserve_new_chat_thread(&send).unwrap();
        host.handle_command(
            Command::Conversation(
                workbench_protocol::namespaces::ConversationCommand::SendChat(send),
            ),
            workbench_host::provenance::local_desktop(),
        )
        .unwrap()
        .run_id
        .unwrap()
        .as_str()
        .parse()
        .unwrap()
    };
    loop {
        let event = events.recv().await.unwrap();
        if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. }) =
            &event.kind
            && run_id == &run.to_string()
        {
            assert!(
                !matches!(to, AgentRunPhase::Error | AgentRunPhase::Stopped),
                "{event:?}"
            );
            if *to == AgentRunPhase::Waiting {
                break;
            }
        }
    }
    // The model's last request carries every tool result of the script.
    let results = results.lock().unwrap().clone();
    assert_eq!(results.len(), 6, "{results:?}");
    for (index, (is_error, text)) in results.iter().enumerate() {
        assert!(!is_error, "tool call {index} failed: {text}");
    }
    assert!(results[2].1.contains("Add"), "{}", results[2].1);
    // The click returned the refreshed outline (artifact pages run no inline script).
    assert!(results[3].1.contains("button \"Add\""), "{}", results[3].1);
    assert!(results[4].1.contains("1024x768"), "{}", results[4].1);
    assert!(results[5].1.contains("closed"), "{}", results[5].1);
    core.runtime
        .stop(run, runtime::StopScope::SelfOnly)
        .unwrap();
    core.runtime.wait(run).await.unwrap();
    core.shutdown().await.unwrap();
}
