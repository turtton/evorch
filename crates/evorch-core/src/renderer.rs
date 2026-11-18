//! The data connection of the local UDS: raw renderer frames and image bytes.
//!
//! `unix::connection` hands the first frame of every connection and the two
//! halves of its stream to [`route`] before it treats the frame as a
//! `ClientHello`. A data hello claims the connection; everything else keeps the
//! control-plane handling it had.
//!
//! A data connection has no provenance and is not a client. It opens a channel
//! in the data plane and waits: only a control command that passed its own
//! authorization (`BindRendererData`, `BindArtifactContent`) hands the channel
//! a lease. The connection then carries exactly that target, one frame or one
//! blob at a time, and ends when the lease is revoked, the peer stalls for
//! [`NO_PROGRESS_MS`], or the core shuts down.
use crate::backend::HostBackend;
use std::{io, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::unix::{OwnedReadHalf, OwnedWriteHalf},
    sync::watch,
    time::{sleep, timeout},
};
use workbench_host::areas::browser::renderer::data::{
    BlobLease, DataPlane, FrameLease, FrameOut, FrameStep, Lease, Revoked,
};
use workbench_protocol::namespaces::browser::data::{
    AckMeta, BIND_WINDOW_MS, CHUNK_BYTES, ChannelOffer, ChunkMeta, DataError, EndMeta, ErrorCode,
    ErrorMeta, Kind, NO_PROGRESS_MS, PREFIX_LEN, Prefix, SequenceGuard, is_hello,
};

/// The halves of a connection that stays a control connection.
pub(crate) type Halves = (OwnedReadHalf, OwnedWriteHalf);

/// How long one wait for a newer frame lasts before the lease is checked again.
const POLL: Duration = Duration::from_millis(250);
const NO_PROGRESS: Duration = Duration::from_millis(NO_PROGRESS_MS as u64);

/// Decide whether the first frame opens a data connection. `Some` gives the
/// halves back to the control-plane handling, `None` means the connection was
/// taken over and served to its end.
///
/// # Errors
/// Never: a data connection ending, however it ends, is not a core failure.
pub(crate) async fn route(
    first_frame: &[u8],
    read: OwnedReadHalf,
    write: OwnedWriteHalf,
    backend: &Arc<HostBackend>,
    shutdown: &mut watch::Receiver<bool>,
) -> io::Result<Option<Halves>> {
    if !is_hello(first_frame) {
        return Ok(Some((read, write)));
    }
    // The host lock is held for the clone only, never while serving.
    let plane = backend.host.lock().unwrap().renderer_data_plane();
    let _ = serve(plane, read, write, shutdown).await;
    Ok(None)
}

fn protocol(error: DataError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}
fn stalled() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "no progress")
}

async fn send(write: &mut OwnedWriteHalf, bytes: &[u8]) -> io::Result<()> {
    timeout(NO_PROGRESS, write.write_all(bytes))
        .await
        .map_err(|_| stalled())?
}

/// Says why the connection ends, as far as the peer still listens.
async fn finish(write: &mut OwnedWriteHalf, code: ErrorCode) -> io::Result<()> {
    let _ = send(write, &ErrorMeta { code }.encode()).await;
    let _ = write.shutdown().await;
    Ok(())
}

async fn serve(
    plane: Option<DataPlane>,
    mut read: OwnedReadHalf,
    mut write: OwnedWriteHalf,
    shutdown: &mut watch::Receiver<bool>,
) -> io::Result<()> {
    let Some(plane) = plane else {
        return finish(&mut write, ErrorCode::Denied).await;
    };
    let Ok(channel) = plane.open_channel() else {
        return finish(&mut write, ErrorCode::Busy).await;
    };
    let offer = ChannelOffer {
        channel: channel.id().clone(),
        bind_window_ms: BIND_WINDOW_MS,
    };
    send(&mut write, &offer.encode()).await?;
    let mut probe = [0u8; 1];
    // Nothing may come from the peer before the channel is bound.
    let bound = tokio::select! {
        lease = channel.bound() => lease,
        () = sleep(Duration::from_millis(u64::from(BIND_WINDOW_MS))) => {
            return finish(&mut write, ErrorCode::Expired).await;
        }
        _ = read.read(&mut probe) => return Ok(()),
        _ = shutdown.changed() => return Ok(()),
    };
    match bound {
        Some(Lease::Frames(lease)) => frames(lease, read, write, shutdown).await,
        Some(Lease::Blob(lease)) => blob(lease, read, write, shutdown).await,
        None => finish(&mut write, ErrorCode::Denied).await,
    }
}

/// Reads the peer's receipt of the transfer `id`.
async fn ack(read: &mut OwnedReadHalf, id: u64) -> io::Result<()> {
    let mut prefix = [0u8; PREFIX_LEN];
    timeout(NO_PROGRESS, read.read_exact(&mut prefix))
        .await
        .map_err(|_| stalled())??;
    let prefix = Prefix::parse(&prefix).map_err(protocol)?;
    if prefix.kind != Kind::Ack {
        return Err(protocol(DataError::OutOfOrder("expected an ack")));
    }
    let mut meta = vec![0u8; prefix.meta_len];
    timeout(NO_PROGRESS, read.read_exact(&mut meta))
        .await
        .map_err(|_| stalled())??;
    if AckMeta::decode(&meta).map_err(protocol)?.id != id {
        return Err(protocol(DataError::OutOfOrder("ack of another transfer")));
    }
    Ok(())
}

/// Sends `bytes` as the chunks and the end of the transfer `id`.
async fn body(write: &mut OwnedWriteHalf, id: u64, bytes: &[u8]) -> io::Result<()> {
    for (index, chunk) in bytes.chunks(CHUNK_BYTES).enumerate() {
        let meta = ChunkMeta {
            id,
            offset: (index * CHUNK_BYTES) as u64,
        };
        send(write, &meta.encode(chunk)).await?;
    }
    let end = EndMeta {
        id,
        total_bytes: bytes.len() as u64,
    };
    send(write, &end.encode()).await
}

async fn send_frame(
    out: &FrameOut,
    read: &mut OwnedReadHalf,
    write: &mut OwnedWriteHalf,
) -> io::Result<()> {
    let start = out.start();
    start.validate().map_err(protocol)?;
    send(write, &start.encode()).await?;
    body(write, start.sequence, out.bytes()).await?;
    ack(read, start.sequence).await
}

/// Why a send stopped without the peer's receipt.
enum Stop {
    Done,
    Revoked,
    Shutdown,
}

async fn frames(
    mut lease: FrameLease,
    mut read: OwnedReadHalf,
    mut write: OwnedWriteHalf,
    shutdown: &mut watch::Receiver<bool>,
) -> io::Result<()> {
    let mut revoked = lease.revoked();
    let mut order = SequenceGuard::default();
    let mut probe = [0u8; 1];
    loop {
        // One frame is in flight and the peer has acknowledged the last, so
        // anything it sends while the core waits for a newer frame is wrong.
        let step = tokio::select! {
            step = tokio::task::spawn_blocking(move || {
                let step = lease.next(POLL);
                (lease, step)
            }) => step.map_err(io::Error::other)?,
            () = revoked.wait() => return finish(&mut write, ErrorCode::Closed).await,
            _ = read.read(&mut probe) => return Ok(()),
            _ = shutdown.changed() => return Ok(()),
        };
        let (back, step) = step;
        lease = back;
        match step {
            FrameStep::Idle => {}
            FrameStep::Ended => return finish(&mut write, ErrorCode::Closed).await,
            FrameStep::Frame(out) => {
                order.accept(out.start().sequence).map_err(protocol)?;
                match transfer(
                    send_frame(&out, &mut read, &mut write),
                    &mut revoked,
                    shutdown,
                )
                .await?
                {
                    Stop::Done => {}
                    Stop::Revoked => return finish(&mut write, ErrorCode::Closed).await,
                    Stop::Shutdown => return Ok(()),
                }
            }
        }
    }
}

async fn transfer(
    work: impl std::future::Future<Output = io::Result<()>>,
    revoked: &mut Revoked,
    shutdown: &mut watch::Receiver<bool>,
) -> io::Result<Stop> {
    tokio::select! {
        done = work => done.map(|()| Stop::Done),
        () = revoked.wait() => Ok(Stop::Revoked),
        _ = shutdown.changed() => Ok(Stop::Shutdown),
    }
}

async fn send_blob(
    lease: &mut Option<BlobLease>,
    read: &mut OwnedReadHalf,
    write: &mut OwnedWriteHalf,
) -> io::Result<()> {
    let start = lease.as_ref().expect("lease present").start().clone();
    start.validate().map_err(protocol)?;
    send(write, &start.encode()).await?;
    let mut sent = 0u64;
    let mut buf = vec![0u8; CHUNK_BYTES];
    while sent < start.total_bytes {
        let mut owned = lease.take().expect("lease present");
        let (back, buf_back, filled) = tokio::task::spawn_blocking(move || {
            let mut filled = 0;
            let result = loop {
                if filled == buf.len() {
                    break Ok(filled);
                }
                match owned.read(&mut buf[filled..]) {
                    Ok(0) => break Ok(filled),
                    Ok(n) => filled += n,
                    Err(e) => break Err(e),
                }
            };
            (owned, buf, result)
        })
        .await
        .map_err(io::Error::other)?;
        *lease = Some(back);
        buf = buf_back;
        let filled = filled? as u64;
        let filled = filled.min(start.total_bytes - sent);
        if filled == 0 {
            // The stored file is shorter than its record said.
            return Err(protocol(DataError::OutOfOrder(
                "blob shorter than announced",
            )));
        }
        let meta = ChunkMeta {
            id: start.transfer_id,
            offset: sent,
        };
        send(write, &meta.encode(&buf[..filled as usize])).await?;
        sent += filled;
    }
    let end = EndMeta {
        id: start.transfer_id,
        total_bytes: sent,
    };
    send(write, &end.encode()).await?;
    ack(read, start.transfer_id).await
}

async fn blob(
    lease: BlobLease,
    mut read: OwnedReadHalf,
    mut write: OwnedWriteHalf,
    shutdown: &mut watch::Receiver<bool>,
) -> io::Result<()> {
    let mut revoked = lease.revoked();
    let mut lease = Some(lease);
    match transfer(
        send_blob(&mut lease, &mut read, &mut write),
        &mut revoked,
        shutdown,
    )
    .await
    {
        Ok(Stop::Done) => {
            let _ = write.shutdown().await;
            Ok(())
        }
        Ok(Stop::Revoked) => finish(&mut write, ErrorCode::Closed).await,
        Ok(Stop::Shutdown) => Ok(()),
        Err(error) => {
            let code = if error.kind() == io::ErrorKind::TimedOut {
                ErrorCode::TimedOut
            } else {
                ErrorCode::Invalid
            };
            finish(&mut write, code).await?;
            Err(error)
        }
    }
}
