//! Host service of the `settings` area (PR 10): the configuration file as a
//! client sees it, the saves that change it and the reload that makes the
//! running core follow it.
//!
//! A client never writes the file or the credential store. It quotes the
//! revision it read; the core compares that with the file inside one writer
//! lock, together with the restart-target guard and the write, so two saves
//! quoting the same revision end as one success and one `stale_revision`.
//! *Saved* (the file) and *applied* (the models the runs use) are kept apart:
//! a save does not touch what a run already sent, and
//! `reload_models` recomposes the models the requests that follow use (the next
//! request of a running conversation included).
//!
//! All file work runs on the blocking pool inside an operation task; the
//! handlers only look at the table. The task keeps the restart permit its
//! command was accepted with until the outcome is settled: a save is bounded
//! I/O, it never waits for a person.
pub mod feature_options;
pub(in crate::areas) mod operations;
pub mod provider_models;
pub(in crate::areas) mod store;

use super::{AreaHandles, ProjectionSource, ReloadModels};
use crate::bootstrap::BootstrapError;
use crate::bootstrap::restart::HostCommandPermit;
use operations::{Lookup, Operations, Tracked, digest_of};
use provider_models::{FetchFailure, ModelFetch};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use store::{ConfigStore, SaveError, revision_of};
use workbench_protocol::{
    errors::{ErrorCode, RetryAdvice, WorkbenchError},
    events::UiEvent,
    ids::OperationId,
    namespaces::settings::{
        MAX_MESSAGE_BYTES, MAX_OPERATIONS, ProviderModelCandidates, SettingsCommand, SettingsEvent,
        SettingsOperationView, SettingsRevision, SettingsSnapshot,
    },
    operation::{OperationState, OperationView},
};

impl Tracked for SettingsOperationView {
    fn operation(&self) -> &OperationView {
        &self.operation
    }

    fn operation_mut(&mut self) -> &mut OperationView {
        &mut self.operation
    }
}

/// The `settings` service, reached from `ingress/namespaces/settings.rs` as
/// `self.areas.settings`. Without handles (the unwired form) every operation
/// is refused.
#[derive(Default, Clone)]
pub struct SettingsArea {
    inner: Option<Arc<Inner>>,
}

/// What a service needs, all of it from the bootstrap's handles.
pub struct SettingsParts {
    pub config_dir: std::path::PathBuf,
    /// The configuration the core started with.
    pub startup: Arc<config::Config>,
    pub reload_models: ReloadModels,
    pub tasks: super::AreaTasks,
    pub source: ProjectionSource,
    /// What the Features screen follows at once in the running core.
    pub features: Arc<dyn feature_options::FeatureRuntime>,
    /// How `fetch_provider_models` asks a provider for its models.
    pub model_fetch: ModelFetch,
}

type QuotaUpdates = (
    tokio::sync::watch::Receiver<u64>,
    Arc<dyn Fn() -> Option<config::Config> + Send + Sync>,
);

struct Inner {
    store: Arc<ConfigStore>,
    reload: ReloadModels,
    tasks: super::AreaTasks,
    source: ProjectionSource,
    features: Arc<dyn feature_options::FeatureRuntime>,
    model_fetch: ModelFetch,
    state: Mutex<State>,
    quota_changed: tokio::sync::watch::Sender<u64>,
    /// One writer at a time, held across compare, check and write.
    writer: Arc<tokio::sync::Mutex<()>>,
    #[cfg(test)]
    before_snapshot_publish: Mutex<Option<tests::PublishHook>>,
}

struct State {
    operations: Operations<SettingsOperationView>,
    snapshot: SettingsSnapshot,
    /// The Features values the files held when they were last read.
    features: feature_options::SavedFeatures,
    /// Numbers the re-reads in the order they started, and the number of the
    /// newest one whose result was published: a slower, older re-read never
    /// replaces what a newer one already published.
    reads_started: u64,
    reads_published: u64,
    /// What finished fetches found, kept while their operation is in the table.
    found: Vec<ProviderModelCandidates>,
}

/// How an accepted command stands.
pub struct Submitted {
    pub view: SettingsOperationView,
    /// The same operation id and request were accepted before.
    pub already_applied: bool,
}

struct Failure {
    code: ErrorCode,
    message: String,
}

struct Done {
    revision: Option<SettingsRevision>,
    message: Option<String>,
}

fn error(code: ErrorCode, message: &str, retry: RetryAdvice) -> WorkbenchError {
    WorkbenchError::new(code, message, retry)
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A bounded sentence without anything that looks like a credential.
fn sanitize(text: &str) -> String {
    let redacted = secret_guard::SecretRedactor::from_env().redact(text).text;
    let mut end = redacted.len().min(MAX_MESSAGE_BYTES);
    while !redacted.is_char_boundary(end) {
        end -= 1;
    }
    redacted[..end].replace(char::is_control, " ")
}

impl From<SaveError> for Failure {
    fn from(error: SaveError) -> Self {
        match error {
            SaveError::Stale => Self {
                code: ErrorCode::StaleRevision,
                message: "the configuration changed since the revision quoted; read it again"
                    .into(),
            },
            SaveError::Invalid(message) => Self {
                code: ErrorCode::InvalidRequest,
                message: sanitize(&message),
            },
            SaveError::RestartTargets => Self {
                code: ErrorCode::PermissionDenied,
                message: "restart targets may only be changed in local config files".into(),
            },
            SaveError::Io(message) => Self {
                code: ErrorCode::StorageUnavailable,
                message,
            },
            SaveError::NoSuchProvider => Self {
                code: ErrorCode::NotFound,
                message: "the saved configuration has no such provider".into(),
            },
        }
    }
}

impl SettingsArea {
    pub(super) fn new(handles: &AreaHandles) -> Result<Self, BootstrapError> {
        let source = handles
            .source("settings")
            .map_err(|e| BootstrapError::from(e.message))?;
        Ok(Self::with_parts(SettingsParts {
            config_dir: handles.config_dir.clone(),
            startup: handles.config.clone(),
            reload_models: handles.reload_models.clone(),
            tasks: handles.tasks.clone(),
            source,
            features: Arc::new(handles.runtime.clone()),
            model_fetch: ModelFetch::production(handles.credentials.clone()),
        }))
    }

    /// A service over `parts`; what the bootstrap does and what a test of the
    /// area does.
    pub fn with_parts(parts: SettingsParts) -> Self {
        let store = Arc::new(ConfigStore::new(parts.config_dir, parts.startup));
        let applied = store.revision().unwrap_or_else(|_| revision_of(&[]));
        let snapshot = store.snapshot(Some(&applied));
        let features = feature_options::SavedFeatures::read(&store);
        Self {
            inner: Some(Arc::new(Inner {
                store,
                reload: parts.reload_models,
                tasks: parts.tasks,
                source: parts.source,
                features: parts.features,
                model_fetch: parts.model_fetch,
                state: Mutex::new(State {
                    operations: Operations::new(MAX_OPERATIONS),
                    snapshot,
                    features,
                    reads_started: 0,
                    reads_published: 0,
                    found: Vec::new(),
                }),
                writer: Arc::default(),
                quota_changed: tokio::sync::watch::channel(0).0,
                #[cfg(test)]
                before_snapshot_publish: Mutex::default(),
            })),
        }
    }

    /// Internal invalidation only; consumers re-read through ConfigStore on a worker.
    pub(in crate::areas) fn quota_updates(&self) -> Option<QuotaUpdates> {
        let inner = self.inner.as_ref()?;
        let store = inner.store.clone();
        Some((
            inner.quota_changed.subscribe(),
            Arc::new(move || store.effective().ok().map(|(_, config)| config)),
        ))
    }

    fn inner(&self) -> Result<&Arc<Inner>, WorkbenchError> {
        self.inner.as_ref().ok_or_else(|| {
            error(
                ErrorCode::UnsupportedCapability,
                "settings are not available in this host",
                RetryAdvice::Never,
            )
        })
    }

    /// Accept a command: start its operation, or answer from the table.
    ///
    /// # Errors
    /// `Conflict` for an id used by a different request, `LimitExceeded` when
    /// every retained operation is still running, `Draining` once the host is
    /// stopping.
    pub(crate) fn submit(
        &self,
        command: SettingsCommand,
        accepted: Option<HostCommandPermit>,
    ) -> Result<Submitted, WorkbenchError> {
        let inner = self.inner()?;
        let id = command.operation_id().clone();
        let digest = digest_of(&[serde_json::to_vec(&command).unwrap_or_default().as_slice()]);
        let view = {
            let mut state = lock(&inner.state);
            match state.operations.lookup(&id, &digest) {
                Lookup::Same(view) => {
                    return Ok(Submitted {
                        view,
                        already_applied: true,
                    });
                }
                Lookup::Conflict => {
                    return Err(error(
                        ErrorCode::Conflict,
                        "this operation id was used for a different request",
                        RetryAdvice::Never,
                    ));
                }
                Lookup::New => {}
            }
            let revision = state.operations.next_revision();
            let view = SettingsOperationView {
                operation: OperationView {
                    operation_id: id.clone(),
                    state: OperationState::Pending,
                    revision,
                    result: None,
                },
                kind: command.kind(),
                revision: None,
                message: None,
            };
            state.operations.insert(view.clone(), digest)?;
            view
        };
        let task = inner.clone();
        let task_id = id.clone();
        let spawned =
            inner
                .tasks
                .spawn_counted_tracked("settings-operation", accepted, move |_stop| {
                    task.run(task_id, command)
                });
        if let Err(refused) = spawned {
            lock(&inner.state).operations.forget(&id);
            return Err(error(
                ErrorCode::Draining,
                &refused.to_string(),
                RetryAdvice::AfterReconnect,
            ));
        }
        Ok(Submitted {
            view,
            already_applied: false,
        })
    }

    /// The snapshot the core holds.
    ///
    /// # Errors
    /// Only the unwired form refuses.
    pub fn snapshot(&self) -> Result<SettingsSnapshot, WorkbenchError> {
        Ok(lock(&self.inner()?.state).snapshot.clone())
    }

    /// One operation, running or finished.
    ///
    /// # Errors
    /// `NotFound` for an id the core does not hold.
    pub fn operation(&self, id: &OperationId) -> Result<SettingsOperationView, WorkbenchError> {
        lock(&self.inner()?.state)
            .operations
            .get(id)
            .cloned()
            .ok_or_else(|| error(ErrorCode::NotFound, "unknown operation", RetryAdvice::Never))
    }

    /// Wait until `id` is settled and return it.
    ///
    /// # Errors
    /// `NotFound` for an id the core does not hold.
    pub async fn wait_operation(
        &self,
        id: &OperationId,
    ) -> Result<SettingsOperationView, WorkbenchError> {
        let mut changes = lock(&self.inner()?.state).operations.subscribe();
        loop {
            let view = self.operation(id)?;
            if view.operation.state.is_final() {
                return Ok(view);
            }
            if changes.changed().await.is_err() {
                return Ok(view);
            }
        }
    }

    /// What the finished fetch `id` found.
    ///
    /// # Errors
    /// `NotFound` for an operation the core does not hold, one that did not end
    /// `Ready`, or one that was no fetch.
    pub fn provider_models(
        &self,
        id: &OperationId,
    ) -> Result<ProviderModelCandidates, WorkbenchError> {
        let state = lock(&self.inner()?.state);
        state
            .operations
            .get(id)
            .filter(|view| view.operation.state == OperationState::Ready)
            .and_then(|_| state.found.iter().find(|found| &found.operation_id == id))
            .cloned()
            .ok_or_else(|| {
                error(
                    ErrorCode::NotFound,
                    "no models were found by this operation",
                    RetryAdvice::Never,
                )
            })
    }

    /// The retained operations, newest first.
    ///
    /// # Errors
    /// Only the unwired form refuses.
    pub fn operations(&self) -> Result<Vec<SettingsOperationView>, WorkbenchError> {
        Ok(lock(&self.inner()?.state).operations.list())
    }
}

impl Inner {
    fn publish(&self, event: SettingsEvent) {
        #[cfg(test)]
        if matches!(event, SettingsEvent::SettingsSnapshotChanged { .. }) {
            let hook = self.before_snapshot_publish.lock().unwrap().take();
            if let Some(hook) = hook {
                hook(self);
            }
        }
        if matches!(event, SettingsEvent::SettingsSnapshotChanged { .. }) {
            self.quota_changed
                .send_modify(|version| *version = version.wrapping_add(1));
        }
        if let Err(error) = self.source.publish(UiEvent::Settings(event)) {
            tracing::warn!(code = ?error.code, "settings event could not be published");
        }
    }

    async fn blocking<T: Send + 'static>(
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, Failure> {
        tokio::task::spawn_blocking(f).await.map_err(|_| Failure {
            code: ErrorCode::Internal,
            message: "the settings task did not finish".into(),
        })
    }

    async fn run(
        self: Arc<Self>,
        id: OperationId,
        command: SettingsCommand,
    ) -> Result<(), BootstrapError> {
        let outcome = self.execute(command).await;
        self.finish(&id, outcome);
        Ok(())
    }

    async fn execute(&self, command: SettingsCommand) -> Result<Done, Failure> {
        match command {
            SettingsCommand::RefreshSettings(_) => {
                self.reread().await?;
                Ok(Done {
                    revision: Some(lock(&self.state).snapshot.revision.clone()),
                    message: None,
                })
            }
            SettingsCommand::SaveDocument(save) => {
                let store = self.store.clone();
                let guard = self.writer.clone().lock_owned().await;
                let revision = Self::blocking(move || {
                    let _writer = guard;
                    store.save_document(&save.expected_revision, &save.text)
                })
                .await?
                .map_err(Failure::from)?;
                self.reread().await?;
                Ok(Done {
                    revision: Some(revision),
                    message: None,
                })
            }
            SettingsCommand::SaveProvider(save) => {
                let store = self.store.clone();
                // Held through the re-read: a reload (which needs this lock) cannot
                // change what is applied between the write and the decision below.
                let _writer = self.writer.clone().lock_owned().await;
                let expected = save.expected_revision.clone();
                let saved = Self::blocking(move || {
                    store.save_provider(
                        &save.expected_revision,
                        save.original_name.as_deref(),
                        &save.provider,
                    )
                })
                .await?
                .map_err(Failure::from)?;
                // A save that changes nothing the runs use (the quota switch) leaves
                // them as applied as they were: no reload is waiting for it.
                self.reread_applying(
                    saved
                        .models_unchanged
                        .then(|| (expected, saved.revision.clone())),
                )
                .await?;
                Ok(Done {
                    revision: Some(saved.revision),
                    message: None,
                })
            }
            SettingsCommand::FetchProviderModels(fetch) => self.fetch_provider_models(fetch).await,
            SettingsCommand::DeleteProvider(delete) => {
                let store = self.store.clone();
                let guard = self.writer.clone().lock_owned().await;
                let revision = Self::blocking(move || {
                    let _writer = guard;
                    store.delete_provider(&delete.expected_revision, &delete.name)
                })
                .await?
                .map_err(Failure::from)?;
                self.reread().await?;
                Ok(Done {
                    revision: Some(revision),
                    message: None,
                })
            }
            SettingsCommand::ReloadModels(_) => self.reload_models().await,
            SettingsCommand::SaveFeatureSettings(save) => self.save_feature_settings(save).await,
        }
    }

    /// Recompose the models from the saved file. What a run already sent is
    /// never rewritten: the replacement is a new `SwitchableModel` target that
    /// the next request of every conversation goes to, and that request still
    /// extends the prefix that was sent.
    async fn reload_models(&self) -> Result<Done, Failure> {
        // A save cannot land between the read of the revision and the end of the
        // hook: the writer lock is held from the one to the other, so the hook
        // composes from exactly the file whose revision is marked applied, and
        // runs once. A save accepted meanwhile waits for the reload (its
        // operation stays pending); queries and every other area go on.
        let _writer = self.writer.lock().await;
        let store = self.store.clone();
        let revision = Self::blocking(move || store.revision())
            .await?
            .map_err(Failure::from)?;
        let reload = self.reload.clone();
        let composed = match Self::blocking(move || reload()).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(message)) => Err(Failure {
                code: ErrorCode::Internal,
                message: sanitize(&message),
            }),
            Err(failure) => Err(failure),
        };
        if let Err(failure) = composed {
            // The hook may have replaced some of the models before it failed
            // (the startup model, then a project that did not load), so no file
            // is known to be what the runs use. It stays unknown, whatever is
            // saved next, until a reload succeeds.
            self.set_applied(None, Some(failure.message.clone()));
            return Err(failure);
        }
        self.set_applied(Some(revision.clone()), None);
        Ok(Done {
            revision: Some(revision),
            message: None,
        })
    }

    /// What the runs use is `applied` (`None`: not known, for `failure`);
    /// announced from the state itself when it changed, so a re-read finding
    /// it already moved would see no change to tell anybody about.
    fn set_applied(&self, applied: Option<SettingsRevision>, failure: Option<String>) {
        // Keep the state lock through synchronous publication, as in reread:
        // otherwise a newer change can announce first, then this stale event.
        let mut state = lock(&self.state);
        let changed =
            state.snapshot.applied_revision != applied || state.snapshot.apply_failure != failure;
        state.snapshot.applied_revision.clone_from(&applied);
        state.snapshot.apply_failure = failure;
        if changed {
            self.publish(SettingsEvent::SettingsSnapshotChanged {
                revision: state.snapshot.revision.clone(),
                applied_revision: applied,
            });
        }
    }

    /// Read the file again into the snapshot and announce a change. Several
    /// re-reads may overlap (a save finishing while a reload does): each is
    /// numbered when it starts, an older result is dropped once a newer one
    /// published, and the applied revision is always the one the state holds
    /// when the result is published, not the one seen when the read began.
    async fn reread(&self) -> Result<(), Failure> {
        self.reread_applying(None).await
    }

    /// [`Self::reread`]; with `advance` = (the revision the save quoted, the one it
    /// wrote), a state that was applied at the first is applied at the second
    /// from the moment the new file is published, so no client sees the save as
    /// waiting for a reload.
    async fn reread_applying(
        &self,
        advance: Option<(SettingsRevision, SettingsRevision)>,
    ) -> Result<(), Failure> {
        let (ticket, applied) = {
            let mut state = lock(&self.state);
            state.reads_started += 1;
            (state.reads_started, state.snapshot.applied_revision.clone())
        };
        let store = self.store.clone();
        let mut fresh = Self::blocking(move || store.snapshot(applied.as_ref())).await?;
        let saved_features = {
            let store = self.store.clone();
            Self::blocking(move || feature_options::SavedFeatures::read(&store)).await?
        };
        // Only the snapshot update and its publication are serialized here;
        // file I/O stays outside the state lock.
        let mut state = lock(&self.state);
        let advances = advance.is_some_and(|(from, to)| {
            state.snapshot.applied_revision.as_ref() == Some(&from) && fresh.revision == to
        });
        if ticket < state.reads_published {
            // A newer read published first, and it read this write too.
            if advances && state.snapshot.revision == fresh.revision {
                let revision = fresh.revision;
                state.snapshot.applied_revision = Some(revision.clone());
                self.publish(SettingsEvent::SettingsSnapshotChanged {
                    revision: revision.clone(),
                    applied_revision: Some(revision),
                });
            }
            return Ok(());
        }
        state.reads_published = ticket;
        state.features = saved_features;
        fresh
            .applied_revision
            .clone_from(&state.snapshot.applied_revision);
        if advances {
            fresh.applied_revision = Some(fresh.revision.clone());
        }
        fresh
            .apply_failure
            .clone_from(&state.snapshot.apply_failure);
        let announce = (state.snapshot.revision != fresh.revision
            || state.snapshot.applied_revision != fresh.applied_revision)
            .then(|| (fresh.revision.clone(), fresh.applied_revision.clone()));
        state.snapshot = fresh;
        if let Some((revision, applied_revision)) = announce {
            self.publish(SettingsEvent::SettingsSnapshotChanged {
                revision,
                applied_revision,
            });
        }
        Ok(())
    }

    /// Ask the provider for its models. The file is read once to take the
    /// profile; the network wait holds no lock, so saves go on meanwhile, and it
    /// ends at the deadline whatever the provider does.
    async fn fetch_provider_models(
        &self,
        fetch: workbench_protocol::namespaces::settings::FetchProviderModels,
    ) -> Result<Done, Failure> {
        let store = self.store.clone();
        let (expected, name) = (fetch.expected_revision.clone(), fetch.provider.clone());
        let profile = Self::blocking(move || store.provider_at(&expected, &name))
            .await?
            .map_err(Failure::from)?;
        let asked = (self.model_fetch.fetcher)(fetch.provider.clone(), profile);
        let found = match tokio::time::timeout(self.model_fetch.timeout, asked).await {
            Ok(Ok(models)) => models,
            Ok(Err(FetchFailure::Unsupported)) => {
                return Err(Failure {
                    code: ErrorCode::UnsupportedCapability,
                    message: "this kind of provider cannot list its models".into(),
                });
            }
            Ok(Err(FetchFailure::Failed(message))) => {
                return Err(Failure {
                    code: ErrorCode::Internal,
                    message: sanitize(&message),
                });
            }
            Err(_) => {
                return Err(Failure {
                    code: ErrorCode::Internal,
                    message: "the provider did not answer in time".into(),
                });
            }
        };
        let candidates = provider_models::candidates(fetch.operation_id, fetch.provider, found);
        let mut state = lock(&self.state);
        let State {
            operations, found, ..
        } = &mut *state;
        found.retain(|kept| operations.get(&kept.operation_id).is_some());
        found.push(candidates);
        Ok(Done {
            revision: None,
            message: None,
        })
    }

    fn finish(&self, id: &OperationId, outcome: Result<Done, Failure>) {
        let view = {
            let mut state = lock(&self.state);
            state.operations.update(id, |view| match outcome {
                Ok(done) => {
                    view.operation.state = OperationState::Ready;
                    view.revision = done.revision;
                    view.message = done.message;
                }
                Err(failure) => {
                    view.operation.state = OperationState::Failed { code: failure.code };
                    view.message = Some(failure.message);
                }
            })
        };
        if let Some(view) = view {
            self.publish(SettingsEvent::SettingsOperationCompleted { operation: view });
        }
    }
}

#[cfg(test)]
mod tests;
