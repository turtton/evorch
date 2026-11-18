use super::*;
use crate::areas::artifacts::preview_html;
use renderer_ipc::{OpenArtifact, SessionId, TabId, Viewport};
use runtime::artifacts::ArtifactRecord;

enum Job {
    Open {
        artifact: ArtifactRef,
        record: ArtifactRecord,
        session: RendererSessionRef,
        viewport: RendererViewport,
    },
    Tab {
        command: BrowserCommand,
        tab: LiveTab,
    },
    CloseSession(Vec<Tab>),
}
struct NewTabGuard {
    owner: RendererOwner,
    tab: Option<Tab>,
}
impl Drop for NewTabGuard {
    fn drop(&mut self) {
        if let Some(tab) = self.tab {
            self.owner.close_later(tab);
        }
    }
}
impl RendererService {
    fn launch(
        self: &Arc<Self>,
        operation_id: RendererId,
        job: Job,
        restart: HostCommandPermit,
    ) -> Result<RendererId, WorkbenchError> {
        let service = self.clone();
        let task_id = operation_id.clone();
        let mut cancel = self
            .state
            .lock()
            .unwrap()
            .operations
            .get(operation_id.as_str())
            .ok_or_else(denied)?
            .cancel
            .subscribe();
        let mut changed = self.gate.subscribe();
        if self
            .tasks
            .spawn_counted_tracked(
                "renderer-operation",
                Some(restart),
                move |mut stop| async move {
                    let interrupted_tab = match &job {
                        Job::Tab { tab, .. } => Some(tab.clone()),
                        _ => None,
                    };
                    let operation = service.perform(job, &task_id);
                    tokio::pin!(operation);
                    let outcome = loop {
                        if *cancel.borrow() {
                            break Err(RendererFailure::Closed);
                        }
                        if service.gate.phase() != HostRestartPhase::Open {
                            break Err(RendererFailure::Restart);
                        }
                        tokio::select! {
                            biased;
                            _ = stop.stopped() => break Err(RendererFailure::Closed),
                            event = cancel.changed() => {
                                if event.is_err() { break Err(RendererFailure::Closed); }
                            },
                            event = changed.changed() => {
                                if event.is_err() { break Err(RendererFailure::Restart); }
                            },
                            outcome = &mut operation => break outcome,
                        }
                    };
                    if matches!(
                        outcome,
                        Err(RendererFailure::Closed | RendererFailure::Restart)
                    ) && let Some(tab) = interrupted_tab
                        && tab.client.is_some()
                    {
                        service.owner.close_later(tab.physical);
                        service.forget_tab(&tab, &task_id);
                    }
                    service.complete(&task_id, outcome);
                    Ok(())
                },
            )
            .is_err()
        {
            self.state
                .lock()
                .unwrap()
                .operations
                .remove(operation_id.as_str());
            return Err(busy());
        }
        self.publish(BrowserEvent::OperationChanged {
            id: operation_id.clone(),
        });
        Ok(operation_id)
    }

    pub(crate) fn open_preview(
        self: &Arc<Self>,
        artifact: ArtifactRef,
        record: ArtifactRecord,
        session: RendererSessionRef,
        viewport: RendererViewport,
        p: &Provenance,
        restart: HostCommandPermit,
    ) -> Result<RendererId, WorkbenchError> {
        local(p)?;
        self.prune();
        let operation_id = {
            let mut state = self.state.lock().unwrap();
            self.session(&state, &session, p)?;
            let pending = state
                .operations
                .values()
                .filter(|op| {
                    op.session.is_some()
                        && op.tab.is_none()
                        && matches!(op.public.state, RendererOperationState::Running)
                })
                .count();
            if pending + self.owner.status().tabs >= MAX_RENDERER_TABS {
                return Err(busy());
            }
            let operation_id =
                self.reserve(&mut state, p, Some(session.id.as_str().to_owned()), None)?;
            state
                .operations
                .get_mut(operation_id.as_str())
                .unwrap()
                .artifact = Some(artifact.clone());
            operation_id
        };
        self.launch(
            operation_id,
            Job::Open {
                artifact,
                record,
                session,
                viewport,
            },
            restart,
        )
    }

    pub(crate) fn command(
        self: &Arc<Self>,
        command: BrowserCommand,
        p: &Provenance,
        restart: HostCommandPermit,
    ) -> Result<RendererId, WorkbenchError> {
        let client = local(p)?.clone();
        self.prune();
        let mut state = self.state.lock().unwrap();
        if state.operations.len() >= MAX_RENDERER_OPERATIONS
            || state.operations.values().map(|op| op.bytes).sum::<usize>() >= MAX_RESULT_BYTES
        {
            return Err(busy());
        }
        let (session_scope, tab_scope, output, job) = match &command {
            BrowserCommand::CreateRendererSession {} => {
                if state.sessions.len() >= MAX_SESSIONS {
                    return Err(busy());
                }
                let reference = RendererSessionRef {
                    core_epoch: self.epoch,
                    id: id(),
                };
                let token = id();
                state.sessions.insert(
                    reference.id.as_str().to_owned(),
                    Session {
                        reference: reference.clone(),
                        client,
                        surface: p.surface,
                        connection_epoch: self.connections.epoch_of(local(p)?),
                        token: token.clone(),
                        bound_generation: None,
                        detached: None,
                    },
                );
                (
                    Some(reference.id.as_str().to_owned()),
                    None,
                    Some(RendererOutput::Session {
                        session: reference,
                        resume_token: token,
                    }),
                    None,
                )
            }
            BrowserCommand::ResumeRendererSession { session, token } => {
                let saved = state
                    .sessions
                    .get_mut(session.id.as_str())
                    .ok_or_else(denied)?;
                if session.core_epoch != self.epoch
                    || saved.client != client
                    || saved.surface != p.surface
                    || saved.token != *token
                    || saved.detached.is_none_or(|until| until <= Instant::now())
                {
                    return Err(denied());
                }
                saved.client = client.clone();
                saved.surface = p.surface;
                saved.connection_epoch = self.connections.epoch_of(&client);
                saved.detached = None;
                saved.token = id();
                let output = RendererOutput::Session {
                    session: saved.reference.clone(),
                    resume_token: saved.token.clone(),
                };
                for tab in state
                    .tabs
                    .values_mut()
                    .filter(|tab| tab.client.is_some() && tab.public.session == *session)
                {
                    tab.client = Some(client.clone());
                    tab.observers.clear();
                }
                (
                    Some(session.id.as_str().to_owned()),
                    None,
                    Some(output),
                    None,
                )
            }
            BrowserCommand::CloseRendererSession { session } => {
                self.session(&state, session, p)?;
                let tabs = state
                    .tabs
                    .values()
                    .filter(|tab| tab.client.is_some() && tab.public.session == *session)
                    .map(|tab| tab.physical)
                    .collect::<Vec<_>>();
                self.remove_session(&mut state, session.id.as_str(), RendererFailure::Closed);
                (None, None, None, Some(Job::CloseSession(tabs)))
            }
            BrowserCommand::UnsubscribeRendererFrames { subscription } => {
                let lease = state
                    .subscriptions
                    .get(subscription.as_str())
                    .ok_or_else(denied)?;
                if lease.client != client || lease.surface != p.surface {
                    return Err(denied());
                }
                let tab = lease.tab;
                state.subscriptions.remove(subscription.as_str());
                (None, Some(tab), Some(RendererOutput::Done), None)
            }
            BrowserCommand::BindRendererData { channel_id, source } => {
                self.bind_data(&mut state, channel_id, source, &client, p)?;
                (None, None, Some(RendererOutput::Done), None)
            }
            _ => {
                let reference = match &command {
                    BrowserCommand::ResizeRendererTab { tab, .. }
                    | BrowserCommand::SetRendererStreaming { tab, .. }
                    | BrowserCommand::SetRendererFrameRate { tab, .. }
                    | BrowserCommand::RendererCdp { tab, .. }
                    | BrowserCommand::SettleRendererTab { tab }
                    | BrowserCommand::TakeRendererNotices { tab }
                    | BrowserCommand::CaptureRendererTab { tab }
                    | BrowserCommand::CloseRendererTab { tab }
                    | BrowserCommand::GrantRendererObserver { tab, .. }
                    | BrowserCommand::SubscribeRendererFrames { tab } => tab,
                    _ => unreachable!(),
                };
                let tab = if matches!(command, BrowserCommand::GrantRendererObserver { .. }) {
                    // Ingress verifies the core-stored run/thread before a RUN grant.
                    let tab = state.tabs.get(reference.id.as_str()).ok_or_else(denied)?;
                    if tab.client.is_some() {
                        self.tab(&state, reference, p, true)?;
                    }
                    if &tab.public.reference != reference {
                        return Err(denied());
                    }
                    tab.clone()
                } else if matches!(command, BrowserCommand::SubscribeRendererFrames { .. }) {
                    let tab = state.tabs.get(reference.id.as_str()).ok_or_else(denied)?;
                    let owns = self.tab(&state, reference, p, true).is_ok();
                    if &tab.public.reference != reference
                        || (!owns && !tab.observers.get(&client).is_some_and(|rights| rights.1))
                    {
                        return Err(denied());
                    }
                    tab.clone()
                } else {
                    self.tab(&state, reference, p, true)?.clone()
                };
                let scope = tab
                    .client
                    .as_ref()
                    .map(|_| tab.public.session.id.as_str().to_owned());
                match &command {
                    BrowserCommand::GrantRendererObserver {
                        client,
                        metadata,
                        frames,
                        ..
                    } => {
                        if tab.observers.len() >= 16 && !tab.observers.contains_key(client) {
                            return Err(busy());
                        }
                        let entry = state.tabs.get_mut(reference.id.as_str()).unwrap();
                        if *metadata || *frames {
                            entry.observers.insert(client.clone(), (*metadata, *frames));
                        } else {
                            entry.observers.remove(client);
                        }
                        state.subscriptions.retain(|_, lease| {
                            lease.tab != tab.physical || lease.client != *client || *frames
                        });
                        (scope, Some(tab.physical), Some(RendererOutput::Done), None)
                    }
                    BrowserCommand::SubscribeRendererFrames { .. } => {
                        if state.subscriptions.len() >= data::MAX_FRAME_SUBSCRIPTIONS {
                            return Err(busy());
                        }
                        // A 4K stream at 30fps or more is bounded tighter: eight
                        // readers of it do not hold the latency bound (docs/renderer-measurement.md §9).
                        // Only a new subscription is refused; the ones that exist
                        // when a tab grows to 4K keep their stream.
                        let physical = tab.physical;
                        if is_four_k_stream(tab.public.viewport, tab.public.fps)
                            && state
                                .subscriptions
                                .values()
                                .filter(|lease| lease.tab == physical)
                                .count()
                                >= data::MAX_FOUR_K_FRAME_SUBSCRIPTIONS
                        {
                            return Err(four_k_busy());
                        }
                        let subscription = id();
                        let token = id();
                        state.subscriptions.insert(
                            subscription.as_str().to_owned(),
                            Subscription {
                                connection_epoch: self.connections.epoch_of(&client),
                                client,
                                surface: p.surface,
                                session: scope.clone(),
                                tab: tab.physical,
                                expires: Instant::now() + GRACE,
                                token: token.clone(),
                                bound: None,
                            },
                        );
                        (
                            None,
                            Some(tab.physical),
                            Some(RendererOutput::Subscription {
                                subscription,
                                token,
                                tab: reference.clone(),
                            }),
                            None,
                        )
                    }
                    _ => (
                        scope,
                        Some(tab.physical),
                        None,
                        Some(Job::Tab {
                            command: command.clone(),
                            tab,
                        }),
                    ),
                }
            }
        };
        let operation_id = self.reserve(&mut state, p, session_scope, tab_scope)?;
        drop(state);
        if let Some(output) = output {
            self.complete(&operation_id, Ok(output));
            Ok(operation_id)
        } else {
            self.launch(operation_id, job.unwrap(), restart)
        }
    }

    async fn perform(
        &self,
        job: Job,
        operation_id: &RendererId,
    ) -> Result<RendererOutput, RendererFailure> {
        match job {
            Job::Open {
                artifact,
                record,
                session,
                viewport,
            } => {
                let html = preview_html(&self.store, &record)
                    .map_err(|_| RendererFailure::InvalidRequest)?;
                let (open, body) = OpenArtifact::with_files(
                    SessionId(0),
                    TabId(0),
                    Viewport {
                        width: viewport.width,
                        height: viewport.height,
                    },
                    30,
                    30_000,
                    vec![("index.html".into(), "text/html".into(), html)],
                );
                let physical = self.owner.open(open, body).await.map_err(failure)?;
                let mut guard = NewTabGuard {
                    owner: self.owner.clone(),
                    tab: Some(physical),
                };
                let mut state = self.state.lock().unwrap();
                let saved = state
                    .sessions
                    .get_mut(session.id.as_str())
                    .ok_or(RendererFailure::Closed)?;
                if saved.detached.is_some()
                    || self.gate.phase() != HostRestartPhase::Open
                    || self.owner.status().generation != physical.generation
                {
                    return Err(RendererFailure::Closed);
                }
                saved.bound_generation = Some(physical.generation);
                let client = saved.client.clone();
                let public = RendererTab {
                    reference: RendererTabRef {
                        core_epoch: self.epoch,
                        generation: physical.generation,
                        id: id(),
                    },
                    session,
                    owner: RendererOwnerKind::ClientPreview,
                    artifact,
                    viewport,
                    fps: 30,
                    streaming: false,
                };
                state.tabs.insert(
                    public.reference.id.as_str().to_owned(),
                    LiveTab {
                        public: public.clone(),
                        physical,
                        client: Some(client),
                        observers: HashMap::new(),
                    },
                );
                guard.tab = None;
                drop(state);
                self.publish(BrowserEvent::TabChanged {
                    id: public.reference.id.clone(),
                });
                Ok(RendererOutput::Tab(Box::new(public)))
            }
            Job::CloseSession(tabs) => {
                for tab in tabs {
                    self.owner.close_wait(tab).await.map_err(failure)?;
                }
                Ok(RendererOutput::Done)
            }
            Job::Tab { command, tab } => {
                let outcome = async {
                    match command {
                        BrowserCommand::ResizeRendererTab { viewport, .. } => {
                            self.owner
                                .resize(
                                    tab.physical,
                                    Viewport {
                                        width: viewport.width,
                                        height: viewport.height,
                                    },
                                )
                                .await
                                .map_err(failure)?;
                            if let Some(current) = self
                                .state
                                .lock()
                                .unwrap()
                                .tabs
                                .get_mut(tab.public.reference.id.as_str())
                            {
                                current.public.viewport = viewport;
                            }
                            Ok(RendererOutput::Done)
                        }
                        BrowserCommand::SetRendererStreaming { enabled, .. } => {
                            self.owner
                                .set_streaming(tab.physical, enabled)
                                .await
                                .map_err(failure)?;
                            if let Some(current) = self
                                .state
                                .lock()
                                .unwrap()
                                .tabs
                                .get_mut(tab.public.reference.id.as_str())
                            {
                                current.public.streaming = enabled;
                            }
                            Ok(RendererOutput::Done)
                        }
                        BrowserCommand::SetRendererFrameRate { fps, .. } => {
                            self.owner
                                .set_frame_rate(tab.physical, fps)
                                .await
                                .map_err(failure)?;
                            if let Some(current) = self
                                .state
                                .lock()
                                .unwrap()
                                .tabs
                                .get_mut(tab.public.reference.id.as_str())
                            {
                                current.public.fps = fps;
                            }
                            Ok(RendererOutput::Done)
                        }
                        BrowserCommand::RendererCdp { method, params, .. } => self
                            .owner
                            .cdp(tab.physical, method, params)
                            .await
                            .map(|result| RendererOutput::Cdp { result })
                            .map_err(failure),
                        BrowserCommand::SettleRendererTab { .. } => self
                            .owner
                            .settle(tab.physical)
                            .await
                            .map(|_| RendererOutput::Done)
                            .map_err(failure),
                        BrowserCommand::TakeRendererNotices { .. } => self
                            .owner
                            .notices(tab.physical)
                            .await
                            .map(|notices| RendererOutput::Notices {
                                blocked: notices.blocked,
                                dialogs: notices.dialogs,
                            })
                            .map_err(failure),
                        BrowserCommand::CaptureRendererTab { .. } => {
                            let bytes =
                                self.owner.capture(tab.physical).await.map_err(|error| {
                                    if matches!(
                                        error,
                                        OwnerError::Remote(renderer_ipc::ErrorKind::TooLarge)
                                    ) {
                                        RendererFailure::FrameTooLarge
                                    } else {
                                        failure(error)
                                    }
                                })?;
                            let mut state = self.state.lock().unwrap();
                            if !state.tabs.contains_key(tab.public.reference.id.as_str()) {
                                return Err(RendererFailure::Closed);
                            }
                            if state.captures.len() >= MAX_RENDERER_TABS
                                || bytes.len()
                                    + state
                                        .captures
                                        .values()
                                        .map(|capture| capture.bytes.len())
                                        .sum::<usize>()
                                    > MAX_CAPTURE_BYTES
                            {
                                return Err(RendererFailure::Busy);
                            }
                            let (client, surface) = state
                                .operations
                                .get(operation_id.as_str())
                                .map(|op| (op.client.clone(), op.surface))
                                .ok_or(RendererFailure::Closed)?;
                            let capture_id = id();
                            let byte_len = bytes.len() as u64;
                            state.captures.insert(
                                capture_id.as_str().to_owned(),
                                Capture {
                                    tab: tab.physical,
                                    bytes: Arc::new(bytes),
                                    expires: Instant::now() + GRACE,
                                    operation: operation_id.clone(),
                                    connection_epoch: self.connections.epoch_of(&client),
                                    client,
                                    surface,
                                    bound: None,
                                },
                            );
                            Ok(RendererOutput::Capture {
                                capture_id,
                                byte_len,
                            })
                        }
                        BrowserCommand::CloseRendererTab { .. } => {
                            self.owner.close_wait(tab.physical).await.map_err(failure)?;
                            self.forget_tab(&tab, operation_id);
                            Ok(RendererOutput::Done)
                        }
                        _ => Err(RendererFailure::InvalidRequest),
                    }
                }
                .await;
                if matches!(outcome, Err(RendererFailure::TimedOut)) {
                    self.owner.close_later(tab.physical);
                    self.forget_tab(&tab, operation_id);
                }
                self.publish(BrowserEvent::TabChanged {
                    id: tab.public.reference.id.clone(),
                });
                outcome
            }
        }
    }

    fn forget_tab(&self, tab: &LiveTab, except: &RendererId) {
        let mut state = self.state.lock().unwrap();
        for (id, operation) in &mut state.operations {
            if id != except.as_str() && operation.tab == Some(tab.physical) {
                operation.cancel.send_replace(true);
                operation.public.state = RendererOperationState::Cancelled {
                    reason: RendererFailure::Closed,
                };
                operation.bytes = 0;
            }
        }
        state.tabs.remove(tab.public.reference.id.as_str());
        state
            .captures
            .retain(|_, capture| capture.tab != tab.physical);
        state
            .subscriptions
            .retain(|_, lease| lease.tab != tab.physical);
    }
}
