//! Connection-owned synchronous work. No queue or permits in addition to the
//! bounded connections and their serial dispatch.
use crate::backend::HostBackend;
use std::sync::Arc;
use tokio::task::JoinHandle;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Job {
    Request,
    Subscribe,
    Unsubscribe,
    Goodbye,
}
impl Job {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Subscribe => "subscribe",
            Self::Unsubscribe => "unsubscribe",
            Self::Goodbye => "goodbye",
        }
    }
}

impl HostBackend {
    pub(crate) fn blocking<T: Send + 'static>(
        self: &Arc<Self>,
        job: Job,
        work: impl FnOnce(&Self) -> T + Send + 'static,
    ) -> JoinHandle<T> {
        let backend = self.clone();
        #[cfg(test)]
        let hook = self.blocking_hooks.lock().unwrap().take(job);
        #[cfg(not(test))]
        let _ = job;
        tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            let _completion = hook.map(|hook| hook.enter());
            work(&backend)
        })
    }
}

// The seam lives only at the core boundary, never in a production handler.
#[cfg(test)]
pub(crate) mod testing {
    use super::Job;
    use tokio::sync::oneshot;

    pub(crate) const PANIC_MARKER: &str = "injected blocking job panic";

    #[derive(Default)]
    pub(crate) struct Hooks(Vec<(Job, Hook)>);
    impl Hooks {
        pub(crate) fn hold(&mut self, job: Job, panic: bool) -> Gate {
            let (started_tx, started) = oneshot::channel();
            let (release, release_rx) = std::sync::mpsc::channel();
            let (completed_tx, completed) = oneshot::channel();
            self.0.push((
                job,
                Hook {
                    started: started_tx,
                    release: release_rx,
                    completed: completed_tx,
                    panic,
                },
            ));
            Gate {
                started,
                release,
                completed,
            }
        }
        pub(crate) fn take(&mut self, job: Job) -> Option<Hook> {
            let index = self.0.iter().position(|(kind, _)| *kind == job)?;
            Some(self.0.remove(index).1)
        }
    }
    pub(crate) struct Gate {
        pub started: oneshot::Receiver<()>,
        pub release: std::sync::mpsc::Sender<()>,
        pub completed: oneshot::Receiver<()>,
    }
    pub(crate) struct Hook {
        started: oneshot::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
        completed: oneshot::Sender<()>,
        panic: bool,
    }
    pub(crate) struct Completion(Option<oneshot::Sender<()>>);
    impl Drop for Completion {
        fn drop(&mut self) {
            let _ = self.0.take().unwrap().send(());
        }
    }
    impl Hook {
        pub(crate) fn enter(self) -> Completion {
            let completion = Completion(Some(self.completed));
            let _ = self.started.send(());
            // A failed test drops the sender, so it cannot strand the runtime.
            let _ = self.release.recv();
            assert!(!self.panic, "{PANIC_MARKER}");
            completion
        }
    }
}
