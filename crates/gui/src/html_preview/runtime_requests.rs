//! CEF-independent handoff to the GUI thread, including hidden-window logic.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

type InitResult = Result<(), String>;
type SharedResult = Arc<Mutex<Option<InitResult>>>;

pub(crate) struct RuntimeRequests {
    requests: mpsc::Receiver<RuntimeRequest>,
    sender: mpsc::Sender<RuntimeRequest>,
    result: SharedResult,
}

#[derive(Clone)]
pub(crate) struct RuntimeRequestSender {
    requests: mpsc::Sender<RuntimeRequest>,
    result: SharedResult,
    ctx: egui::Context,
    timeout: Duration,
}

pub(crate) struct RuntimeRequest {
    reply: mpsc::Sender<InitResult>,
    result: SharedResult,
    cancelled: Arc<AtomicBool>,
}

impl RuntimeRequests {
    pub(crate) fn new() -> Self {
        let (sender, requests) = mpsc::channel();
        Self {
            requests,
            sender,
            result: Arc::default(),
        }
    }

    pub(crate) fn sender(&self, ctx: &egui::Context, timeout: Duration) -> RuntimeRequestSender {
        RuntimeRequestSender {
            requests: self.sender.clone(),
            result: Arc::clone(&self.result),
            ctx: ctx.clone(),
            timeout,
        }
    }

    pub(crate) fn take_pending(&self) -> Vec<RuntimeRequest> {
        self.requests.try_iter().collect()
    }

    pub(crate) fn finish(&self, result: InitResult) {
        *self
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(result);
    }
}

impl Drop for RuntimeRequests {
    fn drop(&mut self) {
        *self
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner()) =
            Some(Err("the GUI is shutting down".into()));
    }
}

impl RuntimeRequest {
    pub(crate) fn respond_with(self, initialize: impl FnOnce() -> InitResult) {
        // A timed-out request must not start CEF later when the window wakes.
        // An initialization already in progress is not interrupted.
        if self.cancelled.load(Ordering::Acquire) {
            return;
        }
        let cached = self
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let result = cached.unwrap_or_else(initialize);
        *self
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(result.clone());
        let _ = self.reply.send(result);
    }
}

impl RuntimeRequestSender {
    pub(crate) fn ensure_runtime(&self) -> InitResult {
        if let Some(result) = self
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
        {
            return result;
        }
        let (reply, response) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        self.requests
            .send(RuntimeRequest {
                reply,
                result: Arc::clone(&self.result),
                cancelled: Arc::clone(&cancelled),
            })
            .map_err(|_| "the GUI is shutting down".to_owned())?;
        self.ctx.request_repaint();
        match response.recv_timeout(self.timeout) {
            Ok(result) => result,
            Err(error) => {
                cancelled.store(true, Ordering::Release);
                Err(match error {
                    mpsc::RecvTimeoutError::Timeout => {
                        "the GUI did not initialize CEF in time".into()
                    }
                    mpsc::RecvTimeoutError::Disconnected => "the GUI is shutting down".into(),
                })
            }
        }
    }
}
