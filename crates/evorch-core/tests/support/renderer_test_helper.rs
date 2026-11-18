//! A fake `evorch-renderer` for the owned-spawn tests (PR 16a-2): it takes the
//! control channel from stdin and the frame channel from stdout exactly as the
//! real helper does, serves the `renderer-ipc` protocol with a backend that
//! renders nothing, and exits when the core shuts it down or closes the
//! control channel. No CEF, no timers: every step waits on a message.
//!
//! `evorch-renderer-test-helper --cache-dir <absolute dir>` (the flag is
//! accepted so the core starts it like the real helper, and is otherwise
//! unused). Not shipped; built only with the `renderer-test-helper` feature.
use renderer_ipc::{
    Backend, BackendContext, ExitReason, Notices, OpenArtifact, RemoteError, ServeConfig, TabId,
    Viewport, serve,
};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::time::Duration;

struct Blank;

impl Backend for Blank {
    fn open_artifact(
        &self,
        open: &OpenArtifact,
        _files: Vec<(String, String, Vec<u8>)>,
    ) -> Result<Viewport, RemoteError> {
        Ok(open.viewport)
    }
    fn resize(&self, _: TabId, _: Viewport) -> Result<(), RemoteError> {
        Ok(())
    }
    fn set_streaming(&self, _: TabId, _: bool) -> Result<(), RemoteError> {
        Ok(())
    }
    fn capture(&self, _: TabId, _: Duration) -> Result<Vec<u8>, RemoteError> {
        Err(RemoteError::internal("the test helper renders nothing"))
    }
    fn cdp(
        &self,
        _: TabId,
        _: &str,
        _: serde_json::Value,
        _: Duration,
    ) -> Result<serde_json::Value, RemoteError> {
        Ok(serde_json::Value::Null)
    }
    fn settle(&self, _: TabId, _: Duration) -> Result<(), RemoteError> {
        Ok(())
    }
    fn take_notices(&self, _: TabId) -> Result<Notices, RemoteError> {
        Ok(Notices::default())
    }
    fn close_tab(&self, _: TabId) {}
    fn shutdown(&self) {}
}

fn adopt(fd: impl AsFd) -> std::io::Result<UnixStream> {
    Ok(UnixStream::from(fd.as_fd().try_clone_to_owned()?))
}

fn main() -> ExitCode {
    let channels = adopt(std::io::stdin()).and_then(|c| Ok((c, adopt(std::io::stdout())?)));
    let Ok((control, frames)) = channels else {
        eprintln!("evorch-renderer-test-helper: stdin and stdout must be sockets");
        return ExitCode::from(2);
    };
    let reason = serve(
        control,
        frames,
        ServeConfig::default(),
        |_: BackendContext| Ok(Blank),
    );
    match reason {
        ExitReason::ShutdownRequested | ExitReason::PeerClosed => ExitCode::SUCCESS,
        ExitReason::InitFailed(_) | ExitReason::HandshakeFailed(_) => ExitCode::from(2),
        ExitReason::ProtocolViolation(_) | ExitReason::FrameChannelFailed(_) => ExitCode::from(1),
    }
}
