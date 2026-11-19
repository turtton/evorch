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
use std::{
    future::Future,
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::unix::{OwnedReadHalf, OwnedWriteHalf},
    sync::watch,
    time::{sleep, timeout},
};
use workbench_host::areas::browser::renderer::data::{
    BlobLease, DataPlane, FrameLease, FrameOut, FrameStep, Lease,
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

/// Where the writes of one data connection stand. An `Error` record is the
/// last record and must start on a record boundary, so the connection knows
/// whether a record is half written (`mid`), and whether the lease was
/// revoked (`stop`): a revoked transfer finishes the record in flight, starts
/// no other one and then says why it ended.
#[derive(Default)]
struct Gate {
    mid: AtomicBool,
    stop: AtomicBool,
}
impl Gate {
    fn mid(&self) -> bool {
        self.mid.load(Ordering::SeqCst)
    }
    fn stopping(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
    /// A new record may start unless the transfer is stopping.
    fn begin(&self) -> io::Result<()> {
        if self.stopping() {
            return Err(io::ErrorKind::Interrupted.into());
        }
        Ok(())
    }
}

/// The write half of a data connection with its [`Gate`].
struct Out {
    half: OwnedWriteHalf,
    gate: Arc<Gate>,
}
impl Out {
    fn new(half: OwnedWriteHalf) -> Self {
        Self {
            half,
            gate: Arc::default(),
        }
    }
    /// Writes one record, whole within [`NO_PROGRESS`]. A failure leaves it
    /// marked half written.
    async fn put(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.gate.mid.store(true, Ordering::SeqCst);
        timeout(NO_PROGRESS, self.half.write_all(bytes))
            .await
            .map_err(|_| stalled())??;
        self.gate.mid.store(false, Ordering::SeqCst);
        Ok(())
    }
}

/// Sends one record unless the transfer is stopping.
async fn send(out: &mut Out, bytes: &[u8]) -> io::Result<()> {
    out.gate.begin()?;
    out.put(bytes).await
}

/// Writes `head` and `body` with one vectored write per attempt, so a chunk
/// is sent from the shared frame without being copied behind its header. Every
/// attempt must make progress within [`NO_PROGRESS`].
async fn send_chunk(out: &mut Out, head: &[u8], body: &[u8]) -> io::Result<()> {
    out.gate.begin()?;
    out.gate.mid.store(true, Ordering::SeqCst);
    let mut parts = [io::IoSlice::new(head), io::IoSlice::new(body)];
    let mut parts: &mut [io::IoSlice<'_>] = &mut parts;
    io::IoSlice::advance_slices(&mut parts, 0);
    while !parts.is_empty() {
        let written = timeout(NO_PROGRESS, out.half.write_vectored(parts))
            .await
            .map_err(|_| stalled())??;
        if written == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        io::IoSlice::advance_slices(&mut parts, written);
    }
    out.gate.mid.store(false, Ordering::SeqCst);
    Ok(())
}

/// Ends the connection after `error`. A peer that made no progress for
/// [`NO_PROGRESS`] is not told why: its socket is full, so the record would
/// wait another full window. The connection is closed at once, which is what
/// returns its socket, lease, buffers and transfer slot.
async fn end_after(out: &mut Out, error: &io::Error) {
    if error.kind() == io::ErrorKind::TimedOut {
        let _ = out.half.shutdown().await;
    } else {
        let _ = finish(out, ErrorCode::Invalid).await;
    }
}

/// Says why the connection ends, as far as the peer still listens. Behind a
/// record that is only half written nothing is said: the next bytes would not
/// start a record, so the connection just closes.
async fn finish(out: &mut Out, code: ErrorCode) -> io::Result<()> {
    if !out.gate.mid() {
        let _ = out.put(&ErrorMeta { code }.encode()).await;
    }
    let _ = out.half.shutdown().await;
    Ok(())
}

async fn serve(
    plane: Option<DataPlane>,
    mut read: OwnedReadHalf,
    write: OwnedWriteHalf,
    shutdown: &mut watch::Receiver<bool>,
) -> io::Result<()> {
    let mut write = Out::new(write);
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
        Some(Lease::AgentFrames(lease)) => agent_frames(lease, read, write, shutdown).await,
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
async fn body(write: &mut Out, id: u64, bytes: &[u8]) -> io::Result<()> {
    for (index, chunk) in bytes.chunks(CHUNK_BYTES).enumerate() {
        let meta = ChunkMeta {
            id,
            offset: (index * CHUNK_BYTES) as u64,
        };
        send_chunk(write, &meta.encode_head(chunk.len()), chunk).await?;
    }
    let end = EndMeta {
        id,
        total_bytes: bytes.len() as u64,
    };
    send(write, &end.encode()).await
}

async fn send_frame(out: &FrameOut, read: &mut OwnedReadHalf, write: &mut Out) -> io::Result<()> {
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
    mut write: Out,
    shutdown: &mut watch::Receiver<bool>,
) -> io::Result<()> {
    let gate = write.gate.clone();
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
                    revoked.wait(),
                    &gate,
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

/// Runs `work`, which sends records, until it is done, `stop` resolves or the
/// core shuts down. A stop does not cut a record: the record in flight is
/// finished (the peer must keep making progress, and for no longer than
/// [`NO_PROGRESS`] after the stop), and the work ends before the next one
/// starts, so that what the peer has read ends on a record boundary.
async fn transfer(
    work: impl Future<Output = io::Result<()>>,
    stop: impl Future<Output = ()>,
    gate: &Gate,
    shutdown: &mut watch::Receiver<bool>,
) -> io::Result<Stop> {
    let mut work = std::pin::pin!(work);
    let mut stop = std::pin::pin!(stop);
    let mut grace = None;
    let whole = std::future::poll_fn(|cx| {
        if !gate.stopping() && stop.as_mut().poll(cx).is_ready() {
            gate.stop.store(true, Ordering::SeqCst);
            grace = Some(Box::pin(sleep(NO_PROGRESS)));
        }
        match work.as_mut().poll(cx) {
            Poll::Ready(Ok(())) => return Poll::Ready(Ok(Stop::Done)),
            Poll::Ready(Err(error))
                if error.kind() == io::ErrorKind::Interrupted && gate.stopping() =>
            {
                return Poll::Ready(Ok(Stop::Revoked));
            }
            Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
            Poll::Pending => {}
        }
        if !gate.stopping() {
            return Poll::Pending;
        }
        if !gate.mid() {
            return Poll::Ready(Ok(Stop::Revoked));
        }
        if grace
            .as_mut()
            .is_some_and(|g| g.as_mut().poll(cx).is_ready())
        {
            return Poll::Ready(Err(stalled()));
        }
        Poll::Pending
    });
    tokio::select! {
        done = whole => done,
        _ = shutdown.changed() => Ok(Stop::Shutdown),
    }
}

async fn send_blob(
    lease: &mut Option<BlobLease>,
    read: &mut OwnedReadHalf,
    write: &mut Out,
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
    mut write: Out,
    shutdown: &mut watch::Receiver<bool>,
) -> io::Result<()> {
    let gate = write.gate.clone();
    let mut revoked = lease.revoked();
    let mut lease = Some(lease);
    match transfer(
        send_blob(&mut lease, &mut read, &mut write),
        revoked.wait(),
        &gate,
        shutdown,
    )
    .await
    {
        Ok(Stop::Done) => {
            let _ = write.half.shutdown().await;
            Ok(())
        }
        Ok(Stop::Revoked) => finish(&mut write, ErrorCode::Closed).await,
        Ok(Stop::Shutdown) => Ok(()),
        Err(error) => {
            end_after(&mut write, &error).await;
            Err(error)
        }
    }
}

async fn agent_frames(
    mut lease: workbench_host::areas::browser::renderer::data::AgentFrameLease,
    mut read: OwnedReadHalf,
    mut write: Out,
    shutdown: &mut watch::Receiver<bool>,
) -> io::Result<()> {
    let gate = write.gate.clone();
    let mut revoked = lease.revoked();
    let mut order = SequenceGuard::default();
    let mut probe = [0; 1];
    loop {
        let frame = tokio::select! {
            biased;
            ()=revoked.wait()=>return finish(&mut write,ErrorCode::Closed).await,
            _=shutdown.changed()=>return Ok(()),
            _=read.read(&mut probe)=>return Ok(()),
            frame=lease.next()=>frame,
        };
        let Some(frame) = frame else {
            return finish(&mut write, ErrorCode::Closed).await;
        };
        if !lease.current() {
            return finish(&mut write, ErrorCode::Closed).await;
        }
        order.accept(frame.start.sequence).map_err(protocol)?;
        let sending = async {
            send(&mut write, &frame.start.encode().map_err(protocol)?).await?;
            body(&mut write, frame.start.sequence, &frame.rgba).await?;
            ack(&mut read, frame.start.sequence).await
        };
        let stop = transfer(
            sending,
            async {
                tokio::select! {
                    () = revoked.wait() => {},
                    () = lease.invalidated() => {},
                }
            },
            &gate,
            shutdown,
        )
        .await?;
        match stop {
            Stop::Done => (),
            Stop::Revoked => return finish(&mut write, ErrorCode::Closed).await,
            Stop::Shutdown => return Ok(()),
        }
    }
}

#[cfg(test)]
mod observation_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixStream;

    /// A peer that never reads: fill the socket until a write would block.
    async fn full_socket() -> (Out, OwnedReadHalf, UnixStream) {
        let (ours, peer) = UnixStream::pair().unwrap();
        let (read, write) = ours.into_split();
        let mut out = Out::new(write);
        let block = vec![0u8; 64 * 1024];
        while timeout(Duration::from_millis(50), out.half.write_all(&block))
            .await
            .is_ok()
        {}
        (out, read, peer)
    }

    /// Fills the socket until a write would block; returns the bytes written.
    async fn fill(out: &Out) -> usize {
        out.half.writable().await.unwrap();
        let block = vec![0u8; 64 * 1024];
        let mut total = 0;
        loop {
            match out.half.try_write(&block) {
                Ok(n) => total += n,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return total,
                Err(error) => panic!("{error}"),
            }
        }
    }

    #[tokio::test]
    async fn a_revoked_transfer_finishes_the_record_in_flight_and_then_says_why() {
        let (ours, mut peer) = UnixStream::pair().unwrap();
        let (_read, half) = ours.into_split();
        let mut out = Out::new(half);
        let gate = out.gate.clone();
        let filler = fill(&out).await;
        assert!(filler > 0, "the socket is full");
        let record = vec![0xAA; 1000];
        let (_keep, mut shutdown) = watch::channel(false);
        let revoked = Arc::new(tokio::sync::Notify::new());
        let (stop, drain) = (revoked.clone(), revoked);
        // The first poll leaves the record half written (the socket is full);
        // the stop comes with the next one, and only then does the peer read.
        let sender = async {
            let work = async {
                send(&mut out, &record).await?;
                send(&mut out, b"never sent").await
            };
            let stopped = transfer(
                work,
                async move {
                    tokio::task::yield_now().await;
                    stop.notify_one();
                },
                &gate,
                &mut shutdown,
            )
            .await
            .unwrap();
            assert!(matches!(stopped, Stop::Revoked));
            finish(&mut out, ErrorCode::Closed).await.unwrap();
            drop(out);
        };
        let reader = async move {
            drain.notified().await;
            let mut all = Vec::new();
            peer.read_to_end(&mut all).await.unwrap();
            all
        };
        let (_, all) = tokio::join!(sender, reader);
        let mut expected = vec![0u8; filler];
        expected.extend_from_slice(&record);
        expected.extend_from_slice(
            &ErrorMeta {
                code: ErrorCode::Closed,
            }
            .encode(),
        );
        assert!(all == expected, "the peer read {} bytes", all.len());
    }

    #[tokio::test(start_paused = true)]
    async fn a_revoked_transfer_does_not_wait_longer_than_the_no_progress_window() {
        let (ours, mut peer) = UnixStream::pair().unwrap();
        let (_read, half) = ours.into_split();
        let mut out = Out::new(half);
        let gate = out.gate.clone();
        fill(&out).await;
        let (_keep, mut shutdown) = watch::channel(false);
        // The peer reads a little every 4 s, so each attempt of the chunk
        // makes progress, yet the chunk needs far more than the window.
        let slow = tokio::spawn(async move {
            let mut small = [0u8; 8192];
            loop {
                sleep(Duration::from_secs(4)).await;
                if peer.read(&mut small).await.unwrap_or(0) == 0 {
                    return;
                }
            }
        });
        let started = tokio::time::Instant::now();
        let body = vec![1u8; CHUNK_BYTES];
        let error = transfer(
            send_chunk(&mut out, &[0u8; 16], &body),
            tokio::task::yield_now(),
            &gate,
            &mut shutdown,
        )
        .await
        .map(|_| ())
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(gate.mid());
        assert!(started.elapsed() >= NO_PROGRESS);
        assert!(started.elapsed() < NO_PROGRESS + Duration::from_secs(1));
        end_after(&mut out, &error).await;
        drop(out);
        slow.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_peer_is_cut_off_at_the_no_progress_window_and_not_told_why() {
        let (mut write, _read, peer) = full_socket().await;
        let started = tokio::time::Instant::now();
        let error = send(&mut write, &[0u8; 64 * 1024]).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        end_after(&mut write, &error).await;
        // The window is spent once: closing waits for nothing more.
        assert!(started.elapsed() < NO_PROGRESS + Duration::from_millis(100));
        assert!(started.elapsed() >= NO_PROGRESS);
        // The peer sees the connection end, with no record of why behind
        // the bytes that were already queued.
        drop(write);
        let mut peer = peer;
        let mut sink = vec![0u8; 1 << 20];
        let mut total = 0;
        loop {
            match peer.read(&mut sink).await.unwrap() {
                0 => break,
                n => total += n,
            }
        }
        assert!(total > 0);
    }
}
