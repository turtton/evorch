//! The single desktop actor. Closing the UI drops the connection; it never
//! schedules a stop. Only explicit human actions can issue state-changing wire
//! commands, and a command with a lost reply is reconciled, never replayed.
mod data;
use super::auto_reconnect::{self, AutoReconnect};
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
    commands::{Command, CommandReply},
    conversation::{ConversationCommand, ConversationResult},
    events::{EventEnvelope, Subscribe, SubscriptionScope, UiEvent},
    ids::{BodyId, ProjectId, RequestId, ThreadId},
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
    History(ThreadId, bool),
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
    Command(Option<(String, u64)>),
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
            Self::Command(Some((thread, _))) => {
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
                    s.status = "コアへ接続中…".into();
                });
                launcher.connect_or_start_with_endpoint().await
            }
        };
        let (client, endpoint) = match connected {
            Ok(connected) => (connected.client, connected.endpoint),
            Err(e) => {
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
                    stream
                }
                Err(OpenFailed::Subscribe(error)) => {
                    update(&shared, &ctx, |s| s.status = error);
                    break;
                }
                Err(OpenFailed::Ended(events)) => {
                    // The core spoke while the snapshot was paged: its goodbye
                    // and the evidence of a restart count as if they had come
                    // through the session loop, and the session is over.
                    for event in events {
                        orderly |= auto_reconnect::is_orderly_shutdown(&event.event);
                        update(&shared, &ctx, |s| {
                            let _ = s.cache.event_delivering(event, |e| s.features.event(e));
                        });
                    }
                    break;
                }
                Err(OpenFailed::Snapshot(error)) => {
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
                        let mut valid = false;
                        update(&shared, &ctx, |s| valid = s.cache.event_delivering(event, |e| s.features.event(e)));
                        if !valid { resync = true; break; }
                    }
                    action = actions.recv() => {
                        let Some(action) = action else { data.stop(&shared, &ctx).await; discard_unused(&client, &shared).await; release_own(&client, &shared).await; return };
                        if matches!(action, Action::Reconnect) { reconnect_requested = true; break 'session; }
                        if matches!(action, Action::Shutdown) { data.stop(&shared, &ctx).await; discard_unused(&client, &shared).await; release_own(&client, &shared).await; return; }
                        match action {
                            Action::Command { command, draft } => {
                                send_command(&client, &shared, &ctx, &mut uncertain, command, Pending::Command(draft)).await;
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
                            Action::History(thread, older) => {
                                let cursor = if older { shared.lock().unwrap().cache.older.get(thread.as_str()).cloned().flatten() } else { None };
                                let reply = client.query(Query::Conversation(ConversationQuery::ReadCommittedHistory(ReadCommittedHistory {
                                    thread: thread.clone(), run: None, direction: HistoryDirection::Backward,
                                    page: PageRequest { cursor, ..Default::default() },
                                }))).await;
                                match reply {
                                    Ok(QueryReply::Conversation(ConversationReply::CommittedHistory(page))) => update(&shared, &ctx, |s| s.cache.history_page(thread.as_str(), page, older)),
                                    other => update(&shared, &ctx, |s| s.status = format!("履歴取得: {other:?}")),
                                }
                                let mut cursor = None;
                                while let Ok(QueryReply::Conversation(ConversationReply::InputReceipts(page))) = client.query(Query::Conversation(ConversationQuery::GetInputReceipts(workbench_protocol::queries::ThreadQuery { thread: thread.clone(), page: PageRequest { cursor, ..Default::default() } }))).await {
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
                                        other => { update(&shared, &ctx, |s| s.status = format!("承認入力: {other:?}")); break; }
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
        // EOF alone is not a restart, but it is not an orderly stop either.
        let abnormal = !reconnect_requested
            && !planned
            && !orderly
            && matches!(
                client.connection_state(),
                workbench_client::ConnectionState::Closed(_)
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
            match auto.attempt(std::time::Instant::now()) {
                Some(attempt) => {
                    update(&shared, &ctx, |s| {
                        s.status = auto_reconnect::retrying(attempt)
                    });
                    continue;
                }
                None => update(&shared, &ctx, |s| s.status = auto_reconnect::gave_up()),
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
                    other => format!("再起動の完了を確認できませんでした: {other}"),
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
                            s.status = format!("OutcomeUnknown: {request}。再送していません。")
                        });
                        uncertain.push((request, pending));
                    }
                }
            }
            result => {
                update(shared, ctx, |s| {
                    s.status = format!("receipt 照合失敗（再送なし）: {result:?}")
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
                s.status = format!("OutcomeUnknown: {request_id}。再接続後 receipt を照合します。");
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
        let draft = match pending {
            Pending::Command(draft) => draft,
            Pending::Feature { feature, tag } => {
                s.features
                    .reply(feature, tag, FeatureReply::Command(Ok(reply.clone())));
                None
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
                None
            }
        };
        if let Some((thread, _)) = &draft {
            s.sending.remove(thread);
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
        s.status = reply_error.unwrap_or_else(|| format!("受付済み: {:?}", reply.disposition));
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
            s.status =
                "切断中のため未送信です。入力を保持しました。明示的に再接続してください。".into();
        }
    });
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
                    return Err("本文は 4 MiB を超えています（Unsupported）".into());
                }
                text.push_str(&chunk.text);
                match chunk.next_offset {
                    Some(next) if next > offset => offset = next,
                    None => return Ok(text),
                    _ => return Err("body offset did not advance".into()),
                }
            }
            other => return Err(format!("本文取得: {other:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
