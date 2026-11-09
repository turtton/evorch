//! The single desktop actor. Closing the UI drops the connection; it never
//! schedules a stop. Only explicit human actions can issue state-changing wire
//! commands, and a command with a lost reply is reconciled, never replayed.
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;
use workbench_client::{ClientError, Subscription, TransportError, WorkbenchClient};
use workbench_protocol::{
    commands::{Command, CommandReply},
    conversation::{ConversationCommand, ConversationResult},
    events::{EventEnvelope, Subscribe, SubscriptionScope},
    ids::{BodyId, ProjectId, RequestId, ThreadId},
    queries::{
        CommandReceiptState, HistoryDirection, PageRequest, Query, QueryReply, ReadBody,
        ReadCommittedHistory, ReadSnapshot,
    },
    wire::ByteOffset,
};

use super::{Shared, cache::Cache};

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
        mime: String,
        data: String,
    },
    Reconnect,
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

const SNAPSHOT_BUFFER_EVENTS: usize = 256;
const SNAPSHOT_BUFFER_BYTES: usize = 2 * 1024 * 1024;

pub async fn snapshot(
    client: &WorkbenchClient,
    subscription: &mut Subscription,
) -> Result<Cache, String> {
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
        let next = client.query(Query::ReadSnapshot(ReadSnapshot {
            snapshot_id: id.clone(),
            page: PageRequest {
                cursor: Some(cursor),
                ..Default::default()
            },
        }));
        tokio::pin!(next);
        let reply = loop {
            tokio::select! {
                reply = &mut next => break reply.map_err(|e| e.to_string())?,
                event = subscription.next() => {
                    let event = event.ok_or("subscription ended while paging snapshot")?;
                    if event.event.ends_subscription() { return Err("snapshot stream requires resync".into()); }
                    bytes += serde_json::to_vec(&event).map_err(|e| e.to_string())?.len();
                    if buffered.len() >= SNAPSHOT_BUFFER_EVENTS || bytes > SNAPSHOT_BUFFER_BYTES {
                        return Err("snapshot event buffer overflow; resync required".into());
                    }
                    buffered.push(event);
                }
            }
        };
        match reply {
            QueryReply::Snapshot(snapshot)
                if snapshot.snapshot_id == id && snapshot.high_water == started.high_water =>
            {
                page = snapshot.page
            }
            _ => return Err("snapshot cut changed while paging".into()),
        }
    }
    cache.at(started.high_water);
    for event in buffered {
        if !cache.event(event) {
            return Err("snapshot epoch changed; resync required".into());
        }
    }
    Ok(cache)
}

pub async fn serve(
    launcher: core_launcher::Launcher,
    mut actions: mpsc::Receiver<Action>,
    shared: Arc<Mutex<Shared>>,
    ctx: egui::Context,
) {
    let mut uncertain = Vec::<(RequestId, Option<(String, u64)>)>::new();
    loop {
        update(&shared, &ctx, |s| {
            s.connected = false;
            s.status = "コアへ接続中…".into();
        });
        let connected = launcher.connect_or_start().await;
        let client = match connected {
            Ok(client) => client,
            Err(e) => {
                update(&shared, &ctx, |s| {
                    s.status = format!("接続失敗（fallback しません）: {e}")
                });
                if !wait_reconnect(&mut actions, &shared, &ctx).await {
                    return;
                }
                continue;
            }
        };
        for (request, draft) in std::mem::take(&mut uncertain) {
            match client
                .query(Query::GetCommandReceipt {
                    request_id: request.clone(),
                })
                .await
            {
                Ok(QueryReply::CommandReceipt(receipt)) => match receipt.state {
                    CommandReceiptState::Applied { reply } => {
                        update(&shared, &ctx, |s| {
                            s.outcome_unknown.remove(request.as_str());
                        });
                        accepted(&client, &shared, &ctx, reply, draft).await;
                    }
                    CommandReceiptState::Rejected { error } => update(&shared, &ctx, |s| {
                        s.outcome_unknown.remove(request.as_str());
                        if let Some((thread, _)) = draft {
                            s.sending.remove(&thread);
                        }
                        s.status = error.to_string();
                    }),
                    CommandReceiptState::Unknown => {
                        update(&shared, &ctx, |s| {
                            s.status = format!("OutcomeUnknown: {request}。再送していません。")
                        });
                        uncertain.push((request, draft));
                    }
                },
                result => {
                    update(&shared, &ctx, |s| {
                        s.status = format!("receipt 照合失敗（再送なし）: {result:?}")
                    });
                    uncertain.push((request, draft));
                }
            }
        }
        let mut resync = true;
        while resync {
            resync = false;
            let mut stream = match client
                .subscribe(Subscribe {
                    scope: SubscriptionScope::All,
                    after: None,
                })
                .await
            {
                Ok(stream) => stream,
                Err(e) => {
                    update(&shared, &ctx, |s| s.status = e.to_string());
                    break;
                }
            };
            match snapshot(&client, &mut stream).await {
                Ok(cache) => update(&shared, &ctx, |s| {
                    s.replace_projection(cache);
                    s.connected = true;
                    s.status = "core-client: 接続済み".into();
                }),
                Err(error) => {
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
            }
            loop {
                tokio::select! {
                    event = stream.next() => {
                        let Some(event) = event else { break };
                        let mut valid = false;
                        update(&shared, &ctx, |s| valid = s.cache.event(event));
                        if !valid { resync = true; break; }
                    }
                    action = actions.recv() => {
                        let Some(action) = action else { return };
                        if matches!(action, Action::Reconnect) { resync = true; break; }
                        match action {
                            Action::Command { command, draft } => {
                                match client.command(command).await {
                                    Ok(reply) => accepted(&client, &shared, &ctx, reply, draft).await,
                                    Err(ClientError::Transport(TransportError::OutcomeUnknown { request_id })) => {
                                        uncertain.push((request_id.clone(), draft));
                                        update(&shared, &ctx, |s| { s.outcome_unknown.insert(request_id.to_string()); });
                                        update(&shared, &ctx, |s| s.status = format!("OutcomeUnknown: {request_id}。再接続後 receipt を照合します。"));
                                    }
                                    Err(e) => update(&shared, &ctx, |s| { if let Some((thread, _)) = draft { s.sending.remove(&thread); } s.status = e.to_string(); }),
                                }
                            }
                            Action::History(thread, older) => {
                                let cursor = if older { shared.lock().unwrap().cache.older.get(thread.as_str()).cloned().flatten() } else { None };
                                let reply = client.query(Query::ReadCommittedHistory(ReadCommittedHistory {
                                    thread: thread.clone(), run: None, direction: HistoryDirection::Backward,
                                    page: PageRequest { cursor, ..Default::default() },
                                })).await;
                                match reply {
                                    Ok(QueryReply::CommittedHistory(page)) => update(&shared, &ctx, |s| s.cache.history_page(thread.as_str(), page, older)),
                                    other => update(&shared, &ctx, |s| s.status = format!("履歴取得: {other:?}")),
                                }
                                let mut cursor = None;
                                while let Ok(QueryReply::InputReceipts(page)) = client.query(Query::GetInputReceipts(workbench_protocol::queries::ThreadQuery { thread: thread.clone(), page: PageRequest { cursor, ..Default::default() } })).await {
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
                                    match client.query(Query::GetApprovalInput(query)).await {
                                        Ok(QueryReply::ApprovalInput(chunk)) if chunk.offset == offset && chunk.approval_id == approval.id && chunk.core_epoch == approval.core_epoch && chunk.approval_revision == approval.revision => {
                                            text.push_str(&chunk.text);
                                            if text.len() > 1024 * 1024 { break; }
                                            match chunk.next_offset { Some(next) if next > offset => offset = next, None => { update(&shared, &ctx, |s| { s.cache.approval_inputs.insert(approval.id.to_string(), text); }); break; }, _ => break }
                                        }
                                        other => { update(&shared, &ctx, |s| s.status = format!("承認入力: {other:?}")); break; }
                                    }
                                }
                            }
                            Action::Upload { project, thread, mime, data } => {
                                let mut position = 0;
                                let mut uploaded = false;
                                update(&shared, &ctx, |s| {
                                    let images = s.image_drafts.entry(thread.to_string()).or_default();
                                    position = if let Some(index) = images.iter().position(|i| i.mime == mime && i.data == data) { index } else {
                                        images.push(super::ImageDraft { mime: mime.clone(), data: data.clone(), reference: None }); images.len() - 1
                                    };
                                    uploaded = images[position].reference.is_some();
                                });
                                if uploaded { continue; }
                                match client.command(Command::Conversation(ConversationCommand::UploadImage {
                                    project, thread: thread.clone(), mime_type: mime, data,
                                })).await {
                                    Ok(reply) => if let Some(result) = reply.conversation
                                        && let ConversationResult::ImageUploaded(image) = *result {
                                            update(&shared, &ctx, |s| { s.image_drafts.get_mut(thread.as_str()).unwrap()[position].reference = Some(image); });
                                        },
                                    Err(e) => update(&shared, &ctx, |s| s.status = e.to_string()),
                                }
                            }
                            Action::Reconnect => unreachable!(),
                        }
                    }
                }
            }
        }
        update(&shared, &ctx, |s| s.connected = false);
        if !wait_reconnect(&mut actions, &shared, &ctx).await {
            return;
        }
    }
}

async fn accepted(
    client: &WorkbenchClient,
    shared: &Arc<Mutex<Shared>>,
    ctx: &egui::Context,
    reply: CommandReply,
    draft: Option<(String, u64)>,
) {
    let mut discarded = Vec::new();
    update(shared, ctx, |s| {
        if let Some((thread, _)) = &draft {
            s.sending.remove(thread);
            discarded.extend(
                s.image_drafts
                    .remove(thread)
                    .into_iter()
                    .flatten()
                    .filter_map(|image| image.reference),
            );
        }
        if let Some((thread, revision)) = draft
            && let Some(draft) = s.drafts.get_mut(&thread)
            && draft.revision == revision
        {
            draft.text.clear();
            draft.revision += 1;
        }
        s.status = format!("受付済み: {:?}", reply.disposition);
    });
    for image in discarded {
        // Resource cleanup only: a lost discard reply never replays human input.
        let _ = client
            .command(Command::Conversation(
                ConversationCommand::DiscardAttachment {
                    attachment_id: image.attachment_id,
                },
            ))
            .await;
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
        update(shared, ctx, |s| {
            match action {
                Action::Command {
                    draft: Some((thread, _)),
                    ..
                } => {
                    s.sending.remove(&thread);
                }
                Action::Upload {
                    thread, mime, data, ..
                } => {
                    let images = s.image_drafts.entry(thread.to_string()).or_default();
                    if !images
                        .iter()
                        .any(|image| image.mime == mime && image.data == data)
                    {
                        images.push(super::ImageDraft {
                            mime,
                            data,
                            reference: None,
                        });
                    }
                }
                _ => (),
            }
            s.status =
                "切断中のため未送信です。入力を保持しました。明示的に再接続してください。".into();
        });
    }
    false
}

async fn body(client: &WorkbenchClient, id: BodyId) -> Result<String, String> {
    let mut offset = ByteOffset(0);
    let mut text = String::new();
    loop {
        match client
            .query(Query::ReadBody(ReadBody {
                body_id: id.clone(),
                offset,
                max_bytes: 65536.min(client.server_hello().limits.max_body_chunk_bytes),
            }))
            .await
        {
            Ok(QueryReply::Body(chunk)) if chunk.body_id == id && chunk.offset == offset => {
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
            command: Command::StopChat(workbench_protocol::commands::ThreadTarget {
                thread: ThreadId::new("t").unwrap(),
            }),
            draft: Some(("t".into(), 3)),
        })
        .await
        .unwrap();
        tx.send(Action::Upload {
            project: ProjectId::new("p").unwrap(),
            thread: ThreadId::new("t").unwrap(),
            mime: "image/png".into(),
            data: "retained local bytes".into(),
        })
        .await
        .unwrap();
        tx.send(Action::Reconnect).await.unwrap();
        assert!(wait_reconnect(&mut rx, &shared, &egui::Context::default()).await);
        let state = shared.lock().unwrap();
        assert_eq!(state.drafts["t"].text, "not written");
        assert!(!state.sending.contains("t"));
        assert_eq!(state.image_drafts["t"][0].data, "retained local bytes");
        assert!(state.image_drafts["t"][0].reference.is_none());
    }
}
