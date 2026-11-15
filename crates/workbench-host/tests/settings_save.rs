//! The `settings` area through the production command path: no lost update,
//! the protected restart targets, and a stalled reload that stops nobody.
mod common;

use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use workbench_host::{
    CommandHost,
    areas::{
        AreaTasks, Areas,
        settings::{SettingsArea, SettingsParts},
    },
    commands::CommandService,
};
use workbench_protocol::{
    WorkbenchError,
    commands::{Command, CommandDisposition},
    errors::ErrorCode,
    events::{Subscribe, SubscriptionScope, UiEvent},
    ids::{OperationId, SessionId},
    namespaces::settings::*,
    operation::OperationState,
    queries::{Query, QueryReply},
};

const TARGET: &str = "[core.restart.targets.web]\npath = '/usr/bin/true'\n";
const PROVIDER: &str = "[providers.local]\n\
type = 'openai-compatible'\n\
base_url = 'https://llm.example.invalid/v1'\n\
api_key_env = 'EVORCH_TEST_ABSENT_KEY'\n\
models = ['m']\n\
default_model = 'm'\n";

fn op(id: &str) -> OperationId {
    OperationId::new(id).unwrap()
}

/// A reload that can be held, as a model composition waiting on the network.
#[derive(Default)]
struct Reload {
    held: Mutex<bool>,
    released: Condvar,
    calls: AtomicUsize,
    entered: tokio::sync::Notify,
    /// The configuration the hook composed from: the file as the hook reads it
    /// when it gets going again after being held.
    composed: Mutex<Vec<String>>,
}

struct Fixture {
    dir: tempfile::TempDir,
    host: CommandHost,
    settings: SettingsArea,
    reload: Arc<Reload>,
}

impl Fixture {
    fn new(initial: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), initial).unwrap();
        let reload = Arc::new(Reload::default());
        let hook = reload.clone();
        let config_path = dir.path().join("config.toml");
        let reload_models: workbench_host::areas::ReloadModels = Arc::new(move || {
            hook.calls.fetch_add(1, Ordering::SeqCst);
            hook.entered.notify_one();
            let mut held = hook.held.lock().unwrap();
            while *held {
                held = hook.released.wait(held).unwrap();
            }
            hook.composed
                .lock()
                .unwrap()
                .push(std::fs::read_to_string(&config_path).unwrap());
            Ok(())
        });
        let service = CommandService::new(common::runtime(), tokio::runtime::Handle::current());
        let host = CommandHost::new(service);
        let source = host.projection().source("settings").unwrap();
        let settings = SettingsArea::with_parts(SettingsParts {
            config_dir: dir.path().to_path_buf(),
            startup: Arc::new(config::Config::default()),
            reload_models,
            tasks: AreaTasks::new(tokio::runtime::Handle::current()),
            source,
        });
        let host = host.with_areas(Areas {
            settings: settings.clone(),
            ..Default::default()
        });
        Self {
            dir,
            host,
            settings,
            reload,
        }
    }

    fn file(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("config.toml")).unwrap()
    }

    fn snapshot(&self) -> SettingsSnapshot {
        match self
            .host
            .handle_query(
                &SessionId::new("session").unwrap(),
                &Query::Settings(SettingsQuery::GetSettingsSnapshot {}),
            )
            .unwrap()
        {
            QueryReply::Settings(SettingsReply::Settings(snapshot)) => *snapshot,
            other => panic!("{other:?}"),
        }
    }

    fn run(&mut self, command: SettingsCommand) -> Result<CommandDisposition, WorkbenchError> {
        let reply = self
            .host
            .handle_command(Command::Settings(command), common::web())?;
        Ok(reply.disposition)
    }

    async fn finish(&mut self, id: &str, command: SettingsCommand) -> OperationState {
        self.run(command).unwrap();
        self.settings
            .wait_operation(&op(id))
            .await
            .unwrap()
            .operation
            .state
    }

    async fn save_document(&mut self, id: &str, text: &str) -> OperationState {
        let expected = self.snapshot().revision;
        self.finish(
            id,
            SettingsCommand::SaveDocument(SaveDocument {
                operation_id: op(id),
                expected_revision: expected,
                text: text.to_owned(),
            }),
        )
        .await
    }
}

fn provider(name: &str, default_model: &str) -> ProviderInput {
    ProviderInput::OpenaiCompatible {
        name: name.into(),
        kind: OpenaiCompatibleKind::OpenaiCompatible,
        base_url: "https://llm.example.invalid/v1".into(),
        credential: CredentialRef::Env {
            var: "EVORCH_TEST_ABSENT_KEY".into(),
        },
        models: vec![ModelInput {
            id: default_model.into(),
            enabled: true,
        }],
        excluded_models: vec![],
        default_model: default_model.into(),
    }
}

fn save_provider(id: &str, expected: SettingsRevision, input: ProviderInput) -> SettingsCommand {
    SettingsCommand::SaveProvider(Box::new(SaveProvider {
        operation_id: op(id),
        expected_revision: expected,
        original_name: None,
        provider: input,
    }))
}

fn denied() -> OperationState {
    OperationState::Failed {
        code: ErrorCode::PermissionDenied,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn two_saves_of_one_revision_end_as_one_success_and_one_stale() {
    let mut f = Fixture::new("version = 3\n");
    let revision = f.snapshot().revision;
    f.run(save_provider(
        "first",
        revision.clone(),
        provider("one", "a"),
    ))
    .unwrap();
    f.run(save_provider("second", revision, provider("two", "b")))
        .unwrap();
    let first = f.settings.wait_operation(&op("first")).await.unwrap();
    let second = f.settings.wait_operation(&op("second")).await.unwrap();
    let mut states = [first.operation.state, second.operation.state];
    states.sort_by_key(|state| format!("{state:?}"));
    assert_eq!(
        states,
        [
            OperationState::Failed {
                code: ErrorCode::StaleRevision
            },
            OperationState::Ready
        ]
    );
    // Exactly one of the two providers is in the file: no update was lost
    // silently.
    let file = f.file();
    assert_eq!(
        file.contains("[providers.one]") as u8 + file.contains("[providers.two]") as u8,
        1,
        "{file}"
    );
    let snapshot = f.snapshot();
    assert_eq!(snapshot.providers.len(), 1);
    assert_ne!(snapshot.revision, snapshot.applied_revision);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_same_operation_id_is_answered_once() {
    let mut f = Fixture::new("version = 3\n");
    let revision = f.snapshot().revision;
    let command = save_provider("save-1", revision.clone(), provider("one", "a"));
    assert_eq!(
        f.run(command.clone()).unwrap(),
        CommandDisposition::Accepted
    );
    f.settings.wait_operation(&op("save-1")).await.unwrap();
    assert_eq!(f.run(command).unwrap(), CommandDisposition::AlreadyApplied);
    // The same id for another request is refused, not re-run.
    let other = save_provider("save-1", revision, provider("two", "b"));
    assert_eq!(f.run(other).unwrap_err().code, ErrorCode::Conflict);
    assert!(!f.file().contains("[providers.two]"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_save_is_not_applied_until_the_models_are_reloaded() {
    let mut f = Fixture::new("version = 3\n");
    let revision = f.snapshot().revision;
    let saved = f
        .finish(
            "save-1",
            save_provider("save-1", revision, provider("one", "a")),
        )
        .await;
    assert_eq!(saved, OperationState::Ready);
    assert_eq!(f.reload.calls.load(Ordering::SeqCst), 0);
    let snapshot = f.snapshot();
    assert_ne!(snapshot.revision, snapshot.applied_revision);

    let reloaded = f
        .finish(
            "reload-1",
            SettingsCommand::ReloadModels(ReloadModels {
                operation_id: op("reload-1"),
            }),
        )
        .await;
    assert_eq!(reloaded, OperationState::Ready);
    assert_eq!(f.reload.calls.load(Ordering::SeqCst), 1);
    let snapshot = f.snapshot();
    assert_eq!(snapshot.revision, snapshot.applied_revision);
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_targets_cannot_be_changed_by_a_whole_document_save() {
    let base = format!("version = 3\n{TARGET}{PROVIDER}");
    let mut f = Fixture::new(&base);
    let cases = [
        // removed: the section is gone
        ("omitted", format!("version = 3\n{PROVIDER}")),
        (
            "removed",
            format!("version = 3\n[core]\nidle_grace_secs = 9\n{PROVIDER}"),
        ),
        (
            "path-changed",
            format!("version = 3\n[core.restart.targets.web]\npath = '/bin/sh'\n{PROVIDER}"),
        ),
        (
            "renamed",
            format!(
                "version = 3\n[core.restart.targets.other]\npath = '/usr/bin/true'\n{PROVIDER}"
            ),
        ),
        (
            "added",
            format!(
                "version = 3\n{TARGET}[core.restart.targets.extra]\npath = '/bin/ls'\n{PROVIDER}"
            ),
        ),
    ];
    for (name, text) in cases {
        assert_eq!(f.save_document(name, &text).await, denied(), "{name}");
        assert_eq!(f.file(), base, "{name} reached the file");
    }

    // A save that leaves them alone goes through, and keeps them.
    let kept = format!("version = 3\n[core]\nidle_grace_secs = 9\n{TARGET}{PROVIDER}");
    assert_eq!(f.save_document("kept", &kept).await, OperationState::Ready);
    assert!(f.file().contains("path = '/usr/bin/true'"), "{}", f.file());
    assert!(f.file().contains("idle_grace_secs = 9"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_partial_save_leaves_the_restart_targets_as_they_are() {
    let base = format!("version = 3\n{TARGET}{PROVIDER}");
    let mut f = Fixture::new(&base);
    let revision = f.snapshot().revision;
    let state = f
        .finish(
            "provider-1",
            save_provider("provider-1", revision, provider("added", "x")),
        )
        .await;
    assert_eq!(state, OperationState::Ready);
    let file = f.file();
    assert!(file.contains("[providers.added]"), "{file}");
    assert!(file.contains("[core.restart.targets.web]"), "{file}");
    assert!(file.contains("path = '/usr/bin/true'"), "{file}");

    let revision = f.snapshot().revision;
    let state = f
        .finish(
            "delete-1",
            SettingsCommand::DeleteProvider(DeleteProvider {
                operation_id: op("delete-1"),
                expected_revision: revision,
                name: "added".into(),
            }),
        )
        .await;
    assert_eq!(state, OperationState::Ready);
    assert!(f.file().contains("path = '/usr/bin/true'"));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_document_is_refused_without_echoing_its_secrets() {
    let mut f = Fixture::new("version = 3\n");
    let state = f
        .save_document(
            "bad-1",
            "version = 3\n[providers.x]\napi_key = 'SENTINEL-d41d8cd98f00b204'\n",
        )
        .await;
    assert!(matches!(state, OperationState::Failed { .. }), "{state:?}");
    let view = f.settings.operation(&op("bad-1")).unwrap();
    let wire = serde_json::to_string(&view).unwrap();
    assert!(!wire.contains("SENTINEL"), "{wire}");
    assert_eq!(f.file(), "version = 3\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_reload_stops_no_client_but_the_saves_wait_for_it() {
    let mut f = Fixture::new("version = 3\n");
    *f.reload.held.lock().unwrap() = true;
    f.run(SettingsCommand::ReloadModels(ReloadModels {
        operation_id: op("reload-1"),
    }))
    .unwrap();
    f.reload.entered.notified().await;

    // The reload is stuck: reads answer, and a save is accepted at once (the
    // handler does not wait) but does not run before the reload ends.
    assert_eq!(
        f.settings
            .operation(&op("reload-1"))
            .unwrap()
            .operation
            .state,
        OperationState::Pending
    );
    let revision = f.snapshot().revision;
    f.run(save_provider("save-1", revision, provider("one", "a")))
        .unwrap();
    assert_eq!(
        f.settings.operation(&op("save-1")).unwrap().operation.state,
        OperationState::Pending
    );
    assert_eq!(f.settings.operations().unwrap().len(), 2);
    assert_eq!(f.file(), "version = 3\n", "the save waits for the reload");

    *f.reload.held.lock().unwrap() = false;
    f.reload.released.notify_all();
    for id in ["reload-1", "save-1"] {
        assert_eq!(
            f.settings
                .wait_operation(&op(id))
                .await
                .unwrap()
                .operation
                .state,
            OperationState::Ready
        );
    }
    assert!(f.file().contains("[providers.one]"));
}

/// The newest snapshot announcement once `completed` operations have ended.
/// A snapshot is announced before the operation that caused it completes, so
/// nothing later can follow the last completion.
async fn last_announced(
    events: &mut workbench_host::subscriptions::Subscription,
    completed: usize,
) -> Option<(SettingsRevision, SettingsRevision)> {
    let (mut last, mut done) = (None, 0);
    while done < completed {
        match events.recv().await.expect("the subscription is open").event {
            UiEvent::Settings(SettingsEvent::SettingsSnapshotChanged {
                revision,
                applied_revision,
            }) => last = Some((revision, applied_revision)),
            UiEvent::Settings(SettingsEvent::SettingsOperationCompleted { .. }) => done += 1,
            _ => {}
        }
    }
    last
}

fn subscribe(f: &Fixture) -> workbench_host::subscriptions::Subscription {
    f.host
        .projection()
        .subscribe_with_snapshot(&Subscribe {
            scope: SubscriptionScope::All,
            after: None,
        })
        .unwrap()
}

/// A save cannot land while the hook composes: what the hook composed from is
/// the file whose revision is marked applied, the hook runs once, and the saves
/// that waited then run on top (A, then B written during the reload, then A
/// again: the revision alone could not tell the first A from the last).
#[tokio::test(flavor = "multi_thread")]
async fn what_the_hook_composed_is_what_is_marked_applied_whatever_is_saved_meanwhile() {
    let base = format!("version = 3\n{PROVIDER}");
    let mut f = Fixture::new(&base);
    let mut events = subscribe(&f);
    let first = f.snapshot().revision;

    *f.reload.held.lock().unwrap() = true;
    f.run(SettingsCommand::ReloadModels(ReloadModels {
        operation_id: op("reload-1"),
    }))
    .unwrap();
    f.reload.entered.notified().await;
    let changed = format!("{base}# changed meanwhile\n");
    f.run(SettingsCommand::SaveDocument(SaveDocument {
        operation_id: op("save-b"),
        expected_revision: first.clone(),
        text: changed.clone(),
    }))
    .unwrap();
    *f.reload.held.lock().unwrap() = false;
    f.reload.released.notify_all();
    f.settings.wait_operation(&op("reload-1")).await.unwrap();

    // Composed once, from the file as it was when the reload began, and that
    // is the revision it marked applied.
    assert_eq!(f.reload.calls.load(Ordering::SeqCst), 1);
    assert_eq!(*f.reload.composed.lock().unwrap(), vec![base.clone()]);
    f.settings.wait_operation(&op("save-b")).await.unwrap();
    let snapshot = f.snapshot();
    assert_eq!(f.file(), changed);
    assert_ne!(snapshot.revision, first);
    assert_eq!(snapshot.applied_revision, first, "B was never composed");

    // Back to A: the saved file is what was composed, and is shown applied.
    assert_eq!(
        f.save_document("save-a", &base).await,
        OperationState::Ready
    );
    let snapshot = f.snapshot();
    assert_eq!(snapshot.revision, first);
    assert_eq!(snapshot.applied_revision, first);
    assert_eq!(
        last_announced(&mut events, 3).await,
        Some((snapshot.revision, snapshot.applied_revision))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn overlapping_saves_and_reloads_end_with_one_consistent_snapshot() {
    let mut f = Fixture::new("version = 3\n");
    let mut events = subscribe(&f);
    for round in 0..16 {
        let (save, reload) = (format!("save-{round}"), format!("reload-{round}"));
        let revision = f.snapshot().revision;
        f.run(save_provider(
            &save,
            revision,
            provider(&format!("p{round}"), "a"),
        ))
        .unwrap();
        f.run(SettingsCommand::ReloadModels(ReloadModels {
            operation_id: op(&reload),
        }))
        .unwrap();
        for id in [&save, &reload] {
            assert_eq!(
                f.settings
                    .wait_operation(&op(id))
                    .await
                    .unwrap()
                    .operation
                    .state,
                OperationState::Ready
            );
        }

        let snapshot = f.snapshot();
        assert_eq!(
            last_announced(&mut events, 2).await,
            Some((snapshot.revision, snapshot.applied_revision)),
            "round {round}"
        );
    }
}

/// The core starts with the strict load. A document the lenient load accepts by
/// dropping what it does not know is not saved: it would be written and then
/// refuse to start.
#[tokio::test(flavor = "multi_thread")]
async fn a_document_the_core_would_not_start_with_is_refused_on_a_whole_save() {
    let base = format!("version = 3\n{PROVIDER}");
    let mut f = Fixture::new(&base);
    let unknown = format!("{base}[providers.local.not_a_setting]\nvalue = 'SENTINEL-UNKNOWN'\n");
    let misspelt = base.replace("default_model", "defualt_model");
    for (id, text) in [("save-1", unknown), ("save-2", misspelt)] {
        let state = f.save_document(id, &text).await;
        assert!(
            matches!(
                state,
                OperationState::Failed {
                    code: ErrorCode::InvalidRequest
                }
            ),
            "{id}: {state:?}"
        );
        let view = f.settings.operation(&op(id)).unwrap();
        let wire = serde_json::to_string(&view).unwrap();
        assert!(!wire.contains("SENTINEL"), "{wire}");
        assert_eq!(f.file(), base, "{id}");
    }
    // The same document without the stray field is saved.
    let fixed = format!("{base}# a comment\n");
    assert_eq!(
        f.save_document("save-3", &fixed).await,
        OperationState::Ready
    );
    assert_eq!(f.file(), fixed);
}

/// A typed (partial) save does not repair or extend a file the core would
/// refuse: the file is left as it is, and the whole-document save is the way
/// to fix it.
#[tokio::test(flavor = "multi_thread")]
async fn a_partial_save_does_not_build_on_a_file_the_core_would_not_start_with() {
    let broken = format!("version = 3\n{PROVIDER}[providers.local.not_a_setting]\nvalue = 1\n");
    let mut f = Fixture::new(&broken);
    let revision = f.snapshot().revision;
    let state = f
        .finish(
            "save-1",
            save_provider("save-1", revision, provider("two", "b")),
        )
        .await;
    assert!(
        matches!(
            state,
            OperationState::Failed {
                code: ErrorCode::InvalidRequest
            }
        ),
        "{state:?}"
    );
    assert_eq!(f.file(), broken);
    // Fixed by a whole save, the typed saves work again.
    let fixed = format!("version = 3\n{PROVIDER}");
    assert_eq!(
        f.save_document("save-2", &fixed).await,
        OperationState::Ready
    );
    let revision = f.snapshot().revision;
    assert_eq!(
        f.finish(
            "save-3",
            save_provider("save-3", revision, provider("two", "b"))
        )
        .await,
        OperationState::Ready
    );
    assert!(f.file().contains("[providers.two]"));
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_whole_save_keeps_the_mode_of_the_file_it_replaces() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new("version = 3\n");
    let path = f.dir.path().join("config.toml");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        f.save_document("save-1", "version = 3\n# saved\n").await,
        OperationState::Ready
    );
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}
