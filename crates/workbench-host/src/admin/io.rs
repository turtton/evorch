//! One host-owned reader worker. Callers only enqueue bounded jobs.
use super::*;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::{self, Receiver, Sender};
type Job = Box<dyn FnOnce() + Send + 'static>;
pub struct AdminReader {
    sender: Mutex<Option<Sender<Job>>>,
    receiver: Mutex<Option<Receiver<Job>>>,
}
impl AdminReader {
    pub(crate) fn new() -> Arc<Self> {
        let (sender, receiver) = mpsc::channel(64);
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
        let Some(mut receiver) = self.receiver.lock().unwrap().take() else {
            return;
        };
        let owner = self.clone();
        services.start("admin-read-io", move |stop| async move {
            // An empty queue must not occupy a blocking thread for the host's
            // lifetime: that also prevents Tokio's paused clock from advancing
            // through idle grace. Only an actual read enters the blocking pool.
            let worker = async move {
                while let Some(job) = receiver.recv().await {
                    tokio::task::spawn_blocking(job).await?;
                }
                Ok::<(), tokio::task::JoinError>(())
            };
            tokio::pin!(worker);
            let result = tokio::select! {
                result = &mut worker => result,
                _ = stop => {
                    owner.sender.lock().unwrap().take();
                    // Do not drop an in-flight blocking read. Closing the sole
                    // sender also lets this same worker drain accepted jobs.
                    worker.await
                }
            };
            owner.sender.lock().unwrap().take();
            result.map_err(|error| -> crate::bootstrap::BootstrapError { Box::new(error) })?;
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::services::BackgroundServices;

    #[tokio::test(start_paused = true)]
    async fn idle_reader_allows_time_to_advance_without_a_read() {
        let reader = AdminReader::new();
        let mut services = BackgroundServices::default();
        reader.register(&mut services);
        let start = tokio::time::Instant::now();
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        assert_eq!(start.elapsed(), std::time::Duration::from_secs(1));
        services.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_joins_the_current_read_and_drains_accepted_jobs() {
        let reader = AdminReader::new();
        let mut services = BackgroundServices::default();
        reader.register(&mut services);
        let (entered, entering) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let order = Arc::new(Mutex::new(Vec::new()));
        let first = tokio::spawn({
            let reader = reader.clone();
            let order = order.clone();
            async move {
                reader
                    .read(move || {
                        entered.send(()).unwrap();
                        released.recv().unwrap();
                        order.lock().unwrap().push(1);
                        Ok(1)
                    })
                    .await
            }
        });
        entering.await.unwrap();
        let second = reader.read({
            let order = order.clone();
            move || {
                order.lock().unwrap().push(2);
                Ok(2)
            }
        });
        tokio::pin!(second);
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(
                second.as_mut().poll(cx).is_pending()
            ))
            .await
        );
        let stopping = services.shutdown();
        tokio::pin!(stopping);
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(
                stopping.as_mut().poll(cx).is_pending()
            ))
            .await
        );
        assert!(order.lock().unwrap().is_empty());
        release.send(()).unwrap();
        stopping.await.unwrap();
        assert_eq!(first.await.unwrap().unwrap(), 1);
        assert_eq!(second.await.unwrap(), 2);
        assert_eq!(*order.lock().unwrap(), [1, 2]);
        assert_eq!(
            reader.read(|| Ok(3)).await.unwrap_err().code,
            workbench_protocol::errors::ErrorCode::Busy
        );
    }
}
