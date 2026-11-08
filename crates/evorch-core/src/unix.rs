//! 4-byte BE length + JSON. A dedicated writer multiplexes replies and events.
use crate::backend::{HostBackend, error};
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
fn invalid(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

pub async fn connection(
    stream: UnixStream,
    backend: Arc<HostBackend>,
    mut shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    if stream.peer_cred()?.uid() != rustix::process::geteuid().as_raw() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "wrong socket peer owner",
        ));
    }
    let _connected = backend.connected();
    let (mut read, mut write) = stream.into_split();
    let mut hello = backend.hello();
    let first = tokio::select! { r = payload(&mut read, hello.limits.max_frame_bytes) => r?, _ = shutdown.changed() => return Ok(()) };
    let client: ClientHello = serde_json::from_slice(&first).map_err(invalid)?;
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
    let (output, mut frames) = mpsc::channel::<ServerFrame>(64);
    let max = hello.limits.max_frame_bytes;
    let writer_backend = backend.clone();
    tasks.spawn(async move {
        while let Some(frame) = frames.recv().await {
            // Includes early Subscribe replies and connection-generated errors.
            let frame =
                writer_backend.protect_output(serde_json::to_value(frame).map_err(invalid)?);
            let bytes = encode_frame(&frame, max).map_err(invalid)?;
            write.write_all(&bytes).await?;
        }
        Ok::<_, io::Error>(true)
    });
    let (incoming, mut requests) = mpsc::channel(64);
    tasks.spawn(async move {
        loop {
            let frame = payload(&mut read, max).await?;
            if incoming.send(frame).await.is_err() {
                return Ok(true);
            }
        }
    });
    let provenance = Provenance::human(Surface::Desktop, client.client_instance_id);
    let mut session = None;
    let mut subscriptions = BTreeMap::new();
    loop {
        let bytes = tokio::select! {
            _ = shutdown.changed() => break,
            ended = tasks.join_next() => match ended { Some(Ok(Ok(false))) => continue, Some(Err(e)) if e.is_cancelled() => continue, _ => break },
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
                break;
            }
        };
        subscriptions.retain(|_, task: &mut tokio::task::AbortHandle| !task.is_finished());
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
                        match backend.subscription(&s) {
                            Err(e) => ReplyResult::Error(e),
                            Ok(mut sub) => {
                                let sub_id = sub.started.subscription_id.to_string();
                                let started = backend.mask(sub.started.clone()).map_err(invalid)?;
                                // Reply is enqueued before any event from this subscription.
                                tokio::select! {
                                    _ = shutdown.changed() => break,
                                    r = output.send(ServerFrame::Reply(ReplyEnvelope { request_id, result: ReplyResult::Success(ReplyPayload::SubscriptionStarted(started)) })) => { r.map_err(invalid)?; }
                                }
                                let tx = output.clone();
                                let backend = backend.clone();
                                let task = tasks.spawn(async move {
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
                                subscriptions.insert(sub_id, task);
                                continue;
                            }
                        }
                    }
                }
                RequestBody::Control(Control::Unsubscribe { subscription_id }) => {
                    backend.projection.unsubscribe(&subscription_id);
                    if let Some(task) = subscriptions.remove(subscription_id.as_str()) {
                        task.abort();
                    }
                    ReplyResult::Success(ReplyPayload::Unsubscribed { subscription_id })
                }
                RequestBody::Control(Control::Pong { .. }) => {
                    ReplyResult::Success(ReplyPayload::Ack)
                }
                _ => backend.request(request, provenance.clone()).result,
            }
        };
        let reply = backend
            .mask(ReplyEnvelope { request_id, result })
            .map_err(invalid)?;
        tokio::select! { _ = shutdown.changed() => break, r = output.send(ServerFrame::Reply(reply)) => { r.map_err(invalid)?; } }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    Ok(())
}
