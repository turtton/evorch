//! Shared hosts have neither an inbox nor a path to mutable finalization.
#[cfg(target_os = "linux")]
use super::{
    finalization::{FinalizationHandle, FinalizationInbox, PreparedFinalization},
    product_exit::ProductExitContext,
};
use workbench_host::bootstrap::BootstrappedHost;
pub(super) trait Listener {
    fn accept(
        &self,
    ) -> impl std::future::Future<
        Output = std::io::Result<(tokio::net::UnixStream, tokio::net::unix::SocketAddr)>,
    > + Send;
}
impl Listener for tokio::net::UnixListener {
    fn accept(
        &self,
    ) -> impl std::future::Future<
        Output = std::io::Result<(tokio::net::UnixStream, tokio::net::unix::SocketAddr)>,
    > + Send {
        self.accept()
    }
}
pub(super) enum LoopHost<'a> {
    Shared(&'a BootstrappedHost),
    #[cfg(target_os = "linux")]
    Owned {
        host: &'a mut BootstrappedHost,
        context: &'a ProductExitContext,
        inbox: Option<FinalizationInbox>,
    },
}
impl LoopHost<'_> {
    pub(super) fn get(&self) -> &BootstrappedHost {
        match self {
            Self::Shared(host) => host,
            #[cfg(target_os = "linux")]
            Self::Owned { host, .. } => host,
        }
    }
    #[cfg(target_os = "linux")]
    pub(super) async fn recv_finalization(&mut self) -> Option<PreparedFinalization> {
        match self {
            Self::Shared(_) => std::future::pending().await,
            Self::Owned {
                inbox: Some(inbox), ..
            } => inbox.recv().await,
            Self::Owned { inbox: None, .. } => std::future::pending().await,
        }
    }
    #[cfg(not(target_os = "linux"))]
    pub(super) async fn recv_finalization(&mut self) -> Option<()> {
        std::future::pending().await
    }
    /// A mailbox winner must observe an already-ready signal before capture.
    /// This adds no priority changes among the loop's non-mailbox branches.
    #[cfg(target_os = "linux")]
    pub(super) async fn dispatch(
        &mut self,
        command: PreparedFinalization,
        handle: &FinalizationHandle,
        signals: &mut super::OperatorSignals,
    ) -> bool {
        tokio::select! {
            biased;
            _ = signals.received() => false,
            _ = std::future::ready(()) => {
                if let Self::Owned { host, context, .. } = self { command.execute(host, context, handle); }
                true
            },
        }
    }
    pub(super) fn close_finalization(&mut self) {
        #[cfg(target_os = "linux")]
        if let Self::Owned { inbox, .. } = self {
            drop(inbox.take());
        }
    }
}
