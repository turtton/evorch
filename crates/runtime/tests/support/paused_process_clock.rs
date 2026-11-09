//! Keep virtual time under the test's control while real processes do work.

/// Tokio normally advances a paused clock when its async tasks are idle. An
/// outstanding blocking task inhibits that automatic advance, so waiting for a
/// real child's pipe cannot accidentally run its timeout before it is scheduled.
/// Drop the sender to release the blocking task. No thread spins or sleeps.
pub fn hold() -> tokio::sync::oneshot::Sender<()> {
    let (release, held) = tokio::sync::oneshot::channel();
    tokio::task::spawn_blocking(move || {
        let _ = held.blocking_recv();
    });
    release
}
