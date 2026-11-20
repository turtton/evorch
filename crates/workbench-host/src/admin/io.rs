//! One host-owned blocking reader worker. Callers only enqueue bounded jobs.
use super::*;
use std::sync::{
    Arc, Mutex,
    mpsc::{self, Receiver, SyncSender},
};
type Job = Box<dyn FnOnce() + Send + 'static>;
pub struct AdminReader {
    sender: Mutex<Option<SyncSender<Job>>>,
    receiver: Mutex<Option<Receiver<Job>>>,
}
impl AdminReader {
    pub(crate) fn new() -> Arc<Self> {
        let (sender, receiver) = mpsc::sync_channel(64);
        Arc::new(Self {
            sender: Mutex::new(Some(sender)),
            receiver: Mutex::new(Some(receiver)),
        })
    }
    pub async fn read<T: Send + 'static>(
        &self,
        read: impl FnOnce() -> Result<T, AdminFailure> + Send + 'static,
    ) -> Result<T, AdminFailure> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let job = Box::new(move || {
            if tx.is_closed() {
                return;
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(read))
                .unwrap_or_else(|_| Err(unknown()));
            let _ = tx.send(result);
        });
        self.sender
            .lock()
            .unwrap()
            .as_ref()
            .ok_or_else(busy)?
            .try_send(job)
            .map_err(|_| busy())?;
        tokio::time::timeout(audit::ADMIN_IO_ACK_BUDGET, rx)
            .await
            .map_err(|_| unknown())?
            .map_err(|_| unknown())?
    }
    pub(crate) fn register(
        self: &Arc<Self>,
        services: &mut crate::bootstrap::services::BackgroundServices,
    ) {
        let Some(receiver) = self.receiver.lock().unwrap().take() else {
            return;
        };
        let owner = self.clone();
        services.start("admin-read-io", move |stop| async move {
            let worker = tokio::task::spawn_blocking(move || {
                while let Ok(job) = receiver.recv() {
                    job();
                }
            });
            let _ = stop.await;
            owner.sender.lock().unwrap().take();
            worker
                .await
                .map_err(|error| -> crate::bootstrap::BootstrapError { Box::new(error) })?;
            Ok(())
        });
    }
}
