use super::*;
use crate::areas::workspace::WorkspaceArea;
use crate::attachments::AttachmentService;
use providers::{ChatResponse, ContentBlock, FinishReason, Message, Role, ToolSpec};
use runtime::{AgentInvocationContext, AgentModel, AgentRuntime, RuntimeError};
use std::io::{BufRead, Read, Write};
use std::path::Path;
use std::sync::Arc;
use workbench_protocol::ids::{AttachmentId, ThreadId};
use workbench_protocol::namespaces::workspace::*;
use workbench_protocol::namespaces::{WorkspaceQuery, WorkspaceReply};

type Request = (Vec<Message>, tokio::sync::oneshot::Sender<ChatResponse>);
struct Model(tokio::sync::mpsc::UnboundedSender<Request>);
#[async_trait::async_trait]
impl AgentModel for Model {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: runtime::Role,
        messages: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.0.send((messages.to_vec(), tx)).unwrap();
        Ok(rx.await.unwrap())
    }
    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "attachment-test".into()
    }
}

struct World {
    runtime: AgentRuntime,
    host: CommandHost,
    area: WorkspaceArea,
    _storage: storage::Storage,
}
impl World {
    fn new(base: &Path, model: Arc<dyn AgentModel>) -> Self {
        let bus = Arc::new(event_bus::EventBus::new(512));
        let config = storage::StorageConfig {
            db_path: base.join("test.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let executor = Arc::new(tools::ToolExecutor::with_standard_tools(
            bus.clone(),
            Arc::new(sandbox::DirectSandbox::new_unchecked()),
        ));
        let runtime = AgentRuntime::new(bus.clone(), executor, model)
            .with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap())
            .with_sandbox_root(base.to_owned());
        runtime.set_thread_scratch(Arc::new(runtime::scratch::ThreadScratch::at(
            base.join("scratch"),
        )));
        let store = workspace_state::SidebarStore::new(base.join("sidebar.json"));
        if !store.path().exists() {
            let mut document = workspace_state::SidebarDocument::default();
            let project = workspace_state::ProjectId::new("project");
            document
                .sidebar
                .add_project(project.clone(), "test", base)
                .unwrap();
            document
                .sidebar
                .create_thread(workspace_state::ThreadId::new("thread"), project, "test")
                .unwrap();
            let lock = store.lock().unwrap();
            store.write(&lock, &document).unwrap();
        }
        let area = WorkspaceArea::with_store(store, Some(runtime.clone()), vec![], None, |_| false);
        let mut projects = StaticProjects::default();
        projects.insert(&ProjectId::new("project").unwrap(), base.to_owned());
        let host = CommandHost::new(
            CommandService::new(runtime.clone(), tokio::runtime::Handle::current())
                .with_event_bus(bus),
        )
        .with_projects(projects)
        .with_lane_sidebar_path(base.join("sidebar.json"));
        Self {
            runtime,
            host,
            area,
            _storage: storage,
        }
    }
    fn archive(&self, archived: bool) -> WorkspaceResult {
        let WorkspaceReply::Families(page) = self
            .area
            .query(&WorkspaceQuery::ListFamilies(Default::default()))
            .unwrap()
        else {
            panic!()
        };
        let family = &page.items[0];
        let reply = self
            .area
            .command(
                WorkspaceCommand::SetFamilyArchived(SetFamilyArchived {
                    thread: ThreadId::new("thread").unwrap(),
                    archived,
                    expected_family: FamilyIdentity {
                        key: family.key.clone(),
                        membership_epoch: family.membership_epoch,
                    },
                    expected_revision: family.archive_revision,
                }),
                &crate::provenance::local_desktop(),
            )
            .unwrap();
        let workbench_protocol::commands::CommandResult::Workspace(result) = *reply.result.unwrap()
        else {
            panic!()
        };
        result
    }
    fn upload(&self, draft: &str) -> workbench_protocol::commands::AttachmentRef {
        AttachmentService::new(self.runtime.attachment_root().unwrap())
            .upload_bytes(
                ProjectId::new("project").unwrap(),
                ThreadId::new("thread").unwrap(),
                draft.into(),
                "note.txt".into(),
                "text/plain".into(),
                b"restored-attachment-content",
            )
            .unwrap()
    }
}

#[test]
#[ignore = "subprocess entry point; invoked by archive_and_commit_serialize_across_processes"]
fn competing_sender_process() {
    let base = std::env::var_os("EVORCH_RACE_BASE").unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _entered = rt.enter();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let world = World::new(Path::new(&base), Arc::new(Model(tx)));
    let root = world.runtime.attachment_root().unwrap();
    let id = AttachmentId::new(std::env::var("EVORCH_RACE_ID").unwrap()).unwrap();
    world.host.check_attachment_thread_active("thread").unwrap();
    let send_first = std::env::var("EVORCH_SEND_FIRST").unwrap() == "true";
    let lock = send_first.then(|| {
        world
            .host
            .lock_active_attachment_thread(&root, "thread")
            .unwrap()
    });
    if send_first {
        AttachmentService::new(root.clone())
            .commit_locked(
                &ProjectId::new("project").unwrap(),
                &ThreadId::new("thread").unwrap(),
                &[id],
            )
            .unwrap();
    }
    println!("_EVORCH_SENDER_READY_");
    std::io::stdout().flush().unwrap();
    std::io::stdin().read_exact(&mut [0]).unwrap();
    if !send_first {
        // This is the same lock+recheck used immediately before ingress publication.
        // The earlier active check was true, but archive won in the other process.
        assert_eq!(
            world
                .host
                .lock_active_attachment_thread(&root, "thread")
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
    }
    drop(lock);
}

#[test]
fn archive_and_commit_serialize_across_processes() {
    for send_first in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _entered = rt.enter();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let world = World::new(temp.path(), Arc::new(Model(tx)));
        let reference = world.upload("draft");
        let root = world.runtime.attachment_root().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ingress::attachment_race::competing_sender_process",
                "--include-ignored",
                "--nocapture",
            ])
            .env("EVORCH_RACE_BASE", temp.path())
            .env("EVORCH_RACE_ID", reference.id.as_str())
            .env("EVORCH_SEND_FIRST", send_first.to_string())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut reader = std::io::BufReader::new(child.stdout.take().unwrap());
        loop {
            let mut line = String::new();
            assert_ne!(
                reader.read_line(&mut line).unwrap(),
                0,
                "sender exited before handshake"
            );
            if line.contains("_EVORCH_SENDER_READY_") {
                break;
            }
        }
        if send_first {
            assert!(matches!(
                world.archive(true),
                WorkspaceResult::Rejected(WorkspaceRejection::StorageFailed { .. })
            ));
        } else {
            assert!(matches!(world.archive(true), WorkspaceResult::Applied(_)));
        }
        child.stdin.take().unwrap().write_all(b"x").unwrap();
        let mut tail = String::new();
        reader.read_to_string(&mut tail).unwrap();
        assert!(child.wait().unwrap().success(), "{tail}");
        if send_first {
            assert!(matches!(world.archive(true), WorkspaceResult::Applied(_)));
        }
        assert!(
            !root
                .path()
                .join(sandbox::attachments::AttachmentRoot::thread_name("thread"))
                .exists()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_cleanup_rechecks_archive_after_restore_and_new_send() {
    let temp = tempfile::tempdir().unwrap();
    let (tx, mut requests) = tokio::sync::mpsc::unbounded_channel();
    let mut live = World::new(temp.path(), Arc::new(Model(tx)));
    live.upload("before-archive");
    assert!(matches!(live.archive(true), WorkspaceResult::Applied(_)));

    let (snapshot_tx, snapshot_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let base = temp.path().to_owned();
    let handle = tokio::runtime::Handle::current();
    let startup = std::thread::spawn(move || {
        let _entered = handle.enter();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut startup = World::new(&base, Arc::new(Model(tx)));
        startup.host.reconcile_attachments_after_snapshot(|| {
            snapshot_tx.send(()).unwrap();
            resume_rx.recv().unwrap();
        })
    });
    // Startup has observed archived=true but has not acquired attachment locks.
    snapshot_rx.await.unwrap();
    assert!(matches!(live.archive(false), WorkspaceResult::Applied(_)));
    let reference = live.upload("after-restore");
    let reply = send(&mut live, "new-send", vec![reference.id]);
    let (messages, _answer) = requests.recv().await.unwrap();
    let user = messages.iter().rfind(|m| m.role == Role::User).unwrap();
    let [ContentBlock::Text { text }] = user.content.as_slice() else {
        panic!()
    };
    let path: String = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    resume_tx.send(()).unwrap();
    startup.join().unwrap().unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"restored-attachment-content");
    let run = reply.run_id.unwrap().as_str().parse().unwrap();
    live.runtime.cancel(run).unwrap();
    live.runtime.wait(run).await.unwrap();
}

fn response(content: Vec<ContentBlock>, finish_reason: FinishReason) -> ChatResponse {
    ChatResponse {
        usage_reported: true,
        message: Message {
            role: Role::Assistant,
            content,
        },
        usage: Default::default(),
        finish_reason,
    }
}

fn send(
    world: &mut World,
    submission: &str,
    ids: Vec<AttachmentId>,
) -> workbench_protocol::commands::CommandReply {
    world
        .host
        .handle_command(
            Command::Conversation(
                workbench_protocol::namespaces::ConversationCommand::SendChat(SendChat {
                    project: ProjectId::new("project").unwrap(),
                    thread: ThreadId::new("thread").unwrap(),
                    submission_id: SubmissionId::new(submission).unwrap(),
                    composer_role: WireRole::Worker,
                    text: submission.into(),
                    attachments: ids,
                    skills: vec![],
                    model_preference: None,
                    fork_seed: None,
                }),
            ),
            crate::provenance::local_desktop(),
        )
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn unarchive_new_attachment_and_withdrawn_reupload_reach_runtime_read() {
    for withdraw in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut world = World::new(temp.path(), Arc::new(Model(tx)));
        let old = world.upload("before-archive");
        assert!(matches!(world.archive(true), WorkspaceResult::Applied(_)));
        assert!(matches!(world.archive(false), WorkspaceResult::Applied(_)));
        assert!(
            AttachmentService::new(world.runtime.attachment_root().unwrap())
                .references(
                    &ProjectId::new("project").unwrap(),
                    &ThreadId::new("thread").unwrap(),
                    &[old.id]
                )
                .is_err()
        );

        let held = if withdraw {
            send(&mut world, "hold", vec![]);
            Some(rx.recv().await.unwrap().1)
        } else {
            None
        };
        let mut fresh = world.upload("after-restore");
        if withdraw {
            let queued = send(&mut world, "withdraw-me", vec![fresh.id.clone()]);
            world
                .host
                .handle_command(
                    Command::Conversation(
                        workbench_protocol::namespaces::ConversationCommand::WithdrawFollowUp(
                            WithdrawFollowUp {
                                thread: ThreadId::new("thread").unwrap(),
                                run: queued.run_id.unwrap(),
                                submission_id: SubmissionId::new("withdraw-me").unwrap(),
                            },
                        ),
                    ),
                    crate::provenance::local_desktop(),
                )
                .unwrap();
            // The desktop restores raw bytes and uploads a new draft rather than
            // treating a previously published reference as an unsent upload.
            let reuploaded = world.upload("resend-draft");
            assert_ne!(reuploaded.id, fresh.id);
            fresh = reuploaded;
        }
        let reply = send(&mut world, "read-this", vec![fresh.id]);
        if let Some(held) = held {
            held.send(response(
                vec![ContentBlock::Text {
                    text: "done".into(),
                }],
                FinishReason::Stop,
            ))
            .unwrap();
        }
        let (messages, answer) = rx.recv().await.unwrap();
        let user = messages.iter().rfind(|m| m.role == Role::User).unwrap();
        let [ContentBlock::Text { text }] = user.content.as_slice() else {
            panic!("attachment must be text, not an image block")
        };
        assert!(text.starts_with("read-this\n\n"));
        assert_eq!(
            text.matches("[Attached files (read-only; open with read or shell)]")
                .count(),
            1
        );
        let path: String = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        answer
            .send(response(
                vec![ContentBlock::ToolUse {
                    id: "attachment-read".into(),
                    name: "read".into(),
                    input: serde_json::json!({"path": path}),
                }],
                FinishReason::ToolUse,
            ))
            .unwrap();
        let (messages, _answer) = rx.recv().await.unwrap();
        let result = messages
            .iter()
            .flat_map(|m| &m.content)
            .find_map(|b| match b {
                ContentBlock::ToolResult {
                    tool_call_id,
                    content,
                    ..
                } if tool_call_id == "attachment-read" => Some(content),
                _ => None,
            })
            .unwrap();
        assert!(result.iter().any(|b| matches!(b, providers::ToolResultContent::Text { text } if text.contains("restored-attachment-content"))), "{result:?}");
        let run = reply.run_id.unwrap().as_str().parse().unwrap();
        world.runtime.cancel(run).unwrap();
        world.runtime.wait(run).await.unwrap();
    }
}
