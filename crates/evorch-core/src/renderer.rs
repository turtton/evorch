//! Seam for the dedicated renderer frame connection of the local UDS.
//!
//! `unix::connection` hands the first frame of every connection and the two
//! halves of its stream to [`route`] before it treats the frame as a
//! `ClientHello`. A leaf that claims the connection keeps the halves and returns
//! `None`; otherwise it returns them and the control-plane handling goes on.
//! The integration PR never claims, so every connection keeps the handling it
//! had; the frame-delivery PR replaces this leaf and changes no other file for
//! the hand-off.
use crate::backend::HostBackend;
use std::{io, sync::Arc};
use tokio::{
    net::unix::{OwnedReadHalf, OwnedWriteHalf},
    sync::watch,
};

/// The halves of a connection that stays a control connection.
pub(crate) type Halves = (OwnedReadHalf, OwnedWriteHalf);

/// Decide whether the first frame opens a renderer frame connection. `Some`
/// gives the halves back to the control-plane handling, `None` means the leaf
/// took the connection over and served it to its end.
///
/// # Errors
/// A claimed connection that failed.
#[allow(clippy::unused_async)]
pub(crate) async fn route(
    _first_frame: &[u8],
    read: OwnedReadHalf,
    write: OwnedWriteHalf,
    _backend: &Arc<HostBackend>,
    _shutdown: &mut watch::Receiver<bool>,
) -> io::Result<Option<Halves>> {
    Ok(Some((read, write)))
}
