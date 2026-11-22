//! External Linux controller shared by B3-06, B3-07 and B3-08.
mod process;
mod tree;
pub use process::{Identity, PinnedProcess};
use std::{future::Future, io, time::Duration};
pub use tree::ProcessTree;

/// Failure watchdog only. Success always comes from an explicit event.
pub async fn bounded<T>(
    limit: Duration,
    what: &str,
    future: impl Future<Output = io::Result<T>>,
) -> io::Result<T> {
    tokio::time::timeout(limit, future)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, what.to_owned()))?
}
