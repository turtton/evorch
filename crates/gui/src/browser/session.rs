use std::sync::Arc;

use tokio::sync::{mpsc, oneshot, watch};

use super::{BrowserAction, BrowserError, FrameSource, cdp, diagnostics, validate_action};

pub struct ChromiumSource {
    commands: mpsc::Sender<BrowserAction>,
    frames: watch::Receiver<Option<egui::ColorImage>>,
    error: watch::Receiver<Option<String>>,
    stop: Option<oneshot::Sender<()>>,
}

impl ChromiumSource {
    pub fn start(
        runtime: &tokio::runtime::Handle,
        bus: Arc<event_bus::EventBus>,
        headful: bool,
    ) -> Self {
        let (commands, rx) = mpsc::channel(8);
        let (frames_tx, frames) = watch::channel(None);
        let (error_tx, error) = watch::channel(None);
        let (stop, shutdown) = oneshot::channel();
        runtime.spawn(async move {
            if let Err(error) = cdp::run(
                bus.clone(),
                headful,
                cdp::Channels {
                    commands: rx,
                    frames: frames_tx,
                    shutdown,
                },
            )
            .await
            {
                diagnostics::emit(&bus, "browser.error", &error.to_string(), true);
                error_tx.send_replace(Some(error.to_string()));
            }
        });
        Self {
            commands,
            frames,
            error,
            stop: Some(stop),
        }
    }
}

impl FrameSource for ChromiumSource {
    fn poll_frame(&mut self) -> Option<egui::ColorImage> {
        match self.frames.has_changed() {
            Ok(true) => self.frames.borrow_and_update().clone(),
            Ok(false) | Err(_) => None,
        }
    }

    fn submit(&mut self, action: BrowserAction) -> Result<(), BrowserError> {
        validate_action(&action)?;
        self.commands
            .try_send(action)
            .map_err(|_| BrowserError::Unavailable)
    }

    fn error(&self) -> Option<String> {
        self.error.borrow().clone()
    }
}

impl Drop for ChromiumSource {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}
