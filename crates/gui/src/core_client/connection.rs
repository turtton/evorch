//! The single desktop actor. Closing the UI drops the connection; it never
//! schedules a stop. Only explicit human actions can issue state-changing wire
//! commands, and a command with a lost reply is reconciled, never replayed.
mod data;
use super::auto_reconnect::{self, AutoReconnect, LiveCoreBackoff};
use super::disconnect_log::{Episode, SessionEnd};
use super::features::data::DataAction;
use data::DataReaders;

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;
use workbench_client::{ClientError, Subscription, TransportError, WorkbenchClient};
use workbench_protocol::namespaces::{
    ApprovalsQuery, ApprovalsReply, ConversationQuery, ConversationReply, ProjectionQuery,
    ProjectionReply,
};
use workbench_protocol::{
    commands::{Command, CommandDisposition, CommandReply},
    conversation::{ConversationCommand, ConversationResult},
    events::{EventEnvelope, Subscribe, SubscriptionScope, UiEvent},
    ids::{BodyId, ProjectId, RequestId, RunId, ThreadId},
    queries::{
        CommandReceiptState, HistoryDirection, PageRequest, Query, QueryReply, ReadBody,
        ReadCommittedHistory, ReadSnapshot,
    },
    wire::ByteOffset,
};

use super::{
    Shared,
    attachments::ExpectedUploadReply,
    cache::Cache,
    features::{FeatureAction, FeatureError, FeatureId, FeatureReply, FeatureRequest, HeldReply},
};

pub enum Action {
    Command {
        command: Command,
        draft: Option<(String, u64)>,
    },
    /// The newest (or, with `true`, the next older) page of a thread's committed
    /// history, or of one run of it.
    History(ThreadId, Option<RunId>, bool),
    Body(BodyId),
    ApprovalInput(workbench_protocol::views::ApprovalView),
    Upload {
        project: ProjectId,
        thread: ThreadId,
        file: crate::attachments::AttachmentFile,
    },
    /// A request of a feature (`features/`): typed, tagged, answered through
    /// `Shared::features`. The feature gets no connection of its own.
    Feature(FeatureAction),
    Data(DataAction),
    Reconnect,
    Shutdown,
}

fn update(shared: &Arc<Mutex<Shared>>, ctx: &egui::Context, f: impl FnOnce(&mut Shared)) {
    let changed = {
        let mut state = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut state);
        state.changed.clone()
    };
    changed.notify_waiters();
    ctx.request_repaint();
}

#[derive(Clone)]
enum Pending {
    /// `draft` is cleared when the core accepts; the thread's files go with it
    /// only when `takes_files`.
    Command {
        draft: Option<(String, u64)>,
        takes_files: bool,
    },
    Upload {
        thread: String,
        id: String,
        expected: ExpectedUploadReply,
    },
    Feature {
        feature: FeatureId,
        tag: u64,
    },
}

impl Pending {
    fn rejected(&self, state: &mut Shared, error: &str) {
        self.rejected_with(state, FeatureError::new(error));
    }

    fn rejected_with(&self, state: &mut Shared, error: FeatureError) {
        match self {
            Self::Feature { feature, tag } => {
                state
                    .features
                    .reply(*feature, *tag, FeatureReply::Command(Err(error)));
            }
            Self::Command {
                draft: Some((thread, _)),
                ..
            } => {
                state.sending.remove(thread);
            }
            Self::Upload { thread, id, .. } => {
                if let Some(image) = state
                    .attachment_drafts
                    .get_mut(thread)
                    .into_iter()
                    .flatten()
                    .find(|i| i.id == *id)
                {
                    image.uploading = false;
                    image.uncertain = false;
                }
            }
            _ => (),
        }
    }
}

pub(crate) const CONNECTING: &str = "Connecting to the core…";
pub(crate) const NOT_SENT: &str =
    "Not sent: the core is disconnected. Your input is kept; press Reconnect to connect again.";

const SNAPSHOT_BUFFER_EVENTS: usize = 256;
const SNAPSHOT_BUFFER_BYTES: usize = 2 * 1024 * 1024;

/// The projection at the snapshot cut.
pub async fn snapshot(
    client: &WorkbenchClient,
    subscription: &mut Subscription,
) -> Result<Cache, String> {
    snapshot_delivering(client, subscription)
        .await
        .map(|(cache, _)| cache)
        .map_err(|fault| fault.to_string())
}

/// Why a snapshot was not read.
enum SnapshotFault {
    /// It did not settle; a resync may.
    Failed(String),
    /// The core ended the subscription while it was paged: what had arrived so
    /// far, the ending last. It is the core's own word (its goodbye, the evidence
    /// of a restart) and must reach the caller, not be dropped as a snapshot error.
    Ended(Vec<EventEnvelope>),
}

impl std::fmt::Display for SnapshotFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed(error) => f.write_str(error),
            Self::Ended(_) => f.write_str("snapshot stream requires resync"),
        }
    }
}

impl From<String> for SnapshotFault {
    fn from(error: String) -> Self {
        Self::Failed(error)
    }
}

impl From<&str> for SnapshotFault {
    fn from(error: &str) -> Self {
        Self::Failed(error.into())
    }
}

/// [`snapshot`], and the events that arrived while it was being read and apply
/// after it (the features rebuild from them).
async fn snapshot_delivering(
    client: &WorkbenchClient,
    subscription: &mut Subscription,
) -> Result<(Cache, Vec<UiEvent>), SnapshotFault> {
    let started = subscription.started().clone();
    let mut cache = Cache::default();
    let Some(id) = started.snapshot_id else {
        return Err("snapshot was required but the core returned replay".into());
    };
    let mut page = started.initial_page.ok_or("snapshot has no initial page")?;
    let mut buffered = Vec::<EventEnvelope>::new();
    let mut bytes = 0;
    let mut visited = BTreeSet::new();
    loop {
        for item in page.items {
            cache.snapshot(item);
        }
        let Some(cursor) = page.next_cursor else {
            break;
        };
        if !visited.insert(cursor.to_string()) {
            return Err("snapshot cursor repeated".into());
        }
        let next = client.query(Query::Projection(ProjectionQuery::ReadSnapshot(
            ReadSnapshot {
                snapshot_id: id.clone(),
                page: PageRequest {
                    cursor: Some(cursor),
                    ..Default::default()
                },
            },
        )));
        tokio::pin!(next);
        let reply = loop {
            tokio::select! {
                reply = &mut next => match reply {
                    Ok(reply) => break reply,
                    Err(error) => {
                        // The query fails when the connection closes, and the core's
                        // goodbye was queued before that: read it before reporting
                        // the failure, as the select may have chosen this branch.
                        while let Ok(Some(event)) =
                            tokio::time::timeout(std::time::Duration::ZERO, subscription.next()).await
                        {
                            let ends = event.event.ends_subscription();
                            buffered.push(event);
                            if ends {
                                return Err(SnapshotFault::Ended(buffered));
                            }
                        }
                        return Err(SnapshotFault::Failed(error.to_string()));
                    }
                },
                event = subscription.next() => {
                    let event = event.ok_or("subscription ended while paging snapshot")?;
                    if event.event.ends_subscription() {
                        buffered.push(event);
                        return Err(SnapshotFault::Ended(buffered));
                    }
                    bytes += serde_json::to_vec(&event).map_err(|e| e.to_string())?.len();
                    if buffered.len() >= SNAPSHOT_BUFFER_EVENTS || bytes > SNAPSHOT_BUFFER_BYTES {
                        return Err("snapshot event buffer overflow; resync required".into());
                    }
                    buffered.push(event);
                }
            }
        };
        match reply {
            QueryReply::Projection(ProjectionReply::Snapshot(snapshot))
                if snapshot.snapshot_id == id && snapshot.high_water == started.high_water =>
            {
                page = snapshot.page
            }
            _ => return Err("snapshot cut changed while paging".into()),
        }
    }
    cache.at(started.high_water);
    let mut delivered = Vec::new();
    for event in buffered {
        if !cache.event_delivering(event, |event| delivered.push(event.clone())) {
            return Err("snapshot epoch changed; resync required".into());
        }
    }
    Ok((cache, delivered))
}

pub async fn serve(
    launcher: core_launcher::Launcher,
    mut actions: mpsc::Receiver<Action>,
    shared: Arc<Mutex<Shared>>,
    ctx: egui::Context,
) {
    let mut uncertain = Vec::<(RequestId, Pending)>::new();
    let mut auto = AutoReconnect::default();
    let mut live_backoff = LiveCoreBackoff::default();
    let mut episode = Episode::default();
    // A core that still answered after the connection to it was lost, connected
    // and waiting for its reconnect pace to elapse.
    let mut live: Option<core_launcher::ConnectedCore> = None;
    // Resolved outcomes for features, held until the next snapshot has replaced
    // the projection and reset the features (see `features`).
    let mut held = Vec::<HeldReply>::new();
    // The successor of a planned restart, already connected and with its session
    // read, found by a connect-only wait (see `wait_for_successor`).
    #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
    let mut successor: Option<(core_launcher::ConnectedCore, Opened)> = None;
    loop {
        let mut prebuilt = None;
        let connected = match successor.take() {
            Some((core, opened)) => {
                prebuilt = Some(opened);
                Ok(core)
            }
            None => {
                update(&shared, &ctx, |s| {
                    s.disconnect();
                    s.status = CONNECTING.into();
                });
                match live.take() {
                    Some(core) => Ok(core),
                    None => launcher.connect_or_start_with_endpoint().await,
                }
            }
        };
        let connected_at = std::time::Instant::now();
        let (client, endpoint) = match connected {
            Ok(connected) => (connected.client, connected.endpoint),
            Err(e) => {
                tracing::warn!(
                    stage = "connect-or-start",
                    error = %e,
                    since_loss_ms = ?episode.since_loss_ms(connected_at),
                    "could not connect to or start the core"
                );
                update(&shared, &ctx, |s| {
                    s.status = auto_reconnect::launch_failure(&e)
                });
                if !wait_reconnect(&mut actions, &shared, &ctx).await {
                    return;
                }
                continue;
            }
        };
        reconcile_uncertain(&client, &shared, &ctx, &mut uncertain, &mut held).await;
        let mut data = DataReaders::default();
        let mut resync = true;
        let mut reconnect_requested = false;
        let mut orderly = false;
        // The core's own notice that ended the subscription, for the log and
        // for the loss it is when it is not a goodbye.
        let mut ended_by: Option<String> = None;
        'session: while resync {
            data.stop(&shared, &ctx).await;
            resync = false;
            let opened = match prebuilt.take() {
                Some(opened) => Ok(opened),
                None => open_session(&client).await,
            };
            let mut stream = match opened {
                Ok(Opened {
                    stream,
                    capabilities,
                    cache,
                    delivered,
                }) => {
                    update(&shared, &ctx, |s| {
                        s.begin_session(cache, capabilities, &delivered, std::mem::take(&mut held));
                    });
                    episode.recovered(std::time::Instant::now());
                    ended_by = None;
                    stream
                }
                Err(OpenFailed::Subscribe(error)) => {
                    tracing::warn!(stage = "subscribe", %error, since_loss_ms = ?episode.since_loss_ms(std::time::Instant::now()), "could not subscribe to the core");
                    update(&shared, &ctx, |s| s.status = error);
                    break;
                }
                Err(OpenFailed::Ended(events)) => {
                    // The core spoke while the snapshot was paged: its goodbye
                    // and the evidence of a restart count as if they had come
                    // through the session loop, and the session is over. Any
                    // other ending (a resync notice, a slow subscriber) is not
                    // the person's doing: it is a loss, reconnected below.
                    for event in events {
                        if event.event.ends_subscription() {
                            ended_by = Some(format!("{:?}", event.event));
                        }
                        orderly |= auto_reconnect::is_orderly_shutdown(&event.event);
                        update(&shared, &ctx, |s| {
                            let _ = s.cache.event_delivering(event, |e| s.features.event(e));
                        });
                    }
                    break;
                }
                Err(OpenFailed::Snapshot(error)) => {
                    tracing::warn!(stage = "snapshot", %error, since_loss_ms = ?episode.since_loss_ms(std::time::Instant::now()), "could not read the snapshot from the core");
                    update(&shared, &ctx, |s| s.status = error);
                    if matches!(
                        client.connection_state(),
                        workbench_client::ConnectionState::Connected
                    ) {
                        resync = true;
                        tokio::task::yield_now().await;
                    }
                    continue;
                }
            };
            loop {
                let upload = next_upload(&shared, &uncertain);
                tokio::select! {
                    _ = client.closed() => break,
                    () = std::future::ready(()), if upload.is_some() => {
                        let (command, pending) = upload.expect("upload branch enabled");
                        send_command(&client, &shared, &ctx, &mut uncertain, command, pending).await;
                    }
                    event = stream.next() => {
                        let Some(event) = event else { break };
                        orderly |= auto_reconnect::is_orderly_shutdown(&event.event);
                        let ending = event.event.ends_subscription().then(|| format!("{:?}", event.event));
                        let mut valid = false;
                        update(&shared, &ctx, |s| valid = s.cache.event_delivering(event, |e| s.features.event(e)));
                        if !valid {
                            if let Some(ending) = ending {
                                tracing::info!(stage = "session", ended_by = %ending, "the core ended the subscription; reading the session again");
                                ended_by = Some(ending);
                            }
                            resync = true;
                            break;
                        }
                    }
                    action = actions.recv() => {
                        let Some(action) = action else { data.stop(&shared, &ctx).await; discard_unused(&client, &shared).await; release_own(&client, &shared).await; return };
                        if matches!(action, Action::Reconnect) { reconnect_requested = true; break 'session; }
                        if matches!(action, Action::Shutdown) { data.stop(&shared, &ctx).await; discard_unused(&client, &shared).await; release_own(&client, &shared).await; return; }
                        match action {
                            Action::Command { command, draft } => {
                                let takes_files = takes_files(&command);
                                send_command(&client, &shared, &ctx, &mut uncertain, command, Pending::Command { draft, takes_files }).await;
                            }
                            Action::Feature(FeatureAction { feature, tag, request }) => match request {
                                FeatureRequest::Command(command) => {
                                    send_command(&client, &shared, &ctx, &mut uncertain, command, Pending::Feature { feature, tag }).await;
                                }
                                FeatureRequest::Query(query) => {
                                    let result = client.query(query).await.map_err(|e| feature_error(&e));
                                    update(&shared, &ctx, |s| s.features.reply(feature, tag, FeatureReply::Query(result)));
                                }
                            },
                            Action::Data(action) => data.handle(action, &endpoint, &client, &shared, &ctx).await,
                            Action::History(thread, run, older) => {
                                let cursor = if older {
                                    let state = shared.lock().unwrap();
                                    match &run { None => state.cache.older.get(thread.as_str()), Some(run) => state.cache.run_older.get(run.as_str()) }.cloned().flatten()
                                } else { None };
                                let reply = client.query(Query::Conversation(ConversationQuery::ReadCommittedHistory(ReadCommittedHistory {
                                    thread: thread.clone(), run: run.clone(), direction: HistoryDirection::Backward,
                                    page: PageRequest { cursor, ..Default::default() },
                                }))).await;
                                match reply {
                                    Ok(QueryReply::Conversation(ConversationReply::CommittedHistory(page))) => update(&shared, &ctx, |s| match &run { None => s.cache.history_page(thread.as_str(), page, older), Some(run) => s.cache.run_history_page(run.as_str(), page, older) }),
                                    other => update(&shared, &ctx, |s| {
                                        s.status = format!("Could not read the history: {}", failure_reason(&other));
                                        if older { s.cache.older_failed(run.as_ref().map_or(thread.as_str(), |run| run.as_str())); }
                                        else if let Some(run) = &run { s.cache.run_failed(run.as_str()); }
                                    }),
                                }
                                // Receipts belong to the thread, not to one run of it.
                                let mut cursor = None;
                                while run.is_none() && let Ok(QueryReply::Conversation(ConversationReply::InputReceipts(page))) = client.query(Query::Conversation(ConversationQuery::GetInputReceipts(workbench_protocol::queries::ThreadQuery { thread: thread.clone(), page: PageRequest { cursor, ..Default::default() } }))).await {
                                    cursor = page.next_cursor;
                                    update(&shared, &ctx, |s| { for receipt in page.items { s.cache.receipts.insert(receipt.submission_id.to_string(), receipt); } });
                                    if cursor.is_none() { break; }
                                }
                            }
                            Action::Body(id) => {
                                match body(&client, id.clone()).await {
                                    Ok(text) => update(&shared, &ctx, |s| { s.cache.bodies.insert(id.to_string(), text); }),
                                    Err(e) => update(&shared, &ctx, |s| s.status = e),
                                }
                            }
                            Action::ApprovalInput(approval) => {
                                let mut offset = ByteOffset(0);
                                let mut text = String::new();
                                loop {
                                    let query = workbench_protocol::queries::GetApprovalInput { approval_id: approval.id.clone(), core_epoch: approval.core_epoch, approval_revision: approval.revision, offset, max_bytes: 65536.min(client.server_hello().limits.max_body_chunk_bytes) };
                                    match client.query(Query::Approvals(ApprovalsQuery::GetApprovalInput(query))).await {
                                        Ok(QueryReply::Approvals(ApprovalsReply::ApprovalInput(chunk))) if chunk.offset == offset && chunk.approval_id == approval.id && chunk.core_epoch == approval.core_epoch && chunk.approval_revision == approval.revision => {
                                            text.push_str(&chunk.text);
                                            if text.len() > 1024 * 1024 { break; }
                                            match chunk.next_offset { Some(next) if next > offset => offset = next, None => { update(&shared, &ctx, |s| { s.cache.approval_inputs.insert(approval.id.to_string(), text); }); break; }, _ => break }
                                        }
                                        other => { update(&shared, &ctx, |s| s.status = format!("Could not read the approval input: {}", failure_reason(&other))); break; }
                                    }
                                }
                            }
                            Action::Upload { project, thread, file } => {
                                update(&shared, &ctx, |s| {
                                    let draft = s.attachment_draft(project, thread.as_str(), file);
                                    if draft.reference.is_none() && !draft.uncertain { draft.uploading = true; }
                                });
                            }
                            Action::Reconnect | Action::Shutdown => unreachable!(),
                        }
                    }
                }
            }
            // `closed()` and what the core queued before it closed (its goodbye,
            // the evidence of a restart) can both be ready, and the select may pick
            // the close. Read what already arrived before deciding how the core
            // was lost.
            if !resync {
                while let Ok(Some(event)) =
                    tokio::time::timeout(std::time::Duration::ZERO, stream.next()).await
                {
                    orderly |= auto_reconnect::is_orderly_shutdown(&event.event);
                    update(&shared, &ctx, |s| {
                        let _ = s.cache.event_delivering(event, |e| s.features.event(e));
                    });
                }
            }
        }
        // Read before the features are disconnected: only the core's own evidence
        // of a restart counts, never the loss of the connection alone.
        #[cfg(target_os = "linux")]
        let evidence = (!reconnect_requested)
            .then(|| {
                let epoch = client.server_hello().instance_epoch;
                let now = core_launcher::handoff::diagnostics::reconnect::clock_now();
                shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .features
                    .planned_restart(epoch, now)
            })
            .flatten();
        #[cfg(target_os = "linux")]
        let planned = evidence.is_some();
        #[cfg(not(target_os = "linux"))]
        let planned = false;
        // Lost for real, with neither the core's goodbye nor restart evidence:
        // EOF alone is not a restart, but it is not an orderly stop either. A
        // subscription the core ended without a goodbye is the same loss even
        // while the connection still stands: nothing else will read the session.
        let abnormal = !reconnect_requested
            && !planned
            && !orderly
            && (ended_by.is_some()
                || matches!(
                    client.connection_state(),
                    workbench_client::ConnectionState::Closed(_)
                ));
        let closed = match client.connection_state() {
            workbench_client::ConnectionState::Closed(error) => Some(error),
            workbench_client::ConnectionState::Connected => None,
        };
        let session = connected_at.elapsed();
        episode.session_ended(
            std::time::Instant::now(),
            &SessionEnd {
                transport: closed.as_ref(),
                session,
                orderly,
                planned_restart: planned,
                reconnect_requested,
                resync,
                abnormal,
                ended_by: ended_by.as_deref(),
            },
        );
        data.stop(&shared, &ctx).await;
        update(&shared, &ctx, |s| s.disconnect());
        if reconnect_requested {
            continue;
        }
        #[cfg(target_os = "linux")]
        if let Some(evidence) = evidence {
            match wait_for_successor(&launcher, &evidence, &mut actions, &shared, &ctx).await {
                Waited::Next(next) => {
                    successor = Some(*next);
                    continue;
                }
                Waited::Reconnect => continue,
                Waited::Stop => return,
                Waited::Failed => {}
            }
        }
        if abnormal {
            // A core that still answers lost only the connection: reconnecting
            // starts nothing and spends no crash budget, so it is not held to
            // the allowance meant for a core that keeps dying, only paced.
            let probed = match await_probe(
                launcher.connect_existing(LIVE_CORE_PROBE),
                &mut actions,
                &shared,
                &ctx,
            )
            .await
            {
                Probed::Done(probed) => probed,
                Probed::Reconnect => continue,
                Probed::Stop => return,
            };
            match probed {
                Ok(Some(core)) => {
                    let delay = live_backoff.next_delay(session);
                    tracing::info!(
                        stage = "live-core",
                        live_attempt = live_backoff.attempts(),
                        crash_budget_attempts_in_window = auto.used(std::time::Instant::now()),
                        delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                        since_loss_ms = ?episode.since_loss_ms(std::time::Instant::now()),
                        "the core is still running; reconnecting to it"
                    );
                    update(&shared, &ctx, |s| {
                        s.status = auto_reconnect::reconnecting_live(delay)
                    });
                    match pause_for_reconnect(delay, &mut actions, &shared, &ctx).await {
                        Paused::Elapsed | Paused::Reconnect => {
                            live = Some(core);
                            continue;
                        }
                        Paused::Stop => return,
                    }
                }
                Ok(None) => tracing::info!(
                    stage = "live-core",
                    crash_budget_attempts_in_window = auto.used(std::time::Instant::now()),
                    "no running core answered; the start is left to the crash budget"
                ),
                Err(error) => tracing::info!(
                    stage = "live-core",
                    crash_budget_attempts_in_window = auto.used(std::time::Instant::now()),
                    %error,
                    "the running core could not be probed; the start is left to the crash budget"
                ),
            }
            match auto.attempt(std::time::Instant::now()) {
                Some(attempt) => {
                    tracing::warn!(
                        stage = "auto-reconnect",
                        attempt,
                        limit = auto_reconnect::MAX_ATTEMPTS,
                        since_loss_ms = ?episode.since_loss_ms(std::time::Instant::now()),
                        "automatic reconnect attempt"
                    );
                    update(&shared, &ctx, |s| {
                        s.status = auto_reconnect::retrying(attempt)
                    });
                    continue;
                }
                None => {
                    tracing::warn!(
                        stage = "auto-reconnect",
                        limit = auto_reconnect::MAX_ATTEMPTS,
                        since_loss_ms = ?episode.since_loss_ms(std::time::Instant::now()),
                        "automatic reconnection gave up; waiting for a manual reconnect"
                    );
                    update(&shared, &ctx, |s| s.status = auto_reconnect::gave_up());
                }
            }
        } else if orderly && !planned {
            update(&shared, &ctx, |s| s.status = auto_reconnect::orderly_stop());
        }
        if !wait_reconnect(&mut actions, &shared, &ctx).await {
            return;
        }
    }
}

/// A session read from a core: the subscription, what the core offers and the
/// snapshot the projection is rebuilt from.
struct Opened {
    stream: Subscription,
    capabilities: Vec<workbench_protocol::handshake::Capability>,
    cache: Cache,
    delivered: Vec<UiEvent>,
}

enum OpenFailed {
    /// No subscription: the connection is of no use.
    Subscribe(String),
    /// The snapshot did not settle; a resync may.
    Snapshot(String),
    /// The core ended the subscription while the snapshot was paged; what it
    /// sent first, its ending last.
    Ended(Vec<EventEnvelope>),
}

async fn open_session(client: &WorkbenchClient) -> Result<Opened, OpenFailed> {
    let mut stream = client
        .subscribe(Subscribe {
            scope: SubscriptionScope::All,
            after: None,
        })
        .await
        .map_err(|e| OpenFailed::Subscribe(e.to_string()))?;
    let capabilities = client.server_hello().capabilities.clone();
    let (cache, delivered) = snapshot_delivering(client, &mut stream)
        .await
        .map_err(|fault| match fault {
            SnapshotFault::Failed(error) => OpenFailed::Snapshot(error),
            SnapshotFault::Ended(events) => OpenFailed::Ended(events),
        })?;
    Ok(Opened {
        stream,
        capabilities,
        cache,
        delivered,
    })
}

#[cfg(target_os = "linux")]
enum Waited {
    Next(Box<(core_launcher::ConnectedCore, Opened)>),
    /// The person asked to reconnect by hand.
    Reconnect,
    /// The wait ended with the reason shown; nothing was started.
    Failed,
    Stop,
}

/// After a planned restart: wait for its successor, connect-only. Nothing here
/// starts a core, so the restart can neither be raced nor duplicated. The two
/// windows (120 s for the successor to be ready, 30 s to rebuild this session)
/// are the pure machine's; the session is read inside the second one. Whatever
/// the person does meanwhile is refused as for any lost connection.
#[cfg(target_os = "linux")]
async fn wait_for_successor(
    launcher: &core_launcher::Launcher,
    evidence: &workbench_client::restart::reconnect::PlannedEvidence,
    actions: &mut mpsc::Receiver<Action>,
    shared: &Arc<Mutex<Shared>>,
    ctx: &egui::Context,
) -> Waited {
    use super::restart::text;
    use core_launcher::handoff::diagnostics::reconnect::{
        ReconnectError, ReconnectPolicy, Reconnected, Target, reconnect_after_planned_restart,
    };
    let target = launcher.connect_target();
    let connector = Target {
        target: &target,
        client: launcher.client_config(),
    };
    let waiting = reconnect_after_planned_restart(
        &connector,
        evidence,
        ReconnectPolicy::default(),
        |view| {
            update(shared, ctx, |s| {
                s.status = text::reconnect(view.stage, view.remaining);
            });
        },
        |core| async move {
            let core_launcher::ConnectedCore { client, endpoint } = core;
            match open_session(&client).await {
                Ok(opened) => Ok((core_launcher::ConnectedCore { client, endpoint }, opened)),
                Err(OpenFailed::Subscribe(error) | OpenFailed::Snapshot(error)) => Err(error),
                Err(OpenFailed::Ended(_)) => Err("the core ended the subscription".into()),
            }
        },
    );
    tokio::pin!(waiting);
    let result = loop {
        tokio::select! {
            result = &mut waiting => break result,
            action = actions.recv() => match action {
                None | Some(Action::Shutdown) => return Waited::Stop,
                Some(Action::Reconnect) => return Waited::Reconnect,
                Some(action) => refuse_while_disconnected(shared, ctx, action, false),
            },
        }
    };
    match result {
        Ok(Reconnected::Successor { value, .. } | Reconnected::Predecessor { value }) => {
            Waited::Next(Box::new(value))
        }
        Err(error) => {
            update(shared, ctx, |s| {
                s.status = match &error {
                    ReconnectError::Expired(stage) => text::expired(*stage).to_owned(),
                    other => format!("Could not confirm that the restart finished: {other}"),
                };
            });
            Waited::Failed
        }
    }
}

/// Ask the core what became of every command whose reply was lost, never
/// resending any. What a feature's command came to is held (not delivered):
/// the snapshot that follows resets the features, and the reply has to come
/// after that reset.
async fn reconcile_uncertain(
    client: &WorkbenchClient,
    shared: &Arc<Mutex<Shared>>,
    ctx: &egui::Context,
    uncertain: &mut Vec<(RequestId, Pending)>,
    held: &mut Vec<HeldReply>,
) {
    for (request, pending) in std::mem::take(uncertain) {
        match client
            .query(Query::Projection(ProjectionQuery::GetCommandReceipt {
                request_id: request.clone(),
            }))
            .await
        {
            Ok(QueryReply::Projection(ProjectionReply::CommandReceipt(receipt))) => {
                match receipt.state {
                    CommandReceiptState::Applied { reply } => {
                        update(shared, ctx, |s| {
                            s.outcome_unknown.remove(request.as_str());
                        });
                        match pending {
                            Pending::Feature { feature, tag } => held.push(HeldReply {
                                feature,
                                tag,
                                reply: FeatureReply::Command(Ok(reply)),
                            }),
                            pending => accepted(client, shared, ctx, reply, pending).await,
                        }
                    }
                    CommandReceiptState::Rejected { error } => update(shared, ctx, |s| {
                        s.outcome_unknown.remove(request.as_str());
                        match &pending {
                            Pending::Feature { feature, tag } => held.push(HeldReply {
                                feature: *feature,
                                tag: *tag,
                                reply: FeatureReply::Command(Err(FeatureError::new(
                                    error.to_string(),
                                ))),
                            }),
                            pending => pending.rejected(s, &error.to_string()),
                        }
                        s.status = error.to_string();
                    }),
                    CommandReceiptState::Unknown => {
                        update(shared, ctx, |s| {
                            s.status = format!("Outcome unknown: {request}. Not resent.")
                        });
                        uncertain.push((request, pending));
                    }
                }
            }
            result => {
                update(shared, ctx, |s| {
                    s.status = format!(
                        "Could not check the receipt (not resent): {}",
                        failure_reason(&result)
                    )
                });
                uncertain.push((request, pending));
            }
        }
    }
}

/// Send one command. A lost reply is reconciled after the next connect through
/// its receipt, never replayed.
fn feature_error(error: &ClientError) -> FeatureError {
    let refused = FeatureError::new(error.to_string());
    match error {
        ClientError::Server(server) => refused.with_code(server.code).with_retry(server.retry),
        _ => refused,
    }
}

async fn send_command(
    client: &WorkbenchClient,
    shared: &Arc<Mutex<Shared>>,
    ctx: &egui::Context,
    uncertain: &mut Vec<(RequestId, Pending)>,
    command: Command,
    pending: Pending,
) {
    match client.command(command).await {
        Ok(reply) => accepted(client, shared, ctx, reply, pending).await,
        Err(ClientError::Transport(TransportError::OutcomeUnknown { request_id })) => {
            uncertain.push((request_id.clone(), pending.clone()));
            update(shared, ctx, |s| {
                s.outcome_unknown.insert(request_id.to_string());
                if let Pending::Upload { thread, id, .. } = &pending
                    && let Some(draft) = s
                        .attachment_drafts
                        .get_mut(thread)
                        .into_iter()
                        .flatten()
                        .find(|draft| &draft.id == id)
                {
                    draft.uncertain = true;
                }
                s.status = format!(
                    "Outcome unknown: {request_id}. Its receipt is checked after reconnecting."
                );
                if let Pending::Feature { feature, tag } = &pending {
                    s.features.reply(
                        *feature,
                        *tag,
                        FeatureReply::Command(Err(FeatureError::outcome_unknown(format!(
                            "OutcomeUnknown: {request_id}"
                        )))),
                    );
                }
            });
        }
        Err(e) => update(shared, ctx, |s| {
            pending.rejected_with(s, feature_error(&e));
            s.status = e.to_string();
        }),
    }
}

/// A clean exit lets go of what only this client holds in the core (its preview tabs, and
/// with them its frame subscriptions). Nothing else is stopped: the runs, their tabs and the
/// core keep running, and a kill, which sends nothing, ends the same way after the core's own
/// grace. Best effort: an unanswered command is not retried.
async fn release_own(client: &WorkbenchClient, shared: &Arc<Mutex<Shared>>) {
    let commands = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .features
        .release_commands();
    for command in commands {
        let _ = client.command(command).await;
    }
}

async fn discard_unused(client: &WorkbenchClient, shared: &Arc<Mutex<Shared>>) {
    let images: Vec<_> = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .attachment_drafts
        .values()
        .flatten()
        .filter_map(|file| file.upload_id.clone())
        .collect();
    for image in images {
        let _ = client
            .command(Command::Conversation(
                ConversationCommand::DiscardAttachment {
                    attachment_id: image,
                },
            ))
            .await;
    }
}

async fn accepted(
    client: &WorkbenchClient,
    shared: &Arc<Mutex<Shared>>,
    ctx: &egui::Context,
    reply: CommandReply,
    pending: Pending,
) {
    let mut discarded = Vec::new();
    update(shared, ctx, |s| {
        let mut reply_error = None;
        let (draft, takes_files) = match pending {
            Pending::Command { draft, takes_files } => (draft, takes_files),
            Pending::Feature { feature, tag } => {
                s.features
                    .reply(feature, tag, FeatureReply::Command(Ok(reply.clone())));
                (None, false)
            }
            Pending::Upload {
                thread,
                id,
                expected,
            } => {
                if let Some(workbench_protocol::commands::CommandResult::Conversation(result)) =
                    reply.result.as_deref()
                {
                    if let Some(file) = s
                        .attachment_drafts
                        .get_mut(&thread)
                        .into_iter()
                        .flatten()
                        .find(|file| file.id == id)
                    {
                        if let Err(error) = file.acknowledge(expected, result) {
                            file.uploading = false;
                            file.uncertain = false;
                            reply_error = Some(error);
                        }
                    } else {
                        match result {
                            ConversationResult::AttachmentBegun { upload_id, .. } => {
                                discarded.push(upload_id.clone())
                            }
                            ConversationResult::AttachmentFinished(reference) => {
                                discarded.push(reference.id.clone())
                            }
                            _ => {}
                        }
                    }
                } else {
                    if let Some(file) = s
                        .attachment_drafts
                        .get_mut(&thread)
                        .into_iter()
                        .flatten()
                        .find(|file| file.id == id)
                    {
                        file.uploading = false;
                        file.uncertain = false;
                    }
                    reply_error = Some("Attachment command returned no upload result".into());
                }
                (None, false)
            }
        };
        if let Some((thread, _)) = &draft {
            s.sending.remove(thread);
            // The thread exists in the core now: its screen is the thread's own from here,
            // wherever the person is. A refusal never gets here, so its draft stays a new one.
            s.new_conversations.retain(|_, new| new.as_str() != thread);
        }
        if let Some((thread, _)) = &draft
            && takes_files
        {
            discarded.extend(
                s.attachment_drafts
                    .remove(thread)
                    .into_iter()
                    .flatten()
                    .filter_map(|file| file.upload_id),
            );
        }
        if let Some((thread, revision)) = draft
            && let Some(draft) = s.drafts.get_mut(&thread)
            && draft.revision == revision
        {
            draft.text.clear();
            draft.revision += 1;
        }
        s.status = reply_error.unwrap_or_else(|| {
            match reply.disposition {
                CommandDisposition::Accepted => "Accepted",
                CommandDisposition::AlreadyApplied => "Already applied",
            }
            .into()
        });
    });
    for id in discarded {
        // Discard never deletes committed files or replays user input.
        let _ = client
            .command(Command::Conversation(
                ConversationCommand::DiscardAttachment { attachment_id: id },
            ))
            .await;
    }
}

/// Only a message carries the thread's files; a slash command sent from the
/// same composer leaves them for the next message.
fn takes_files(command: &Command) -> bool {
    matches!(
        command,
        Command::Conversation(
            ConversationCommand::SendChat(_) | ConversationCommand::CreateExecution(_)
        )
    )
}

fn next_upload(
    shared: &Arc<Mutex<Shared>>,
    uncertain: &[(RequestId, Pending)],
) -> Option<(Command, Pending)> {
    let state = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (thread, drafts) in &state.attachment_drafts {
        for draft in drafts {
            if uncertain.iter().any(|(_, pending)| matches!(pending, Pending::Upload { thread: pending_thread, id, .. } if pending_thread == thread && id == &draft.id)) { continue; }
            if let Some((command, expected)) = draft.next_command(thread) {
                return Some((
                    command,
                    Pending::Upload {
                        thread: thread.clone(),
                        id: draft.id.clone(),
                        expected,
                    },
                ));
            }
        }
    }
    None
}

/// How long a running core has to answer the probe that tells it from a dead one.
const LIVE_CORE_PROBE: std::time::Duration = std::time::Duration::from_secs(5);

enum Probed<T> {
    Done(T),
    /// The person asked to reconnect by hand while the probe ran.
    Reconnect,
    Stop,
}

/// Ask whether a core still answers, without making the window wait for the
/// answer: a person's reconnect or the window closing is taken at once.
async fn await_probe<T>(
    probe: impl std::future::Future<Output = T>,
    actions: &mut mpsc::Receiver<Action>,
    shared: &Arc<Mutex<Shared>>,
    ctx: &egui::Context,
) -> Probed<T> {
    tokio::pin!(probe);
    loop {
        tokio::select! {
            probed = &mut probe => return Probed::Done(probed),
            action = actions.recv() => match action {
                None | Some(Action::Shutdown) => return Probed::Stop,
                Some(Action::Reconnect) => return Probed::Reconnect,
                Some(action) => refuse_while_disconnected(shared, ctx, action, false),
            },
        }
    }
}

enum Paused {
    Elapsed,
    /// The person asked to reconnect by hand: the pace does not apply.
    Reconnect,
    Stop,
}

/// Wait out the reconnect pace while the window stays usable: whatever the
/// person does is refused as for any lost connection.
async fn pause_for_reconnect(
    delay: std::time::Duration,
    actions: &mut mpsc::Receiver<Action>,
    shared: &Arc<Mutex<Shared>>,
    ctx: &egui::Context,
) -> Paused {
    let pace = tokio::time::sleep(delay);
    tokio::pin!(pace);
    loop {
        tokio::select! {
            () = &mut pace => return Paused::Elapsed,
            action = actions.recv() => match action {
                None | Some(Action::Shutdown) => return Paused::Stop,
                Some(Action::Reconnect) => return Paused::Reconnect,
                Some(action) => refuse_while_disconnected(shared, ctx, action, false),
            },
        }
    }
}

async fn wait_reconnect(
    actions: &mut mpsc::Receiver<Action>,
    shared: &Arc<Mutex<Shared>>,
    ctx: &egui::Context,
) -> bool {
    while let Some(action) = actions.recv().await {
        if matches!(action, Action::Reconnect) {
            return true;
        }
        if matches!(action, Action::Shutdown) {
            return false;
        }
        refuse_while_disconnected(shared, ctx, action, true);
    }
    false
}

/// Nothing is sent while there is no session: the action is refused and the
/// human input it came from is kept.
fn refuse_while_disconnected(
    shared: &Arc<Mutex<Shared>>,
    ctx: &egui::Context,
    action: Action,
    say: bool,
) {
    update(shared, ctx, |s| {
        match action {
            Action::Command {
                draft: Some((thread, _)),
                ..
            } => {
                s.sending.remove(&thread);
            }
            Action::Upload {
                project,
                thread,
                file,
                ..
            } => {
                s.attachment_draft(project, thread.as_str(), file).uploading = false;
            }
            Action::Feature(action) => s.features.refuse(&action, "disconnected"),
            Action::Data(action) if s.features.data.current(&action) => {
                s.features.data_notice(
                    action.feature,
                    action.tag,
                    super::features::data::DataNotice::Failed(FeatureError::new("disconnected")),
                );
            }
            _ => (),
        }
        if say {
            s.status = NOT_SENT.into();
        }
    });
}

fn failure_reason<T>(result: &Result<T, impl std::fmt::Display>) -> String {
    match result {
        Ok(_) => "unexpected reply".into(),
        Err(error) => error.to_string(),
    }
}

async fn body(client: &WorkbenchClient, id: BodyId) -> Result<String, String> {
    let mut offset = ByteOffset(0);
    let mut text = String::new();
    loop {
        match client
            .query(Query::Projection(ProjectionQuery::ReadBody(ReadBody {
                body_id: id.clone(),
                offset,
                max_bytes: 65536.min(client.server_hello().limits.max_body_chunk_bytes),
            })))
            .await
        {
            Ok(QueryReply::Projection(ProjectionReply::Body(chunk)))
                if chunk.body_id == id && chunk.offset == offset =>
            {
                if text.len() + chunk.text.len() > 4 * 1024 * 1024 {
                    return Err("The message body is larger than 4 MiB (unsupported)".into());
                }
                text.push_str(&chunk.text);
                match chunk.next_offset {
                    Some(next) if next > offset => offset = next,
                    None => return Ok(text),
                    _ => return Err("body offset did not advance".into()),
                }
            }
            other => {
                return Err(format!(
                    "Could not read the message body: {}",
                    failure_reason(&other)
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn a_probe_of_the_core_does_not_hold_back_a_reconnect_or_the_window_closing() {
        let (tx, mut rx) = mpsc::channel(4);
        let shared = Arc::new(Mutex::new(Shared::default()));
        let ctx = egui::Context::default();

        // A probe that never answers: a person's reconnect and the close are taken at once.
        tx.send(Action::Reconnect).await.unwrap();
        assert!(matches!(
            await_probe(std::future::pending::<u8>(), &mut rx, &shared, &ctx).await,
            Probed::Reconnect
        ));
        tx.send(Action::Shutdown).await.unwrap();
        assert!(matches!(
            await_probe(std::future::pending::<u8>(), &mut rx, &shared, &ctx).await,
            Probed::Stop
        ));

        // An answer that arrives is returned.
        let (answer, answered) = tokio::sync::oneshot::channel();
        answer.send(7u8).unwrap();
        match await_probe(async { answered.await.unwrap() }, &mut rx, &shared, &ctx).await {
            Probed::Done(value) => assert_eq!(value, 7),
            _ => panic!("the probe answered"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_reconnect_pace_runs_out_with_time_and_a_manual_reconnect_ends_it_at_once() {
        let (tx, mut rx) = mpsc::channel(4);
        let shared = Arc::new(Mutex::new(Shared::default()));
        let ctx = egui::Context::default();
        let pace = std::time::Duration::from_secs(30);

        // Nothing happens meanwhile: virtual time runs the pace out.
        assert!(matches!(
            pause_for_reconnect(pace, &mut rx, &shared, &ctx).await,
            Paused::Elapsed
        ));

        // A command sent meanwhile is refused (its input kept), and the pace still runs out.
        shared.lock().unwrap().sending.insert("t".into());
        tx.send(Action::Command {
            command: Command::Execution(
                workbench_protocol::namespaces::ExecutionCommand::StopChat(
                    workbench_protocol::commands::ThreadTarget {
                        thread: ThreadId::new("t").unwrap(),
                    },
                ),
            ),
            draft: Some(("t".into(), 1)),
        })
        .await
        .unwrap();
        assert!(matches!(
            pause_for_reconnect(pace, &mut rx, &shared, &ctx).await,
            Paused::Elapsed
        ));
        assert!(!shared.lock().unwrap().sending.contains("t"));

        // A person's reconnect, and the window closing, end it without waiting.
        tx.send(Action::Reconnect).await.unwrap();
        assert!(matches!(
            pause_for_reconnect(pace, &mut rx, &shared, &ctx).await,
            Paused::Reconnect
        ));
        tx.send(Action::Shutdown).await.unwrap();
        assert!(matches!(
            pause_for_reconnect(pace, &mut rx, &shared, &ctx).await,
            Paused::Stop
        ));
    }

    #[tokio::test]
    async fn disconnect_keeps_unwritten_input_but_never_replays_queued_commands() {
        let (tx, mut rx) = mpsc::channel(4);
        let shared = Arc::new(Mutex::new(Shared::default()));
        shared.lock().unwrap().drafts.insert(
            "t".into(),
            super::super::Draft {
                text: "not written".into(),
                revision: 3,
            },
        );
        shared.lock().unwrap().sending.insert("t".into());
        tx.send(Action::Command {
            command: Command::Execution(
                workbench_protocol::namespaces::ExecutionCommand::StopChat(
                    workbench_protocol::commands::ThreadTarget {
                        thread: ThreadId::new("t").unwrap(),
                    },
                ),
            ),
            draft: Some(("t".into(), 3)),
        })
        .await
        .unwrap();
        tx.send(Action::Upload {
            project: ProjectId::new("p").unwrap(),
            thread: ThreadId::new("t").unwrap(),
            file: crate::attachments::AttachmentFile {
                bytes: b"retained local bytes".to_vec(),
                ..crate::fixture::attachment_draft().file
            },
        })
        .await
        .unwrap();
        tx.send(Action::Reconnect).await.unwrap();
        assert!(wait_reconnect(&mut rx, &shared, &egui::Context::default()).await);
        let state = shared.lock().unwrap();
        assert_eq!(state.drafts["t"].text, "not written");
        assert!(!state.sending.contains("t"));
        assert_eq!(
            state.attachment_drafts["t"][0].file.bytes,
            b"retained local bytes"
        );
        assert!(state.attachment_drafts["t"][0].reference.is_none());
    }

    #[tokio::test]
    async fn an_accepted_slash_command_clears_its_text_and_leaves_the_files() {
        use workbench_protocol::envelope::{ReplyPayload, ReplyResult};
        let shared = Arc::new(Mutex::new(Shared::default()));
        {
            let mut state = shared.lock().unwrap();
            state.drafts.insert(
                "t".into(),
                super::super::Draft {
                    text: "/compaction".into(),
                    revision: 2,
                },
            );
            state.sending.insert("t".into());
            state.attachment_draft(
                ProjectId::new("p").unwrap(),
                "t",
                crate::fixture::attachment_draft().file,
            );
        }
        let command = Command::Conversation(ConversationCommand::CompactChat(
            workbench_protocol::commands::ThreadTarget {
                thread: ThreadId::new("t").unwrap(),
            },
        ));
        let pending = Pending::Command {
            draft: Some(("t".into(), 2)),
            takes_files: takes_files(&command),
        };
        let (client, mut core) = connect().await;
        let (ctx, mut uncertain) = (egui::Context::default(), Vec::new());
        tokio::join!(
            send_command(&client, &shared, &ctx, &mut uncertain, command, pending,),
            async {
                let request = core.recv_request().await.unwrap();
                core.reply(
                    &request.request_id,
                    ReplyResult::Success(ReplyPayload::Command(CommandReply::accepted())),
                );
            },
        );
        let state = shared.lock().unwrap();
        assert_eq!(state.drafts["t"].text, "");
        assert!(!state.sending.contains("t"));
        assert_eq!(state.attachment_drafts["t"].len(), 1);
    }

    // Given: new conversations of two projects, the first one's goal being created, and
    //          its text edited and a file added while the core had not answered
    // When: the core accepts the goal
    // Then: only that conversation stops being new, and the edit, the file and the other
    //       project's draft stay
    #[tokio::test]
    async fn an_accepted_first_goal_ends_only_its_own_new_conversation() {
        use workbench_protocol::envelope::{ReplyPayload, ReplyResult};
        let (project, thread) = (ProjectId::new("p").unwrap(), ThreadId::new("t").unwrap());
        let shared = Arc::new(Mutex::new(Shared::default()));
        {
            let mut state = shared.lock().unwrap();
            state
                .new_conversations
                .insert(project.clone(), thread.clone());
            state
                .new_conversations
                .insert(ProjectId::new("q").unwrap(), ThreadId::new("u").unwrap());
            state.drafts.insert(
                "t".into(),
                super::super::Draft {
                    text: "/goal ship it, and more".into(),
                    revision: 3,
                },
            );
            state.sending.insert("t".into());
            state.attachment_draft(
                project.clone(),
                "t",
                crate::fixture::attachment_draft().file,
            );
        }
        let command = Command::Conversation(ConversationCommand::CreateGoal(
            workbench_protocol::commands::SubmitGoal {
                project,
                thread,
                objective: "ship it".into(),
                references: vec![],
                constraints: vec![],
                delegation: None,
            },
        ));
        let pending = Pending::Command {
            draft: Some(("t".into(), 2)),
            takes_files: takes_files(&command),
        };
        let (client, mut core) = connect().await;
        let (ctx, mut uncertain) = (egui::Context::default(), Vec::new());
        tokio::join!(
            send_command(&client, &shared, &ctx, &mut uncertain, command, pending),
            async {
                let request = core.recv_request().await.unwrap();
                core.reply(
                    &request.request_id,
                    ReplyResult::Success(ReplyPayload::Command(CommandReply::accepted())),
                );
            },
        );
        let state = shared.lock().unwrap();
        let still_new: Vec<_> = state
            .new_conversations
            .keys()
            .map(ProjectId::as_str)
            .collect();
        assert_eq!(still_new, ["q"]);
        assert_eq!(state.drafts["t"].text, "/goal ship it, and more");
        assert!(!state.sending.contains("t"));
        assert_eq!(state.attachment_drafts["t"].len(), 1);
    }

    /// Remembers what the features were told, in order.
    struct Probe(Arc<Mutex<Vec<String>>>);

    impl super::super::features::Feature for Probe {
        fn id(&self) -> FeatureId {
            FeatureId::Mcp
        }
        fn title(&self) -> &'static str {
            "probe"
        }
        fn reset(&mut self) {
            self.0.lock().unwrap().push("reset".into());
        }
        fn reply(&mut self, tag: u64, reply: FeatureReply) {
            let outcome = match reply {
                FeatureReply::Command(Ok(_)) => "applied",
                FeatureReply::Command(Err(e)) if e.outcome_unknown => "unknown",
                _ => "other",
            };
            self.0
                .lock()
                .unwrap()
                .push(format!("reply {tag} {outcome}"));
        }
    }

    pub(super) async fn connect() -> (WorkbenchClient, workbench_client::fake::FakeServer) {
        connect_epoch(7).await
    }

    pub(super) async fn connect_epoch(
        epoch: u64,
    ) -> (WorkbenchClient, workbench_client::fake::FakeServer) {
        use workbench_protocol::handshake::{
            ActivityCounts, BuildInfo, Capability, CoreState, CoreStatus, ProtocolVersion,
            ServerHello, WireLimits,
        };
        let hello = ServerHello {
            protocol_version: ProtocolVersion::CURRENT,
            capabilities: Capability::IMPLEMENTED.to_vec(),
            server_build: BuildInfo {
                name: "test".into(),
                version: "0".into(),
                revision: None,
            },
            core: workbench_protocol::ids::CoreIdentity::new("core-1").unwrap(),
            instance_epoch: workbench_protocol::wire::InstanceEpoch(epoch),
            limits: WireLimits::default(),
            status: CoreStatus {
                state: CoreState::Ready,
                retention_reasons: vec![],
                running: ActivityCounts::default(),
            },
        };
        let (transport, mut core) = workbench_client::fake::FakeTransport::pair();
        let (client, _) = tokio::join!(
            async {
                WorkbenchClient::connect(transport, super::super::client_config())
                    .await
                    .unwrap()
            },
            core.handshake(&hello),
        );
        (client, core)
    }

    #[tokio::test]
    async fn a_resolved_feature_reply_survives_the_reset_after_reconnect_and_nothing_is_resent() {
        use workbench_protocol::{
            envelope::{ReplyPayload, ReplyResult, RequestBody},
            queries::CommandReceipt,
        };
        let log = Arc::new(Mutex::new(Vec::new()));
        let shared = Arc::new(Mutex::new(Shared::default()));
        shared
            .lock()
            .unwrap()
            .features
            .install(Box::new(Probe(log.clone())));
        let ctx = egui::Context::default();
        let (mut uncertain, mut held) = (Vec::new(), Vec::new());

        // The core receives the command and the connection drops before the reply.
        let (client, mut core) = connect().await;
        let command =
            Command::Execution(workbench_protocol::namespaces::ExecutionCommand::StopChat(
                workbench_protocol::commands::ThreadTarget {
                    thread: ThreadId::new("t").unwrap(),
                },
            ));
        let (_, request_id) = tokio::join!(
            send_command(
                &client,
                &shared,
                &ctx,
                &mut uncertain,
                command,
                Pending::Feature {
                    feature: FeatureId::Mcp,
                    tag: 5
                },
            ),
            async {
                let request = core.recv_request().await.unwrap();
                core.close();
                request.request_id
            },
        );
        assert_eq!(uncertain.len(), 1);
        assert_eq!(uncertain[0].0, request_id);
        assert_eq!(*log.lock().unwrap(), ["reply 5 unknown"]);

        // After reconnecting, the receipt is asked for; the outcome is held, not delivered.
        let (client, mut core) = connect().await;
        let (_, requests) = tokio::join!(
            reconcile_uncertain(&client, &shared, &ctx, &mut uncertain, &mut held),
            async {
                let request = core.recv_request().await.unwrap();
                core.reply(
                    &request.request_id,
                    ReplyResult::Success(ReplyPayload::Query(QueryReply::Projection(
                        ProjectionReply::CommandReceipt(CommandReceipt {
                            request_id: request_id.clone(),
                            state: CommandReceiptState::Applied {
                                reply: CommandReply::accepted(),
                            },
                        }),
                    ))),
                );
                vec![request.body]
            },
        );
        assert!(uncertain.is_empty());
        assert_eq!(held.len(), 1);
        assert_eq!(*log.lock().unwrap(), ["reply 5 unknown"]);
        assert!(shared.lock().unwrap().outcome_unknown.is_empty());

        // The snapshot resets the features, and only then the held reply arrives.
        shared.lock().unwrap().begin_session(
            Cache::default(),
            vec![],
            &[],
            std::mem::take(&mut held),
        );
        assert_eq!(
            *log.lock().unwrap(),
            ["reply 5 unknown", "reset", "reply 5 applied"]
        );

        // The only thing sent on the new connection was the receipt query: the
        // command itself was never resent.
        drop(client);
        core.close();
        assert!(core.recv_request().await.is_none());
        assert!(matches!(
            requests.as_slice(),
            [RequestBody::Query(Query::Projection(
                ProjectionQuery::GetCommandReceipt { .. }
            ))]
        ));
    }
}
