//! Internal IPC between the evorch core and the `evorch-renderer` helper
//! (ADR 0045, plan 16a).
//!
//! The helper owns CEF; the core must not link it. This crate is the only
//! thing both sides share, so it depends on no CEF, egui, runtime, storage or
//! workbench crate and carries no database handle or credential: only opaque
//! ids, bounded requests and replies, and raw BGRA frames.
//!
//! Two Unix-socket channels connect the processes. The *control* channel
//! carries requests, responses and events; the *frame* channel carries raw
//! frames from the helper and the core's acknowledgements. Keeping them apart
//! means a large frame in flight never delays a response or a shutdown.
//! Measurements (spike 16a-1) showed raw BGRA over a Unix socket meets the
//! 30 fps / 33 ms budget at 1280x720, so no shared memory is used.
//!
//! Production helpers receive the channels as stdin (control) and stdout
//! (frames), because evorch's launch policy lets a child inherit only its
//! standard streams: [`stdio_channels`] builds the pair and the matching
//! `Stdio` values.

#![cfg(unix)]

pub mod client;
pub mod codec;
pub mod frames;
pub mod ids;
pub mod protocol;
pub mod server;

pub use client::{ClientConfig, ClientError, ClientStats, CloseReason, HelperInfo, RendererClient};
pub use frames::{Frame, FrameQueue, FrameStats};
pub use protocol::*;
pub use server::{
    Backend, BackendContext, EventSink, ExitReason, MAX_QUEUED_REQUESTS, ServeConfig, serve,
    serve_in_thread,
};

use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::Stdio;

/// The core's two ends of a new helper connection, and the descriptors to give
/// the helper as stdin (control) and stdout (frames).
pub struct HelperChannels {
    pub control: UnixStream,
    pub frames: UnixStream,
    pub helper_stdin: Stdio,
    pub helper_stdout: Stdio,
}

/// Creates the socket pairs for one helper. The core keeps `control` and
/// `frames`; dropping `helper_stdin` and `helper_stdout` after the spawn
/// closes the core's copies of the helper's ends, so the helper's exit is seen
/// as a closed channel.
pub fn stdio_channels() -> io::Result<HelperChannels> {
    let (control, helper_control) = UnixStream::pair()?;
    let (frames, helper_frames) = UnixStream::pair()?;
    Ok(HelperChannels {
        control,
        frames,
        helper_stdin: Stdio::from(OwnedFd::from(helper_control)),
        helper_stdout: Stdio::from(OwnedFd::from(helper_frames)),
    })
}
