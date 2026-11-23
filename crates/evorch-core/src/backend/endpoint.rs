//! A launcher attaches only when the record's epoch equals the hello's, and the
//! hello epoch advances on every projection invalidation.
use core_instance::Instance;
use std::{
    io,
    sync::{Arc, Mutex},
};
use tokio::sync::watch;

#[derive(Default)]
struct Owner {
    stopped: bool,
    instance: Option<Arc<Instance>>,
}

#[derive(Default)]
pub(super) struct Endpoint {
    owner: Arc<Mutex<Owner>>,
    task: Mutex<Option<tokio::task::AbortHandle>>,
    published: Mutex<Option<watch::Receiver<u64>>>,
}

impl Endpoint {
    pub(super) fn start(
        &self,
        instance: Arc<Instance>,
        mut epochs: watch::Receiver<u64>,
        published: u64,
    ) {
        {
            let mut owner = self.owner.lock().unwrap();
            if owner.stopped {
                return;
            }
            owner.instance = Some(instance);
        }
        if *epochs.borrow_and_update() != published {
            epochs.mark_changed();
        }
        let (written, observed) = watch::channel(published);
        let owner = self.owner.clone();
        let task = tokio::spawn(async move {
            while epochs.changed().await.is_ok() {
                let epoch = *epochs.borrow_and_update();
                let owner = owner.clone();
                match tokio::task::spawn_blocking(move || publish(&owner, epoch)).await {
                    Ok(Ok(true)) => {
                        written.send_replace(epoch);
                    }
                    Ok(Err(error)) => {
                        tracing::warn!(%error, epoch, "endpoint record republish failed");
                    }
                    Ok(Ok(false)) | Err(_) => break,
                }
            }
        });
        *self.task.lock().unwrap() = Some(task.abort_handle());
        *self.published.lock().unwrap() = Some(observed);
    }

    /// Once this returns, the record is never written again, so a successor's
    /// record cannot be overwritten by its predecessor.
    pub(super) fn stop(&self) {
        {
            let mut owner = self.owner.lock().unwrap();
            owner.stopped = true;
            owner.instance = None;
        }
        if let Some(task) = self.task.lock().unwrap().take() {
            task.abort();
        }
    }

    pub(super) fn published(&self) -> Option<watch::Receiver<u64>> {
        self.published.lock().unwrap().clone()
    }
}

fn publish(owner: &Mutex<Owner>, epoch: u64) -> io::Result<bool> {
    let owner = owner.lock().unwrap();
    let Some(instance) = owner.instance.as_ref() else {
        return Ok(false);
    };
    instance.publish(epoch).map(|()| true)
}
