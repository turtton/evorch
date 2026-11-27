//! 4-byte BE length + JSON. A dedicated writer multiplexes replies and events.
use crate::backend::{HostBackend, error};
use crate::blocking::Job;
use crate::connection_log::ConnectionLog;
use std::{collections::BTreeMap, io, sync::Arc};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    sync::{mpsc, watch},
    task::JoinSet,
};
use workbench_protocol::{envelope::*, errors::ErrorCode, framing::encode_frame, handshake::*};
use workbench_web::WorkbenchBackend;

async fn payload(read: &mut (impl AsyncRead + Unpin), max: u32) -> io::Result<Vec<u8>> {
    let len = read.read_u32().await?;
    if len == 0 || len > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid frame length",
        ));
    }
    let mut bytes = vec![0; len as usize];
    read.read_exact(&mut bytes).await?;
    Ok(bytes)
}
/// The text of serde's error quotes the input (an unknown field name, a wrong
/// value), and this error is logged: keep only what kind of error and where.
fn malformed_hello(e: serde_json::Error) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "malformed client hello ({:?} error at line {} column {})",
            e.classify(),
            e.line(),
            e.column()
        ),
    )
}
fn invalid(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

/// The core is stopping on purpose (signal, idle exit, management stop): tell
/// every subscription so before the connection closes, so a client can tell this
/// from a crash. Bounded: a client that does not read cannot hold the shutdown.
const GOODBYE_BOUND: std::time::Duration = std::time::Duration::from_secs(2);
struct WriterShutdown {
    finish: tokio::sync::oneshot::Sender<()>,
    flushed: tokio::sync::oneshot::Receiver<()>,
    task: tokio::task::AbortHandle,
}
async fn say_goodbye(
    backend: &Arc<HostBackend>,
    output: &mpsc::Sender<ServerFrame>,
    writer: WriterShutdown,
    ids: Vec<workbench_protocol::ids::SubscriptionId>,
    pumps: &mut JoinSet<io::Result<bool>>,
    log: &ConnectionLog,
) {
    let WriterShutdown {
        finish,
        mut flushed,
        task,
    } = writer;
    let deadline = tokio::time::Instant::now() + GOODBYE_BOUND;
    // No more events of these subscriptions may follow the goodbye.
    pumps.abort_all();
    while pumps.join_next().await.is_some() {}
    // Keep the handle outside the delivery deadline: dropping an await cannot
    // cancel a running synchronous operation.
    let mut notice = backend.blocking(Job::Goodbye, move |backend| {
        ids.iter()
            .filter_map(|id| backend.mask(backend.projection.shutdown_notice(id)).ok())
            .collect::<Vec<_>>()
    });
    let mut joined = false;
    let mut writer_flushed = false;
    let _ = tokio::time::timeout_at(deadline, async {
        let notices = log.blocking_result(Job::Goodbye, (&mut notice).await);
        joined = true;
        for notice in notices {
            if tokio::time::Instant::now() >= deadline {
                return;
            }
            if output
                .send(ServerFrame::Event(Box::new(notice)))
                .await
                .is_err()
            {
                return;
            }
        }
        let _ = finish.send(());
        let _ = (&mut flushed).await;
        writer_flushed = true;
    })
    .await;
    // A timed-out delivery must stop the writer now, not after a slow notice
    // job finishes. The flush sender is owned by the writer, so its closure
    // also confirms cancellation of any outstanding write.
    task.abort();
    if !writer_flushed {
        let _ = flushed.await;
    }
    if !joined {
        // The two seconds bound wire delivery, not synchronous lock ownership.
        log.blocking_result(Job::Goodbye, notice.await);
    }
}

pub async fn connection(
    stream: UnixStream,
    backend: Arc<HostBackend>,
    shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    let log = ConnectionLog::new(&shutdown);
    let result = serve_connection(stream, backend, shutdown, &log).await;
    log.finished(&result);
    result
}

async fn serve_connection(
    stream: UnixStream,
    backend: Arc<HostBackend>,
    mut shutdown: watch::Receiver<bool>,
    log: &ConnectionLog,
) -> io::Result<()> {
    let verified_peer = workbench_authority::verify_unix_peer(&stream)?;
    let (mut read, write) = stream.into_split();
    let max_frame_bytes = backend.wire_limits().max_frame_bytes;
    let first = tokio::select! { r = payload(&mut read, max_frame_bytes) => r?, _ = shutdown.changed() => return Ok(()) };
    let Some((mut read, mut write)) =
        crate::renderer::route(&first, read, write, &backend, &mut shutdown).await?
    else {
        return Ok(());
    };
    let mut hello = backend.hello();
    let _connected = backend.connected();
    let client: ClientHello = serde_json::from_slice(&first).map_err(malformed_hello)?;
    let negotiated = negotiate(
        &client,
        &ServerOffer {
            core: &hello.core,
            versions: &[ProtocolVersion::CURRENT],
            capabilities: &hello.capabilities,
        },
    );
    let reply = match negotiated {
        Ok(n) => {
            hello.protocol_version = n.version;
            hello.capabilities = n.capabilities;
            HandshakeReply::Accepted(hello.clone())
        }
        Err(e) => HandshakeReply::Rejected(e),
    };
    let wire = backend.protect_output(serde_json::to_value(&reply).map_err(invalid)?);
    let frame = encode_frame(&wire, hello.limits.max_frame_bytes).map_err(invalid)?;
    tokio::select! { r = write.write_all(&frame) => r?, _ = shutdown.changed() => return Ok(()) }
    if matches!(reply, HandshakeReply::Rejected(_)) {
        return Ok(());
    }
    let mut tasks = JoinSet::new();
    let mut pumps = JoinSet::new();
    let (output, mut frames) = mpsc::channel::<ServerFrame>(64);
    let max = hello.limits.max_frame_bytes;
    let writer_backend = backend.clone();
    // `finish` asks the writer to write what is queued and stop; `flushed`
    // says it did. An orderly shutdown sends its goodbye through the same queue
    // and closes only after that.
    let (finish, mut finish_rx) = tokio::sync::oneshot::channel::<()>();
    let (flushed_tx, flushed) = tokio::sync::oneshot::channel::<()>();
    let writer_task = tasks.spawn(log.task("writer", async move {
        let mut finishing = false;
        loop {
            let frame = tokio::select! {
                biased;
                frame = frames.recv() => frame,
                _ = &mut finish_rx, if !finishing => {
                    finishing = true;
                    frames.close();
                    continue;
                }
            };
            let Some(frame) = frame else { break };
            // Includes early Subscribe replies and connection-generated errors.
            let frame =
                writer_backend.protect_output(serde_json::to_value(frame).map_err(invalid)?);
            let bytes = encode_frame(&frame, max).map_err(invalid)?;
            write.write_all(&bytes).await?;
        }
        if finishing {
            write.flush().await?;
            let _ = flushed_tx.send(());
        }
        Ok::<_, io::Error>(true)
    }));
    let writer = WriterShutdown {
        finish,
        flushed,
        task: writer_task,
    };
    let (incoming, mut requests) = mpsc::channel(64);
    tasks.spawn(log.task("reader", async move {
        loop {
            let frame = payload(&mut read, max).await?;
            if incoming.send(frame).await.is_err() {
                return Ok(true);
            }
        }
    }));
    log.established(client.client_instance_id.as_str());
    let _client_connection = backend.client_connection(client.client_instance_id.clone());
    let surface = match client.local_surface {
        Some(LocalSurface::Cli) => Surface::Cli,
        _ => Surface::Desktop,
    };
    let context = workbench_authority::IngressContext::new(
        Provenance::human(surface, client.client_instance_id),
        verified_peer.authority(),
    );
    let admin_slots = Arc::new(tokio::sync::Semaphore::new(32));
    let mut session = None;
    let mut subscriptions = BTreeMap::new();
    let mut subscribed = BTreeMap::<String, workbench_protocol::ids::SubscriptionId>::new();
    loop {
        let bytes = tokio::select! {
            _ = shutdown.changed() => break,
            ended = tasks.join_next() => match ended { Some(Ok(Ok(false))) => continue, Some(Err(e)) if e.is_cancelled() => continue, other => { log.task_ended(&other); break } },
            ended = pumps.join_next(), if !pumps.is_empty() => match ended { Some(Ok(Ok(false))) => continue, Some(Err(e)) if e.is_cancelled() => continue, other => { log.task_ended(&other); break } },
            r = requests.recv() => match r { Some(r) => r, None => break },
        };
        let request = match decode_request(&bytes, &hello.limits) {
            Ok(r) => r,
            Err(rejected) => {
                if let Some(id) = rejected.request_id {
                    let reply = backend
                        .mask(ReplyEnvelope {
                            request_id: id,
                            result: ReplyResult::Error(rejected.error),
                        })
                        .map_err(invalid)?;
                    tokio::select! { _ = shutdown.changed() => break, r = output.send(ServerFrame::Reply(reply)) => { r.map_err(invalid)?; } }
                    continue;
                }
                log.closing("a request without an id could not be decoded");
                break;
            }
        };
        subscriptions.retain(|_, task: &mut tokio::task::AbortHandle| !task.is_finished());
        subscribed.retain(|key, _| subscriptions.contains_key(key));
        let request_id = request.request_id.clone();
        let wrong_session = session.as_ref().is_some_and(|s| s != &request.session_id);
        session.get_or_insert(request.session_id.clone());
        let result = if wrong_session {
            ReplyResult::Error(error(
                ErrorCode::PermissionDenied,
                "session changed on a live connection",
            ))
        } else if request
            .body
            .required_capability()
            .is_some_and(|c| !hello.has(c))
        {
            ReplyResult::Error(error(
                ErrorCode::UnsupportedCapability,
                "capability was not negotiated",
            ))
        } else {
            match request.body.clone() {
                RequestBody::Control(Control::Subscribe(s)) => {
                    if subscriptions.len() >= 16 {
                        ReplyResult::Error(error(
                            ErrorCode::LimitExceeded,
                            "too many subscriptions",
                        ))
                    } else {
                        let context = context.clone();
                        let result = backend
                            .blocking(Job::Subscribe, move |backend| {
                                backend.subscription(&s, &context)
                            })
                            .await;
                        match log.blocking_result(Job::Subscribe, result) {
                            Err(e) => ReplyResult::Error(e),
                            Ok(mut sub) => {
                                let sub_id = sub.started.subscription_id.to_string();
                                let sub_started_id = sub.started.subscription_id.clone();
                                let started = backend.mask(sub.started.clone()).map_err(invalid)?;
                                // Reply is enqueued before any event from this subscription.
                                tokio::select! {
                                    _ = shutdown.changed() => break,
                                    r = output.send(ServerFrame::Reply(ReplyEnvelope { request_id, result: ReplyResult::Success(ReplyPayload::SubscriptionStarted(started)) })) => { r.map_err(invalid)?; }
                                }
                                let tx = output.clone();
                                let backend = backend.clone();
                                let task = pumps.spawn(async move {
                                    loop {
                                        let Ok(permit) = tx.reserve().await else {
                                            break;
                                        };
                                        let Some(event) = sub.recv().await else {
                                            break;
                                        };
                                        let terminal = event.event.ends_subscription();
                                        permit.send(ServerFrame::Event(Box::new(
                                            backend.mask(event).map_err(invalid)?,
                                        )));
                                        if terminal {
                                            break;
                                        }
                                    }
                                    // A subscription ending must not close other subscriptions.
                                    Ok(false)
                                });
                                subscribed.insert(sub_id.clone(), sub_started_id);
                                subscriptions.insert(sub_id, task);
                                continue;
                            }
                        }
                    }
                }
                RequestBody::Control(Control::Unsubscribe { subscription_id }) => {
                    let id = subscription_id.clone();
                    let result = backend
                        .blocking(Job::Unsubscribe, move |backend| {
                            backend.projection.unsubscribe(&id)
                        })
                        .await;
                    log.blocking_result(Job::Unsubscribe, result);
                    subscribed.remove(subscription_id.as_str());
                    if let Some(task) = subscriptions.remove(subscription_id.as_str()) {
                        task.abort();
                    }
                    ReplyResult::Success(ReplyPayload::Unsubscribed { subscription_id })
                }
                RequestBody::Control(Control::Pong { .. }) => {
                    ReplyResult::Success(ReplyPayload::Ack)
                }
                _ if request.body.admin_action().is_some() => {
                    if request.body.is_admin_fast_lane() {
                        let session = workbench_web::WebSession {
                            session_id: request.session_id.clone(),
                            client_id: context.provenance().client_id.clone().unwrap(),
                        };
                        // Both are synchronous memory fast lanes in the common service.
                        let reply = backend
                            .handle_request(&session, request, context.clone())
                            .await;
                        output
                            .send(ServerFrame::Reply(backend.mask(reply).map_err(invalid)?))
                            .await
                            .map_err(invalid)?;
                        continue;
                    }
                    let Ok(slot) = admin_slots.clone().try_acquire_owned() else {
                        output
                            .send(ServerFrame::Reply(ReplyEnvelope {
                                request_id,
                                result: ReplyResult::Error(error(
                                    ErrorCode::Busy,
                                    "too many management requests",
                                )),
                            }))
                            .await
                            .map_err(invalid)?;
                        continue;
                    };
                    // Pending audit ACKs must not serialize a later cancel on this socket.
                    let backend = backend.clone();
                    let context = context.clone();
                    let tx = output.clone();
                    tasks.spawn(async move {
                        let _slot = slot;
                        let session = workbench_web::WebSession {
                            session_id: request.session_id.clone(),
                            client_id: context.provenance().client_id.clone().unwrap(),
                        };
                        let reply = backend.handle_request(&session, request, context).await;
                        tx.send(ServerFrame::Reply(backend.mask(reply).map_err(invalid)?))
                            .await
                            .map_err(invalid)?;
                        Ok(false)
                    });
                    continue;
                }
                _ => {
                    log.blocking_result(
                        Job::Request,
                        backend.request_unix(request, context.clone()).await,
                    )
                    .result
                }
            }
        };
        let reply = backend
            .mask(ReplyEnvelope { request_id, result })
            .map_err(invalid)?;
        tokio::select! { _ = shutdown.changed() => break, r = output.send(ServerFrame::Reply(reply)) => { r.map_err(invalid)?; } }
    }
    if *shutdown.borrow() {
        let ids = subscribed
            .iter()
            .filter(|(key, _)| subscriptions.contains_key(*key))
            .map(|(_, id)| id.clone())
            .collect();
        say_goodbye(&backend, &output, writer, ids, &mut pumps, log).await;
    }
    tasks.abort_all();
    pumps.abort_all();
    while tasks.join_next().await.is_some() {}
    while pumps.join_next().await.is_some() {}
    Ok(())
}

#[cfg(test)]
mod tests;
