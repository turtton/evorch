use std::sync::Arc;

use event_bus::event::diagnostic_codes;
use tokio::sync::{mpsc, oneshot, watch};

use crate::{BrowserAction, BrowserError, BrowserReport, cdp, diagnostics, validate_action};

/// One Chromium process with a single screencast page. Dropping it closes the browser.
pub struct BrowserSession {
    commands: mpsc::Sender<BrowserAction>,
    frames: watch::Receiver<Option<image::RgbaImage>>,
    error: watch::Receiver<Option<String>>,
    stop: Option<oneshot::Sender<()>>,
    reports: mpsc::Receiver<BrowserReport>,
}

impl BrowserSession {
    pub fn start(
        runtime: &tokio::runtime::Handle,
        bus: Arc<event_bus::EventBus>,
        headful: bool,
    ) -> Self {
        let (commands, rx) = mpsc::channel(8);
        let (frames_tx, frames) = watch::channel(None);
        let (error_tx, error) = watch::channel(None);
        let (stop, shutdown) = oneshot::channel();
        let (reports_tx, reports) = mpsc::channel(32);
        runtime.spawn(async move {
            if let Err(error) = cdp::run(
                bus.clone(),
                headful,
                cdp::Channels {
                    commands: rx,
                    frames: frames_tx,
                    shutdown,
                    reports: reports_tx,
                },
            )
            .await
            {
                diagnostics::emit(
                    &bus,
                    diagnostic_codes::BROWSER_ERROR,
                    &error.to_string(),
                    true,
                );
                error_tx.send_replace(Some(error.to_string()));
            }
        });
        Self {
            commands,
            frames,
            error,
            stop: Some(stop),
            reports,
        }
    }

    /// The latest screencast frame, once per change.
    pub fn poll_frame(&mut self) -> Option<image::RgbaImage> {
        match self.frames.has_changed() {
            Ok(true) => self.frames.borrow_and_update().clone(),
            Ok(false) | Err(_) => None,
        }
    }

    pub fn submit(&self, action: BrowserAction) -> Result<(), BrowserError> {
        validate_action(&action)?;
        self.commands
            .try_send(action)
            .map_err(|_| BrowserError::Unavailable)
    }

    pub fn error(&self) -> Option<String> {
        self.error.borrow().clone()
    }

    pub fn poll_report(&mut self) -> Option<BrowserReport> {
        self.reports.try_recv().ok()
    }
}

impl Drop for BrowserSession {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}
