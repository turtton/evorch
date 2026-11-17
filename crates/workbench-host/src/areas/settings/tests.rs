use super::*;
use crate::projection::Projection;
use event_bus::EventBus;
use providers::{ChatResponse, Message, ToolSpec};
use runtime::{AgentInvocationContext, AgentModel, AgentRuntime, Role, RuntimeError};
use workbench_protocol::events::{Subscribe, SubscriptionScope};

pub(super) type PublishHook = Box<dyn FnOnce(&Inner) + Send>;

const A: &str = "version = 3\n";
const B: &str = "version = 3\n# B\n";
const C: &str = "version = 3\n# C\n";

struct NoModel;

#[async_trait::async_trait]
impl AgentModel for NoModel {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        unreachable!("settings publication does not call a model")
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "unused".into()
    }
}

#[derive(Clone, Copy)]
enum Change {
    Saved(&'static str),
    Applied(Option<&'static str>),
}

impl Change {
    async fn apply(self, inner: &Inner, file: &std::path::Path) {
        match self {
            Self::Saved(text) => {
                std::fs::write(file, text).unwrap();
                assert!(inner.reread().await.is_ok());
            }
            Self::Applied(text) => inner.set_applied(
                text.map(|text| revision_of(text.as_bytes())),
                text.is_none().then(|| "composition failed".into()),
            ),
        }
    }

    fn announced(self, previous: SettingsEvent) -> SettingsEvent {
        let SettingsEvent::SettingsSnapshotChanged {
            mut revision,
            mut applied_revision,
        } = previous
        else {
            unreachable!()
        };
        match self {
            Self::Saved(text) => revision = revision_of(text.as_bytes()),
            Self::Applied(text) => {
                applied_revision = text.map(|text| revision_of(text.as_bytes()));
            }
        }
        SettingsEvent::SettingsSnapshotChanged {
            revision,
            applied_revision,
        }
    }
}

/// Pause the first change immediately before publication, then let a second
/// change overtake it if the state lock has already been released. If the lock
/// is still held, release publication first so the second change can proceed.
/// This forces the old race without sleeps, scheduler luck or a deadlock in
/// the fixed implementation. try_lock chooses the schedule, not the verdict:
/// the assertion checks the events a real subscription receives.
async fn announcements_in_order(first: Change, second: Change) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config.toml");
    std::fs::write(&file, A).unwrap();
    let bus = Arc::new(EventBus::new(64));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        Arc::new(NoModel),
    );
    let handle = tokio::runtime::Handle::current();
    let projection = Projection::new(runtime, &handle, Default::default());
    let settings = SettingsArea::with_parts(SettingsParts {
        config_dir: dir.path().to_path_buf(),
        startup: Arc::new(config::Config::default()),
        reload_models: Arc::new(|| Ok(())),
        tasks: super::super::AreaTasks::new(handle.clone()),
        source: projection.source("settings").unwrap(),
    });
    let inner = settings.inner.as_ref().unwrap().clone();
    let mut events = projection
        .subscribe_with_snapshot(&Subscribe {
            scope: SubscriptionScope::All,
            after: None,
        })
        .unwrap();
    let initial = SettingsEvent::SettingsSnapshotChanged {
        revision: revision_of(A.as_bytes()),
        applied_revision: Some(revision_of(A.as_bytes())),
    };
    let expected_first = first.announced(initial);
    let expected_last = second.announced(expected_first.clone());

    let (entered, paused) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    *inner.before_snapshot_publish.lock().unwrap() = Some(Box::new(move |inner| {
        entered.send(inner.state.try_lock().is_ok()).unwrap();
        released.recv().unwrap();
    }));
    let changing = inner.clone();
    let changing_file = file.clone();
    let task = tokio::task::spawn_blocking(move || {
        handle.block_on(first.apply(&changing, &changing_file));
    });
    if paused.await.unwrap() {
        second.apply(&inner, &file).await;
        release.send(()).unwrap();
    } else {
        release.send(()).unwrap();
        second.apply(&inner, &file).await;
    }
    task.await.unwrap();

    let snapshot = settings.snapshot().unwrap();
    assert_eq!(
        SettingsEvent::SettingsSnapshotChanged {
            revision: snapshot.revision,
            applied_revision: snapshot.applied_revision,
        },
        expected_last,
        "both changes reached the snapshot"
    );
    let mut announced = Vec::new();
    while announced.len() < 2 {
        if let UiEvent::Settings(event @ SettingsEvent::SettingsSnapshotChanged { .. }) =
            events.recv().await.unwrap().event
        {
            announced.push(event);
        }
    }
    assert_eq!(announced, [expected_first, expected_last]);
}

#[tokio::test]
async fn a_reread_announcement_cannot_follow_a_newer_applied_announcement() {
    announcements_in_order(Change::Saved(B), Change::Applied(Some(B))).await;
}

#[tokio::test]
async fn an_applied_announcement_cannot_follow_a_newer_reread_announcement() {
    announcements_in_order(Change::Applied(None), Change::Saved(B)).await;
}

#[tokio::test]
async fn overlapping_rereads_announce_in_snapshot_order() {
    announcements_in_order(Change::Saved(B), Change::Saved(C)).await;
}
