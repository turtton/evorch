use super::*;
use crate::restart::{
    RestartCoordinator, RestartId, RestartPhase,
    public::{FinalizationOwner, LiveUndisposedAdmission, PublicRestartCoordinator},
};
use event_bus::{AgentRunPhase, EventKind, EventReceiver, LifecycleEvent};
use runtime::{Role, RunConfig};
use std::os::unix::fs::PermissionsExt;
use workbench_host::admin::{AdminContinuation, AdminFacade};
use workbench_host::{
    bootstrap::{BootstrapOptions, TestSeams},
    commands::{ChatInput, ComposerRole, HostCommand, HostEvent},
};
use workbench_protocol::{
    envelope::{Provenance, Surface},
    ids::ClientId,
    namespaces::lifecycle::{
        AdminId, LifecycleCommand, LifecycleQuery, LifecycleReply,
        restart::{RequestRestart, RestartBinding, RestartCommand, RestartQuery, RestartReply},
    },
    wire::InstanceEpoch,
};

use crate::restart::tests::restart_model as model;

struct RecoveryModel {
    inner: model::Model,
    block_child: std::sync::atomic::AtomicBool,
    block_parent: std::sync::atomic::AtomicBool,
    parent_entered: tokio::sync::Notify,
    parent_release: tokio::sync::Semaphore,
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}
impl Default for RecoveryModel {
    fn default() -> Self {
        Self {
            inner: Default::default(),
            block_child: false.into(),
            block_parent: false.into(),
            parent_entered: Default::default(),
            parent_release: tokio::sync::Semaphore::new(0),
            entered: Default::default(),
            release: tokio::sync::Semaphore::new(0),
        }
    }
}
#[async_trait::async_trait]
impl runtime::AgentModel for RecoveryModel {
    fn selected_model(&self, role: Role, category: Option<&str>) -> String {
        self.inner.selected_model(role, category)
    }
    fn requires_admission(&self) -> bool {
        false
    }
    fn checkpoint_provider_input(&self, run: &str) -> Option<runtime::quiesce::ProviderInput> {
        self.inner.checkpoint_provider_input(run)
    }
    fn validate_checkpoint_compatibility(
        &self,
        invocation: &runtime::AgentInvocationContext,
        _: Role,
        pins: &[runtime::RoutePinRecord],
        input: Option<&runtime::quiesce::ProviderInput>,
    ) -> Result<(), runtime::RuntimeError> {
        assert!(pins.is_empty());
        let expected = invocation
            .model_preference
            .as_ref()
            .and_then(|p| p.model.as_deref())
            .unwrap_or("restart-fixture");
        assert!(input.is_some_and(|input| input.request.model == expected));
        Ok(())
    }
    fn adopt_recovery_checkpoint(
        &self,
        run: &str,
        pins: &[runtime::RoutePinRecord],
        input: Option<&runtime::quiesce::ProviderInput>,
    ) {
        assert!(pins.is_empty());
        self.inner
            .1
            .lock()
            .unwrap()
            .insert(run.into(), input.unwrap().clone());
    }
    async fn admit(
        &self,
        _: &runtime::AgentInvocationContext,
        _: Role,
    ) -> Result<(), runtime::RuntimeError> {
        Ok(())
    }
    async fn complete(
        &self,
        invocation: &runtime::AgentInvocationContext,
        role: Role,
        messages: &[providers::Message],
        tools: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        let response = self
            .inner
            .complete(invocation, role, messages, tools)
            .await?;
        if let Some(preference) = &invocation.model_preference {
            let mut inputs = self.inner.1.lock().unwrap();
            let input = inputs.get_mut(&invocation.run_id).unwrap();
            input.profile.clone_from(&preference.profile);
            input.request.model = preference
                .model
                .clone()
                .unwrap_or_else(|| "restart-fixture".into());
            input
                .request
                .reasoning_effort
                .clone_from(&preference.reasoning_effort);
        }
        if role == Role::Orchestrator && self.block_parent.load(std::sync::atomic::Ordering::SeqCst)
        {
            self.parent_entered.notify_one();
            self.parent_release.acquire().await.unwrap().forget();
        }
        if role == Role::Worker && self.block_child.load(std::sync::atomic::Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.acquire().await.unwrap().forget();
            return Ok(providers::ChatResponse {
                message: providers::Message {
                    role: providers::Role::Assistant,
                    content: vec![providers::ContentBlock::ToolUse {
                        id: "child-read".into(),
                        name: "read".into(),
                        input: serde_json::json!({"file_path":"child-input.txt"}),
                    }],
                },
                usage: Default::default(),
                usage_reported: true,
                finish_reason: providers::FinishReason::ToolUse,
            });
        }
        Ok(response)
    }
}

struct NoFinalization;
impl FinalizationOwner for NoFinalization {
    fn finalize(
        &self,
        _: RestartCoordinator,
        _: RestartId,
        _: Arc<core_launcher::handoff::CurrentElf>,
        _: core_launcher::handoff::targets::CaptureCheckedTarget,
    ) -> AdminContinuation {
        panic!("refused inspection must not finalize")
    }
}

struct Fixture {
    _dir: test_socket_dir::SplitDir,
    boot: BootstrappedHost,
    source: HostRecipeSource,
    events: EventReceiver,
    model: Arc<RecoveryModel>,
}
impl Fixture {
    async fn new() -> Self {
        let dir = test_socket_dir::SplitDir::new();
        let root = dir.path().canonicalize().unwrap();
        let mut sidebar = workspace_state::SidebarState::default();
        sidebar
            .add_project(
                workspace_state::ProjectId::new("fixture-project"),
                "fixture",
                &root,
            )
            .unwrap();
        sidebar.projects[0].trust = workspace_state::TrustState::Approved;
        std::fs::write(
            root.join("sidebar.json"),
            serde_json::to_vec(&sidebar).unwrap(),
        )
        .unwrap();
        let wrapper = root.join("target.sh");
        let description = crate::handoff_description::describe().unwrap();
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nprintf '%s\\n' '{}'\n",
                serde_json::to_string(&description)
                    .unwrap()
                    .replace('\'', "'\"'\"'")
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(
            root.join("config.toml"),
            format!("[core.restart.targets.local]\npath = {wrapper:?}\n"),
        )
        .unwrap();
        let model = Arc::new(RecoveryModel::default());
        let boot = BootstrappedHost::open_for_core_with_test_seams(
            BootstrapOptions {
                config_dir: root.clone(),
                project_root: root,
                ownership_root: dir.sockets().to_path_buf(),
            },
            TestSeams {
                model: model.clone(),
                sandbox: Arc::new(model::Direct),
            },
            &crate::recovery_service::CoreGraphRecovery,
        )
        .await
        .unwrap();
        let source = HostRecipeSource::new(&boot);
        let events = boot.runtime.event_bus().subscribe();
        Self {
            _dir: dir,
            boot,
            source,
            events,
            model,
        }
    }
    async fn chat(&mut self) -> RunId {
        let events = self.boot.host.lock().unwrap().submit_local(
            HostCommand::SendChat(ChatInput {
                submission_id: "first".into(),
                project_root: Some(self._dir.path().canonicalize().unwrap()),
                composer_role: ComposerRole::Orchestrator,
                attachments: Vec::new(),
                thread_id: "plain".into(),
                text: "ordinary conversation".into(),
                skills: Vec::new(),
                model_preference: runtime::ModelPreferenceUpdate::Keep,
                fork_seed: None,
            }),
            &Provenance::human(Surface::Desktop, ClientId::new("human").unwrap()),
        );
        let run = events
            .iter()
            .find_map(|event| match event {
                HostEvent::ChatAccepted { run_id, .. } => Some(run_id.parse().unwrap()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("chat rejected: {events:?}"));
        self.waiting(run).await;
        run
    }
    async fn waiting(&mut self, run: RunId) {
        loop {
            if matches!(self.events.recv().await.unwrap().kind,
                EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                    run_id, to: AgentRunPhase::Waiting, ..
                }) if run_id == run.to_string())
            {
                break;
            }
        }
    }
    fn inventory(&self) -> Vec<RestartRecipeMember> {
        self.boot.runtime.restart_recipe_inventory().unwrap()
    }
    async fn configured_chat(&mut self, mut config: RunConfig) -> RunId {
        config.interactive = true;
        config.keep_alive = true;
        let run = self
            .boot
            .runtime
            .delegate_chat("plain", Role::Worker, "classified chat".into(), config)
            .unwrap();
        {
            let mut host = self.boot.host.lock().unwrap();
            let service = host.service_mut();
            service.bindings.chat_runs.insert("plain".into(), run);
            service.publish_lane_bindings();
        }
        self.waiting(run).await;
        run
    }
    fn calls(&self) -> usize {
        self.model.inner.0.load(std::sync::atomic::Ordering::SeqCst)
    }
    async fn rejected(&self, kind: &str) {
        let calls = self.calls();
        let binding_observation = self.source.bindings().unwrap().observed;
        let error = self.source.collect().await.err().expect("must reject");
        assert_eq!(error.code, ErrorCode::UnsupportedCapability);
        assert!(
            self.inventory()
                .iter()
                .any(|member| error.message.contains(&member.run.to_string())),
            "{error:?}"
        );
        assert!(error.message.contains(kind), "{error:?}");
        assert_eq!(self.calls(), calls, "collection must not invoke a provider");
        assert!(!error.message.contains(self._dir.path().to_str().unwrap()));
        let db = storage::Database::open(self.boot.storage_config()).unwrap();
        assert!(db.restart_transactions().unwrap().is_empty());
        // Exercise the real public inspection boundary, not just collection.
        let core = RestartCoordinator::new(&self.boot);
        let epoch = InstanceEpoch(1);
        let targets = super::super::targets::build(
            self._dir.path().canonicalize().unwrap(),
            Arc::new(core_launcher::handoff::CurrentElf::record().unwrap()),
        )
        .unwrap();
        let leaf = PublicRestartCoordinator::new(
            core.clone(),
            epoch,
            targets,
            Arc::new(HostRecipeSource::new(&self.boot)),
            Arc::new(LiveUndisposedAdmission {
                collector: self.boot.graph_collector().clone(),
                core: core.clone(),
                epoch,
            }),
            Arc::new(NoFinalization),
        );
        let (socket, _peer) = tokio::net::UnixStream::pair().unwrap();
        let lease = workbench_authority::verify_unix_peer(&socket).unwrap();
        let verified = lease.authority().connection().unwrap();
        let LifecycleReply::Restart(RestartReply::Targets(targets)) = leaf
            .query(&LifecycleQuery::Restart(RestartQuery::Targets), &verified)
            .await
            .unwrap()
        else {
            panic!("expected targets")
        };
        let intent = LifecycleCommand::Restart(RestartCommand::Request(RequestRestart {
            binding: RestartBinding {
                target_id: AdminId::new("local").unwrap(),
                expected_core_epoch: epoch,
                target_revision: targets.revision,
                operation_id: AdminId::new("inspection").unwrap(),
            },
            confirmed: true,
        }));
        let failure = leaf.inspect(&intent, &verified).await.err().unwrap();
        assert_eq!(failure.code, error.code);
        assert_eq!(failure.message, error.message);
        assert_eq!(core.snapshot().phase, RestartPhase::Running);
        assert_eq!(core.snapshot().revision, 0);
        assert!(core.snapshot().id.is_none());
        assert!(db.restart_transactions().unwrap().is_empty());
        assert_eq!(self.calls(), calls);
        assert!(binding_observation.is_current());
    }
    async fn lane(
        &mut self,
        lead: RunId,
        policy: event_bus::LanePolicy,
    ) -> runtime::lanes::LaneOpened {
        let lane = self
            .boot
            .runtime
            .open_lane(lead, policy.as_str(), lane_brief(policy))
            .unwrap();
        self.waiting(lane.root_run_id).await;
        lane
    }
    async fn shutdown(mut self) {
        self.boot.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn plain_host_conversation_and_normal_child_keep_every_descriptor() {
    ordinary_or_lane_graph(false, false).await;
}

#[tokio::test]
async fn restored_lane_can_recollect_recipes_without_gui_bindings_or_a_second_physical_restart() {
    ordinary_or_lane_graph(true, false).await;
}

#[tokio::test]
async fn adopted_explicit_choice_is_accepted_by_host_recipe_source_and_restored() {
    ordinary_or_lane_graph(false, true).await;
}

async fn ordinary_or_lane_graph(is_lane: bool, switch_choice: bool) {
    let mut f = Fixture::new().await;
    let lead = f.chat().await;
    let (root, thread) = if is_lane {
        let lane = f.lane(lead, event_bus::LanePolicy::Attached).await;
        // The lane is an independent root, not a descendant of its lead.
        f.boot
            .runtime
            .stop(lead, runtime::StopScope::Subtree)
            .unwrap();
        f.boot.runtime.wait(lead).await.unwrap();
        (lane.root_run_id, lane.thread_id)
    } else {
        (lead, "plain".into())
    };
    let selection = runtime::ModelPreference {
        profile: "restart-fixture".into(),
        model: Some("switched-fixture".into()),
        reasoning_effort: Some("high".into()),
    };
    if switch_choice {
        f.boot
            .runtime
            .continue_goal_with_model_update(
                root,
                "adopt explicit selection".into(),
                RunConfig::default(),
                runtime::ModelPreferenceUpdate::explicit(selection.clone()),
            )
            .unwrap();
        f.waiting(root).await;
    }
    let scope = is_lane.then_some(event_bus::LaneScope::Lane);
    std::fs::write(f._dir.path().join("child-input.txt"), "recoverable child").unwrap();
    f.model
        .block_child
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let child = f
        .boot
        .runtime
        .delegate_background_as_child(
            root,
            Role::Worker,
            "ordinary delegated child",
            RunConfig {
                interactive: false,
                keep_alive: false,
                ..Default::default()
            },
        )
        .unwrap();
    f.model.entered.notified().await;
    let inventory = f.boot.runtime.restart_recipe_inventory().unwrap();
    let child_metadata = inventory.iter().find(|m| m.run == child).unwrap();
    assert_eq!(child_metadata.root, root);
    assert_eq!(child_metadata.parent, Some(root));
    assert_eq!(child_metadata.thread.as_deref(), Some(thread.as_str()));
    let root_metadata = inventory.iter().find(|m| m.run == root).unwrap();
    assert!(root_metadata.conversation);
    assert_eq!(root_metadata.lane, is_lane);
    assert!(!root_metadata.team && !root_metadata.goal);
    let calls = f.calls();
    let db = storage::Database::open(f.boot.storage_config()).unwrap();
    let before: BTreeMap<_, _> = inventory
        .iter()
        .map(|member| {
            (
                member.run,
                db.run_context(&member.run.to_string()).unwrap().unwrap(),
            )
        })
        .collect();
    let observed = f.source.collect().await.unwrap();
    if switch_choice {
        assert_eq!(
            observed.recipes[&root]
                .descriptor
                .as_ref()
                .unwrap()
                .model_preference
                .as_ref(),
            Some(&selection)
        );
    }
    assert!(observed.bindings.is_current());
    assert_eq!(observed.recipes.len(), 2);
    f.boot
        .runtime
        .prepare_restart_quiesce(&observed.recipes)
        .unwrap();
    for run in [root, child] {
        let recipe = &observed.recipes[&run];
        assert_eq!(recipe.policy, ResumePolicy::CoreOwned);
        assert_eq!(recipe.settings_reference, CORE_RECOVERY_SETTINGS_REFERENCE);
        let record = db.run_context(&run.to_string()).unwrap().unwrap();
        let saved = &before[&run];
        assert_eq!(record.messages_json, saved.messages_json);
        assert_eq!(record.config_json, saved.config_json);
        assert_eq!(record.checkpoints_json, saved.checkpoints_json);
        let original: RunRestoreDescriptor = serde_json::from_str(&record.config_json).unwrap();
        assert_eq!(recipe.descriptor.as_ref(), Some(&original));
        assert_eq!(recipe.lane, scope);
        assert_eq!(recipe.team, None);
        assert_eq!(recipe.goal_id, None);
    }
    assert_eq!(f.calls(), calls, "recipe evidence must not activate runs");
    // A normal child parks before executing its returned tool. Recovery uses
    // current binding/config authority, never the saved descriptor as authority.
    f.boot
        .runtime
        .begin_planned_restart_quiesce(&observed.recipes)
        .unwrap();
    f.model
        .block_child
        .store(false, std::sync::atomic::Ordering::SeqCst);
    f.model.release.add_permits(1);
    f.boot.runtime.wait_quiesced().await.unwrap();
    let fence = f.boot.runtime.fence_dispatch_restart().await.unwrap();
    f.boot.runtime.drain_dispatch_tails(&fence).await.unwrap();
    let records = f
        .boot
        .runtime
        .capture_recovery_records(&fence, &observed.recipes)
        .unwrap();
    let manifests = f
        .boot
        .runtime
        .save_recovery_graph("ordinary", &records)
        .unwrap();
    assert_eq!(manifests.len(), if is_lane { 2 } else { 1 });
    // Logical source and target coexist in this unit test. Take a consistent
    // database snapshot so target incarnations cannot fence source cleanup.
    let recovery_config = storage::StorageConfig {
        db_path: f._dir.path().join("recovered.db"),
        ..f.boot.storage_config().clone()
    };
    rusqlite::Connection::open(&f.boot.storage_config().db_path)
        .unwrap()
        .execute(
            "VACUUM INTO ?1",
            [recovery_config.db_path.to_str().unwrap()],
        )
        .unwrap();
    let recovered_storage = storage::Storage::open(recovery_config.clone()).unwrap();
    let bus = Arc::new(event_bus::EventBus::new(256));
    let restarted = runtime::AgentRuntime::with_sandbox_factory(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        f.model.clone(),
        Arc::new(model::Direct),
    )
    .with_run_store(runtime::RunStore::open(&recovery_config, recovered_storage.handle()).unwrap());
    restarted.install_planned_recovery_fixture().unwrap();
    let ownership = f._dir.sockets();
    let leases = storage::shared_with_wait(
        &recovery_config.db_path,
        ownership,
        std::time::Duration::ZERO,
    )
    .unwrap();
    let startup = leases
        .recovery_startup_complete(&recovery_config.db_path, ownership)
        .unwrap();
    let plan = restarted.recovery_plan().unwrap();
    assert_eq!(plan.components.len(), 1, "{plan:?}");
    let component = plan
        .components
        .iter()
        .find(|c| c.runs.contains(&root))
        .unwrap();
    let mut paused = restarted
        .prepare_component(component, startup, ownership)
        .unwrap();
    restarted
        .restore_relations_and_continuations(&mut paused)
        .unwrap();
    let mut projects = workbench_host::projects::StaticProjects::default();
    projects.insert(
        &workbench_protocol::ids::ProjectId::new("recipe-project").unwrap(),
        f._dir.path().canonicalize().unwrap(),
    );
    let bindings =
        workbench_host::bootstrap::recovery::bindings::load(&recovery_config, &projects).unwrap();
    let authority = workbench_host::bootstrap::recovery::authority::CurrentAuthorityProvider {
        runtime: &restarted,
        bindings: &bindings,
        storage: &recovery_config,
        writer: &recovered_storage.handle(),
        config_dir: f._dir.path(),
    }
    .resolve_current(&records, &std::collections::BTreeSet::from([root, child]))
    .unwrap();
    restarted.bind_thread_root(&thread, root).unwrap();
    let reservation = restarted
        .reserve_automatic_activation(&component.manifest_id)
        .unwrap();
    assert_eq!(
        f.calls(),
        calls,
        "preparation and reservation never invoke the model"
    );
    let (activated, publication) = restarted
        .activate_component(paused, authority, reservation)
        .unwrap();
    assert_eq!(
        activated
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
        [root, child].into_iter().collect()
    );
    let mut activation_events = restarted.event_bus().subscribe();
    f.model
        .block_parent
        .store(is_lane, std::sync::atomic::Ordering::SeqCst);
    publication.release();
    let phase = restarted.wait(child).await.unwrap();
    loop {
        let event = activation_events.recv().await.unwrap();
        if matches!(&event.kind, EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {run_id, to, ..}) if run_id == &child.to_string() && *to == phase)
        {
            assert_eq!(phase, event_bus::AgentRunPhase::Done, "{:?}", event.kind);
            break;
        }
    }
    assert!(f.calls() > calls);
    if switch_choice {
        restarted
            .send_message(root, "internal wake keeps the applied model".into())
            .unwrap();
        loop {
            if matches!(activation_events.recv().await.unwrap().kind, EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to: AgentRunPhase::Waiting, .. }) if run_id == root.to_string())
            {
                break;
            }
        }
        let input = f
            .model
            .inner
            .1
            .lock()
            .unwrap()
            .get(&root.to_string())
            .unwrap()
            .clone();
        assert_eq!(input.request.model, "switched-fixture");
        assert_eq!(input.request.reasoning_effort.as_deref(), Some("high"));
    }
    if is_lane {
        // Child completion wakes the lane. Hold that actual provider turn so
        // recollection observes a stable live run, not an intermediate snapshot.
        f.model.parent_entered.notified().await;
        // Startup also uses goal_projects as project metadata, not evidence of
        // an actual goal. Neither this metadata nor absent GUI bindings blocks
        // a subsequent recipe collection.
        f.boot
            .host
            .lock()
            .unwrap()
            .service_mut()
            .bindings
            .goal_projects
            .insert(thread.clone(), "project".into());
        let source = HostRecipeSource {
            runtime: restarted.clone(),
            host: f.boot.host.clone(),
            storage: recovery_config.clone(),
            reader: f.source.reader.clone(),
        };
        let recollected = source.collect().await.unwrap();
        assert_eq!(recollected.recipes[&root].lane, scope);
        // The fixture retains a separate stopped-lead manifest for disposal;
        // check live-member preflight independently of that adopted-tx gate.
        restarted
            .preflight_restart_capture(&recollected.recipes)
            .unwrap();
        f.model
            .block_parent
            .store(false, std::sync::atomic::Ordering::SeqCst);
        f.model.parent_release.add_permits(1);
    }
    restarted.stop(root, runtime::StopScope::Subtree).unwrap();
    restarted.wait(root).await.unwrap();
    drop(restarted);
    recovered_storage.close();
    f.shutdown().await;
}

#[tokio::test]
async fn no_runs_is_empty_but_unavailable_storage_is_not() {
    let mut f = Fixture::new().await;
    assert!(f.source.collect().await.unwrap().recipes.is_empty());
    f.source.storage.db_path = f._dir.path().join("does-not-exist");
    assert_eq!(
        f.source.collect().await.err().unwrap().code,
        ErrorCode::Busy
    );
    assert!(!f.source.storage.db_path.exists());
    f.shutdown().await;
}

#[tokio::test]
async fn host_goal_binding_rejects_instead_of_becoming_none() {
    let mut f = Fixture::new().await;
    let root = f.chat().await;
    f.boot.host.lock().unwrap().service_mut().bind_goal_context(
        "plain",
        "project",
        &root.to_string(),
    );
    f.rejected("goal").await;
    f.shutdown().await;
}

#[tokio::test]
async fn missing_binding_never_defaults_to_core_owned() {
    let mut f = Fixture::new().await;
    let root = f.chat().await;
    {
        let mut host = f.boot.host.lock().unwrap();
        let service = host.service_mut();
        service.bindings.chat_runs.clear();
        service.publish_lane_bindings();
    }
    f.rejected("binding").await;
    {
        let mut host = f.boot.host.lock().unwrap();
        let service = host.service_mut();
        service.bindings.chat_runs.insert("plain".into(), root);
        service.publish_lane_bindings();
    }
    assert_eq!(f.source.collect().await.unwrap().recipes.len(), 1);
    f.shutdown().await;
}

#[tokio::test]
async fn corrupt_or_missing_descriptor_does_not_disappear_from_the_map() {
    let mut f = Fixture::new().await;
    let run = f.chat().await;
    let conn = rusqlite::Connection::open(&f.boot.storage_config().db_path).unwrap();
    conn.execute(
        "UPDATE run_contexts SET config_json='not-json' WHERE run_id=?1",
        [run.to_string()],
    )
    .unwrap();
    // Storage's row decoder rejects malformed JSON before descriptor decoding.
    f.rejected("source unavailable").await;
    conn.pragma_update(None, "foreign_keys", false).unwrap();
    conn.execute(
        "DELETE FROM run_contexts WHERE run_id=?1",
        [run.to_string()],
    )
    .unwrap();
    f.rejected("missing snapshot").await;
    f.shutdown().await;
}

#[tokio::test]
async fn saved_lane_team_and_changed_snapshot_reject_even_when_live_metadata_is_plain() {
    let mut f = Fixture::new().await;
    let run = f.chat().await;
    let record = storage::Database::open(f.boot.storage_config())
        .unwrap()
        .run_context(&run.to_string())
        .unwrap()
        .unwrap();
    let original: RunRestoreDescriptor = serde_json::from_str(&record.config_json).unwrap();
    for kind in [
        "lane",
        "team",
        "descriptor mismatch",
        "project mismatch",
        "model mismatch",
        "missing or changed snapshot",
    ] {
        let mut descriptor = original.clone();
        match kind {
            "lane" => descriptor.lane_scope = Some(event_bus::LaneScope::Lane),
            "team" => {
                descriptor.renewable_team = Some(runtime::restore::TeamRestoreIdentity {
                    team_id: "unreconciled".into(),
                    coordinator_run_id: run,
                })
            }
            "descriptor mismatch" => descriptor.thread_id = Some("different".into()),
            "project mismatch" => descriptor.project_root = Some("/different-project".into()),
            "model mismatch" => {
                descriptor.model_preference = Some(runtime::ModelPreference {
                    profile: "different".into(),
                    model: None,
                    reasoning_effort: None,
                })
            }
            _ => descriptor.name = Some("changed snapshot".into()),
        }
        let mut changed = record.clone();
        if kind == "missing or changed snapshot" {
            changed.updated_at_ns = changed.updated_at_ns.checked_add(1).unwrap();
        }
        changed.config_json = serde_json::to_string(&descriptor).unwrap();
        f.boot
            .storage_writer()
            .upsert_run_context_receipt(&changed)
            .unwrap();
        let live = &f.inventory()[0];
        assert!(!live.lane && !live.team && !live.goal);
        f.rejected(if kind == "project mismatch" || kind == "model mismatch" {
            "descriptor mismatch"
        } else {
            kind
        })
        .await;
    }
    f.shutdown().await;
}

#[tokio::test]
async fn lane_ledger_alone_rejects_a_plain_live_graph() {
    let mut f = Fixture::new().await;
    let root = f.chat().await;
    // A ledger-only fixture deliberately leaves runtime/snapshot scope plain.
    let sql = rusqlite::Connection::open(&f.boot.storage_config().db_path).unwrap();
    sql.execute(
        "UPDATE threads SET lineage_kind=NULL WHERE thread_id='plain'",
        [],
    )
    .unwrap();
    f.boot
        .storage_writer()
        .create_lane(storage::lanes::NewLane {
            lane_id: "ledger-only".into(),
            thread_id: "plain".into(),
            origin_thread_id: "origin".into(),
            origin_run_id: root.to_string(),
            creation_key: "creation".into(),
            brief: event_bus::LaneBrief {
                verification_requirements: Default::default(),
                objective: "ledger-only classification".into(),
                inputs: Vec::new(),
                deliverables: vec!["result".into()],
                acceptance_criteria: vec!["checked".into()],
                constraints: Vec::new(),
                workspace: Default::default(),
                report_policy: event_bus::LanePolicy::Detached,
            },
            start_seed: "seed".into(),
            project_id: None,
            project_root: None,
            title: None,
            at_ns: 1,
        })
        .unwrap();
    assert!(!f.inventory()[0].lane);
    f.rejected("lane").await;
    // Undo the deliberately inconsistent ledger before normal durable Stop.
    sql.execute("DELETE FROM lanes WHERE lane_id='ledger-only'", [])
        .unwrap();
    f.shutdown().await;
}

#[tokio::test]
async fn storage_read_completes_without_reacquiring_command_host_mutex() {
    use std::{future::Future, task::Poll};
    let mut f = Fixture::new().await;
    let lead = f.chat().await;
    f.lane(lead, event_bus::LanePolicy::Attached).await;
    {
        let barrier = queued_reader_barrier(f.source.reader.clone()).await;
        barrier.entered.await.unwrap();
        let collection = f.source.collect();
        tokio::pin!(collection);
        std::future::poll_fn(|context| {
            assert!(collection.as_mut().poll(context).is_pending());
            Poll::Ready(())
        })
        .await;
        let host = f.boot.host.clone();
        let (locked, ready) = std::sync::mpsc::channel();
        let (release, finished) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let guard = host.try_lock().expect("collection retained host mutex");
            locked.send(()).unwrap();
            finished.recv().unwrap();
            drop(guard);
        });
        ready.recv().unwrap();
        barrier.release.send(()).unwrap();
        barrier.job.await.unwrap();
        let result = collection.await;
        release.send(()).unwrap();
        holder.join().unwrap();
        assert!(!result.unwrap().recipes.is_empty());
    }
    f.shutdown().await;
}

#[tokio::test]
async fn collected_binding_observation_expires_even_when_the_binding_returns_to_the_same_root() {
    let mut f = Fixture::new().await;
    let root = f.chat().await;
    let observed = f.source.collect().await.unwrap();
    {
        let mut host = f.boot.host.lock().unwrap();
        let service = host.service_mut();
        service.bindings.chat_runs.remove("plain");
        service.publish_lane_bindings();
        service.bindings.chat_runs.insert("plain".into(), root);
        service.publish_lane_bindings();
    }
    assert!(!observed.bindings.is_current());
    assert!(f.source.collect().await.unwrap().bindings.is_current());
    f.shutdown().await;
}

#[tokio::test]
async fn live_lane_team_and_internal_classifications_are_copied_and_rejected() {
    for (kind, config) in [
        (
            "lane",
            RunConfig {
                conversation: true,
                lane_scope: Some(event_bus::LaneScope::Lane),
                ..Default::default()
            },
        ),
        (
            "team",
            RunConfig {
                conversation: true,
                topology: runtime::CoordinationTopology::DynamicTeam { max_workers: 2 },
                delegation_value: Some("coordinate".into()),
                ..Default::default()
            },
        ),
        (
            "internal run",
            RunConfig {
                conversation: true,
                learning_internal: true,
                ..Default::default()
            },
        ),
    ] {
        let mut f = Fixture::new().await;
        f.configured_chat(config).await;
        let member = &f.inventory()[0];
        assert!(member.parent.is_none() && member.conversation);
        assert_eq!(member.thread.as_deref(), Some("plain"));
        assert_eq!(member.lane, kind == "lane");
        assert_eq!(member.team, kind == "team");
        assert_eq!(member.internal, kind == "internal run");
        f.rejected(kind).await;
        f.shutdown().await;
    }
}

#[tokio::test]
async fn runtime_goal_is_not_hidden_by_plain_host_binding() {
    let mut f = Fixture::new().await;
    let root = f.chat().await;
    f.boot
        .runtime
        .create_thread_goal("plain", root, "objective".into(), vec!["criterion".into()])
        .unwrap();
    assert!(f.inventory().iter().find(|m| m.run == root).unwrap().goal);
    f.rejected("goal").await;
    f.shutdown().await;
}

#[tokio::test]
async fn cli_like_non_conversation_root_is_not_promoted_by_a_binding() {
    let mut f = Fixture::new().await;
    f.configured_chat(RunConfig::default()).await;
    assert!(!f.inventory()[0].conversation);
    f.rejected("non-conversation root").await;
    f.shutdown().await;
}

#[tokio::test]
async fn one_special_child_rejects_the_whole_graph_without_partial_recipes() {
    let mut f = Fixture::new().await;
    let root = f.chat().await;
    let child = f
        .boot
        .runtime
        .delegate_background_as_child(
            root,
            Role::Worker,
            "lane worker".to_string(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                lane_scope: Some(event_bus::LaneScope::Lane),
                ..Default::default()
            },
        )
        .unwrap();
    f.waiting(child).await;
    assert!(f.inventory().iter().find(|m| m.run == child).unwrap().lane);
    f.rejected("lane").await;
    f.shutdown().await;
}

// Keep the reader blocked until the caller explicitly releases it. Therefore a
// first poll cannot complete based on an OS thread winning a scheduling race.
struct ReaderBarrier {
    job: AdminIo<'static, ()>,
    entered: tokio::sync::oneshot::Receiver<()>,
    release: std::sync::mpsc::Sender<()>,
}
async fn queued_reader_barrier(reader: Arc<AdminReader>) -> ReaderBarrier {
    use std::task::Poll;
    let (enter, entered) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let mut job: AdminIo<'static, ()> = Box::pin(async move {
        reader
            .read(move || {
                enter.send(()).unwrap();
                released.recv().unwrap();
                Ok(())
            })
            .await
    });
    std::future::poll_fn(|cx| {
        assert!(job.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    ReaderBarrier {
        job,
        entered,
        release,
    }
}

fn lane_brief(policy: event_bus::LanePolicy) -> event_bus::LaneBrief {
    event_bus::LaneBrief {
        objective: "restart a GUI-unopened lane".into(),
        inputs: vec![],
        deliverables: vec!["result".into()],
        acceptance_criteria: vec!["checked".into()],
        constraints: vec![],
        workspace: Default::default(),
        report_policy: policy,
        verification_requirements: Default::default(),
    }
}

#[tokio::test]
async fn unopened_lanes_and_their_children_use_current_ledger_not_chat_bindings() {
    let mut f = Fixture::new().await;
    let lead = f.chat().await;
    let mut expected = vec![lead];
    for policy in [
        event_bus::LanePolicy::Attached,
        event_bus::LanePolicy::Summary,
        event_bus::LanePolicy::Detached,
    ] {
        let lane = f.lane(lead, policy).await;
        let child = f
            .boot
            .runtime
            .delegate_background_as_child(
                lane.root_run_id,
                Role::Worker,
                "lane child",
                RunConfig {
                    interactive: true,
                    keep_alive: true,
                    ..Default::default()
                },
            )
            .unwrap();
        f.waiting(child).await;
        assert!(
            !f.source
                .bindings()
                .unwrap()
                .chats
                .contains_key(&lane.thread_id)
        );
        expected.extend([lane.root_run_id, child]);
    }
    let observed = f.source.collect().await.unwrap();
    assert_eq!(
        observed
            .recipes
            .keys()
            .copied()
            .collect::<std::collections::BTreeSet<_>>(),
        expected.iter().copied().collect()
    );
    for run in expected {
        let recipe = &observed.recipes[&run];
        assert_eq!(recipe.lane.is_some(), run != lead);
        assert_eq!(recipe.descriptor.as_ref().unwrap().lane_scope, recipe.lane);
    }
    f.boot
        .runtime
        .prepare_restart_quiesce(&observed.recipes)
        .unwrap();
    f.shutdown().await;
}

#[tokio::test]
async fn lane_ledger_scope_and_snapshot_refusals_keep_the_cause_run() {
    let mut f = Fixture::new().await;
    let lead = f.chat().await;
    let lane = f.lane(lead, event_bus::LanePolicy::Attached).await;
    let conn = rusqlite::Connection::open(&f.boot.storage_config().db_path).unwrap();
    conn.execute(
        "UPDATE lanes SET root_run_id=?1 WHERE lane_id=?2",
        rusqlite::params![lead.to_string(), &lane.lane_id],
    )
    .unwrap();
    let failure = f.source.collect().await.err().unwrap();
    assert!(
        failure.message.contains(&lane.root_run_id.to_string()),
        "{failure:?}"
    );
    assert!(failure.message.contains("ledger mismatch"), "{failure:?}");
    conn.execute(
        "UPDATE lanes SET root_run_id=?1 WHERE lane_id=?2",
        rusqlite::params![lane.root_run_id.to_string(), &lane.lane_id],
    )
    .unwrap();
    conn.pragma_update(None, "foreign_keys", false).unwrap();
    conn.execute(
        "DELETE FROM run_contexts WHERE run_id=?1",
        [lane.root_run_id.to_string()],
    )
    .unwrap();
    let failure = f.source.collect().await.err().unwrap();
    assert!(
        failure.message.contains(&lane.root_run_id.to_string())
            && failure.message.contains("snapshot"),
        "{failure:?}"
    );
    f.shutdown().await;
}

#[tokio::test]
async fn archive_round_trip_invalidates_commit_ledger_even_when_root_is_unchanged() {
    let mut f = Fixture::new().await;
    let run = f.chat().await;
    let observed = f.source.collect().await.unwrap();
    for (archive_revision, archived) in [(1, true), (2, false)] {
        f.boot
            .storage_writer()
            .upsert_threads_host(&[storage::threads::ThreadUpsert {
                thread_id: "plain".into(),
                root_run_id: Some(run.to_string()),
                archived: Some(archived),
                archive_revision,
                ..Default::default()
            }])
            .unwrap();
    }
    let error = f.source.verify_ledgers(observed.ledgers).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::Conflict);
    assert!(error.message.contains("archive ledger changed"));
    assert!(error.message.contains(&run.to_string()));
    f.shutdown().await;
}

#[tokio::test]
async fn lane_ledger_change_between_discovery_and_snapshot_validation_is_a_conflict() {
    use std::{future::Future, task::Poll};
    let mut f = Fixture::new().await;
    let lead = f.chat().await;
    let lane = f.lane(lead, event_bus::LanePolicy::Attached).await;
    let first = queued_reader_barrier(f.source.reader.clone()).await;
    first.entered.await.unwrap();
    let collection = f.source.collect();
    tokio::pin!(collection);
    std::future::poll_fn(|cx| {
        assert!(collection.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    // FIFO: both discovery and this barrier are enqueued behind the first
    // blocked job. Discovery cannot finish before the second barrier exists.
    let barrier = queued_reader_barrier(f.source.reader.clone()).await;
    first.release.send(()).unwrap();
    first.job.await.unwrap();
    barrier.entered.await.unwrap();
    rusqlite::Connection::open(&f.boot.storage_config().db_path)
        .unwrap()
        .execute(
            "UPDATE lanes SET root_run_id=?1 WHERE lane_id=?2",
            rusqlite::params![lead.to_string(), &lane.lane_id],
        )
        .unwrap();
    barrier.release.send(()).unwrap();
    barrier.job.await.unwrap();
    let failure = collection.await.err().unwrap();
    assert_eq!(failure.code, ErrorCode::Conflict);
    assert!(
        failure.message.contains(&lane.root_run_id.to_string()),
        "{failure:?}"
    );
    rusqlite::Connection::open(&f.boot.storage_config().db_path)
        .unwrap()
        .execute(
            "UPDATE lanes SET root_run_id=?1 WHERE lane_id=?2",
            rusqlite::params![lane.root_run_id.to_string(), &lane.lane_id],
        )
        .unwrap();
    // The pinned futures borrow f until this scope ends; explicit shutdown is
    // performed below by stopping the runs and joining the bootstrap services.
    f.boot.shutdown().await.unwrap();
}
