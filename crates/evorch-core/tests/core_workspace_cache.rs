//! Trust and sidebar changes made through the production core are bookkeeping
//! for later runs: they never rewrite what a running conversation already sent
//! to the provider. The provider is the production wire (routing, HTTP, and the
//! subscription protocol too) with the shared input-derived cache fixture, and
//! the prefix assertions read the recorded requests, not the model's own view.
use std::sync::Arc;

use event_bus::AgentRunPhase;
use sandbox::CredentialStore;
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
use workbench_host::provenance::local_desktop;
use workbench_protocol::commands::{Command, CommandResult, ComposerRole, SendChat, ThreadTarget};
use workbench_protocol::ids::{ProjectId, SessionId, SubmissionId, ThreadId};
use workbench_protocol::namespaces::workspace::*;
use workbench_protocol::namespaces::{WorkspaceQuery, WorkspaceReply};
use workbench_protocol::queries::{PageRequest, Query, QueryReply};
use workspace_state::{
    ProjectId as StateProject, SidebarDocument, SidebarStore, ThreadId as StateThread, ThreadRecord,
};

#[allow(dead_code)]
#[path = "../../runtime/tests/mcp_cache_support/mod.rs"]
mod cache;

struct Unisolated;
impl runtime::SandboxFactory for Unisolated {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}

struct Core<'a>(&'a BootstrappedHost);

impl Core<'_> {
    fn query(&self, query: WorkspaceQuery) -> WorkspaceReply {
        match self
            .0
            .host
            .lock()
            .unwrap()
            .handle_query(&SessionId::new("test").unwrap(), &Query::Workspace(query))
            .unwrap()
        {
            QueryReply::Workspace(reply) => reply,
            other => panic!("{other:?}"),
        }
    }

    fn apply(&self, command: WorkspaceCommand) {
        let reply = self
            .0
            .host
            .lock()
            .unwrap()
            .handle_command(Command::Workspace(command), local_desktop())
            .unwrap();
        match reply.result.map(|result| *result) {
            Some(CommandResult::Workspace(WorkspaceResult::Applied(_))) => (),
            other => panic!("{other:?}"),
        }
    }

    fn access(&self) -> ProjectAccessView {
        match self.query(WorkspaceQuery::GetProjectAccess(ProjectTarget {
            project: ProjectId::new("p").unwrap(),
        })) {
            WorkspaceReply::ProjectAccess(view) => *view,
            other => panic!("{other:?}"),
        }
    }

    fn detail(&self) -> ThreadDetail {
        match self.query(WorkspaceQuery::GetThreadDetail(ThreadTarget {
            thread: ThreadId::new("t1").unwrap(),
        })) {
            WorkspaceReply::ThreadDetail(detail) => *detail,
            other => panic!("{other:?}"),
        }
    }

    fn family(&self) -> FamilyView {
        match self.query(WorkspaceQuery::ListFamilies(PageRequest::default())) {
            WorkspaceReply::Families(page) => page.items.into_iter().next().unwrap(),
            other => panic!("{other:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn trust_and_sidebar_changes_keep_the_sent_prefix_on_the_production_wire() {
    for subscription in [false, true] {
        let project = cache::Project::new();
        let provider = cache::Provider::new(subscription).await;
        let config = cache::configuration(&provider);
        let root = project.root().canonicalize().unwrap();
        let user = root.join("user");
        std::fs::create_dir_all(&user).unwrap();
        let store = SidebarStore::new(user.join("sidebar.json"));
        let mut document = SidebarDocument::default();
        document
            .sidebar
            .add_project(StateProject::new("p"), "repo", &root)
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
        project
            .secrets
            .set(
                "claude",
                &sandbox::Secret::from(
                    serde_json::json!({
                        "access_token": "fixture-mcp-token",
                        "refresh_token": "fixture-refresh",
                        "expires_at": 4_102_444_800u64
                    })
                    .to_string(),
                ),
            )
            .unwrap();
        let provider_bus = Arc::new(event_bus::EventBus::new(4096));
        let mut provider_events = provider_bus.subscribe();
        let model = workbench_host::bootstrap::models::compose_production_model(
            &config,
            &workbench_host::bootstrap::models::ProductionModel {
                load_options: config::LoadOptions {
                    user_config_dir: Some(user.clone()),
                    ..Default::default()
                },
                credential_store: project.secrets.clone(),
                bus: provider_bus.clone(),
                env: Arc::new(routing::MapEnv::from_iter([(
                    "MCP_CACHE_KEY",
                    "fixture-key",
                )])),
                model_catalog_dir: Some(test_support::model_catalog_dir()),
            },
        )
        .unwrap();
        let mut core = BootstrappedHost::open_with_test_seams(
            BootstrapOptions {
                config_dir: user.clone(),
                project_root: root.clone(),
                ownership_root: root.join("owners"),
            },
            TestSeams {
                model,
                sandbox: Arc::new(Unisolated),
            },
        )
        .await
        .unwrap();
        let mut h = cache::Harness {
            runtime: core.runtime.clone(),
            receiver: core.runtime.event_bus().subscribe(),
        };
        provider.enqueue(vec![cache::answer()]);
        let run: runtime::RunId = {
            let send = SendChat {
                project: ProjectId::new("p").unwrap(),
                thread: ThreadId::new("t1").unwrap(),
                // The production Claude transport requires a UUID prompt identity.
                submission_id: SubmissionId::new("00000000-0000-4000-8000-000000000009").unwrap(),
                composer_role: ComposerRole::Worker,
                text: cache::GOAL.into(),
                skills: vec![],
                images: vec![],
                model_preference: None,
                fork_seed: None,
            };
            let mut host = core.host.lock().unwrap();
            host.reserve_new_chat_thread(&send).unwrap();
            host.handle_command(
                Command::Conversation(
                    workbench_protocol::namespaces::ConversationCommand::SendChat(send),
                ),
                local_desktop(),
            )
            .unwrap()
            .run_id
            .unwrap()
            .as_str()
            .parse()
            .unwrap()
        };
        let thread = core
            .runtime
            .with_storage_reader(|db| db.thread("t1"))
            .unwrap()
            .flatten()
            .unwrap();
        assert_eq!(
            thread.root_run_id.as_deref(),
            Some(run.to_string().as_str())
        );
        cache::through(&mut h.receiver, run, AgentRunPhase::Waiting).await;

        // Everything the workspace area can change, while the run is open.
        let extra = tempfile::tempdir().unwrap();
        let reference = tempfile::tempdir().unwrap();
        let client = Core(&core);
        client.apply(WorkspaceCommand::SetProjectTrust(SetProjectTrust {
            project: ProjectId::new("p").unwrap(),
            trust: TrustLevel::Approved,
            expected_revision: client.access().revision,
        }));
        client.apply(WorkspaceCommand::UpdateAllowedDirectory(
            UpdateAllowedDirectory {
                project: ProjectId::new("p").unwrap(),
                action: AllowedDirectoryAction::Add {
                    path: extra.path().to_string_lossy().into_owned(),
                    trust: TrustLevel::Approved,
                },
                expected_revision: client.access().revision,
            },
        ));
        client.apply(WorkspaceCommand::RenameThread(RenameThread {
            thread: ThreadId::new("t1").unwrap(),
            title: "renamed".into(),
            expected_revision: client.detail().revision,
        }));
        client.apply(WorkspaceCommand::SetThreadExtraReadDirs(
            SetThreadExtraReadDirs {
                thread: ThreadId::new("t1").unwrap(),
                dirs: vec![reference.path().to_string_lossy().into_owned()],
                expected_revision: client.detail().revision,
            },
        ));
        let family = client.family();
        let identity = FamilyIdentity {
            key: family.key.clone(),
            membership_epoch: family.membership_epoch,
        };
        client.apply(WorkspaceCommand::SetFamilyPinned(SetFamilyPinned {
            thread: ThreadId::new("t1").unwrap(),
            pinned: true,
            expected_family: identity.clone(),
            expected_revision: family.pin_revision,
        }));
        let family = client.family();
        client.apply(WorkspaceCommand::SetFamilyArchived(SetFamilyArchived {
            thread: ThreadId::new("t1").unwrap(),
            archived: true,
            expected_family: identity,
            expected_revision: family.archive_revision,
        }));

        provider.enqueue(vec![cache::answer()]);
        core.runtime
            .send_message(run, "Continue after the workspace changes".into())
            .unwrap();
        cache::through(&mut h.receiver, run, AgentRunPhase::Waiting).await;

        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        cache::assert_frozen(&provider, &requests);
        let mut completions = Vec::new();
        while completions.len() < 2 {
            let event = provider_events.recv().await.unwrap();
            if matches!(
                event.kind,
                event_bus::EventKind::Provider(event_bus::ProviderEvent::RequestCompleted { .. })
            ) {
                completions.push(event);
            }
        }
        cache::assert_usage_reuse(&completions, 2);
        provider.assert_drained();
        core.runtime
            .stop(run, runtime::StopScope::SelfOnly)
            .unwrap();
        core.runtime.wait(run).await.unwrap();
        core.shutdown().await.unwrap();
    }
}
