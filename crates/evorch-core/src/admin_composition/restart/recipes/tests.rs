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
    model: Arc<model::Model>,
}
impl Fixture {
    async fn new() -> Self {
        let dir = test_socket_dir::SplitDir::new();
        let root = dir.path().canonicalize().unwrap();
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
        let model = Arc::new(model::Model::default());
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
        self.boot.owner.start("plain").unwrap();
        let events = self.boot.host.lock().unwrap().submit_local(
            HostCommand::SendChat(ChatInput {
                submission_id: "first".into(),
                project_root: Some(self._dir.path().canonicalize().unwrap()),
                composer_role: ComposerRole::Orchestrator,
                attachments: Vec::new(),
                thread_id: "plain".into(),
                text: "ordinary conversation".into(),
                skills: Vec::new(),
                model_preference: None,
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
        let binding = self.source.bindings().unwrap();
        let held = binding
            .chats
            .iter()
            .filter_map(|(thread, run)| {
                binding
                    .permits
                    .get(thread)
                    .map(|permit| (*run, permit.clone()))
            })
            .collect();
        self.boot.runtime.restart_recipe_inventory(&held).unwrap()
    }
    async fn configured_chat(&mut self, mut config: RunConfig) -> RunId {
        let permit = self.boot.owner.start("plain").unwrap();
        config.ownership = Some(permit.clone());
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
            service.bindings.chat_permits.insert("plain".into(), permit);
            service.publish_lane_bindings();
        }
        self.waiting(run).await;
        run
    }
    fn calls(&self) -> usize {
        self.model.0.load(std::sync::atomic::Ordering::SeqCst)
    }
    async fn rejected(&self, kind: &str) {
        let calls = self.calls();
        let binding_observation = self.source.bindings().unwrap().observed;
        let owner = self.boot.owner.owned_permit("plain").unwrap();
        let error = self.source.collect().await.err().expect("must reject");
        assert_eq!(
            error.code,
            if kind == "source unavailable" {
                ErrorCode::Busy
            } else {
                ErrorCode::UnsupportedCapability
            }
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
        assert!(same_permit(
            &owner,
            &self.boot.owner.owned_permit("plain").unwrap()
        ));
    }
    async fn shutdown(mut self) {
        self.boot.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn plain_host_conversation_and_normal_child_keep_every_descriptor() {
    let mut f = Fixture::new().await;
    let root = f.chat().await;
    let child = f
        .boot
        .runtime
        .delegate_background_as_child(
            root,
            Role::Worker,
            "ordinary delegated child",
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    f.waiting(child).await;
    let inventory = f.inventory();
    let child_metadata = inventory.iter().find(|m| m.run == child).unwrap();
    assert_eq!(child_metadata.root, root);
    assert_eq!(child_metadata.parent, Some(root));
    assert_eq!(child_metadata.thread.as_deref(), Some("plain"));
    assert!(child_metadata.permit_matches);
    let root_metadata = inventory.iter().find(|m| m.run == root).unwrap();
    assert!(root_metadata.conversation);
    assert!(!root_metadata.lane && !root_metadata.team && !root_metadata.goal);
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
        assert_eq!(recipe.lane, None);
        assert_eq!(recipe.team, None);
        assert_eq!(recipe.goal_id, None);
    }
    assert_eq!(f.calls(), calls, "recipe evidence must not activate runs");
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
async fn missing_binding_or_retained_permit_never_defaults_to_core_owned() {
    let mut f = Fixture::new().await;
    let root = f.chat().await;
    {
        let mut host = f.boot.host.lock().unwrap();
        let service = host.service_mut();
        service.bindings.chat_runs.clear();
        service.publish_lane_bindings();
    }
    assert!(!f.inventory()[0].permit_matches);
    f.rejected("ownership").await;
    {
        let mut host = f.boot.host.lock().unwrap();
        let service = host.service_mut();
        service.bindings.chat_runs.insert("plain".into(), root);
        service.bindings.chat_permits.clear();
        service.publish_lane_bindings();
    }
    f.rejected("ownership").await;
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
    f.shutdown().await;
}

#[tokio::test]
async fn storage_read_completes_without_reacquiring_command_host_mutex() {
    use std::{future::Future, task::Poll};
    let mut f = Fixture::new().await;
    f.chat().await;
    {
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
        assert!(member.parent.is_none() && member.conversation && member.permit_matches);
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
async fn cli_like_non_conversation_root_is_not_promoted_even_with_a_permit() {
    let mut f = Fixture::new().await;
    f.configured_chat(RunConfig::default()).await;
    assert!(!f.inventory()[0].conversation);
    f.rejected("non-conversation root").await;
    f.shutdown().await;
}

#[tokio::test]
async fn foreign_retained_permit_cannot_match_live_runtime_metadata() {
    let mut f = Fixture::new().await;
    f.chat().await;
    {
        let mut host = f.boot.host.lock().unwrap();
        let service = host.service_mut();
        service
            .bindings
            .chat_permits
            .get_mut("plain")
            .unwrap()
            .lease
            .owner_id = "foreign".into();
        service.publish_lane_bindings();
    }
    assert!(!f.inventory()[0].permit_matches);
    f.rejected("ownership").await;
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
