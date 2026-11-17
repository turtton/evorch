//! Owned control-drain request. Only the lifecycle owning the listener and all
//! Unix connection tasks can acknowledge it. A caller's boolean is not proof.
use crate::backend::HostBackend;
use std::{io, sync::Arc};
use tokio::sync::{mpsc, oneshot};
pub(super) struct Request {
    pub deadline: tokio::time::Instant,
    pub reply: oneshot::Sender<io::Result<ControlDrained>>,
}
#[derive(Clone)]
pub struct ControlDrainHandle(pub(super) mpsc::UnboundedSender<Request>);
pub struct ControlDrained {
    pub(super) backend: Arc<HostBackend>,
}
impl ControlDrainHandle {
    pub async fn drain(&self, deadline: tokio::time::Instant) -> io::Result<ControlDrained> {
        let (send, reply) = oneshot::channel();
        self.0
            .send(Request {
                deadline,
                reply: send,
            })
            .map_err(|_| io::Error::other("control owner unavailable"))?;
        reply
            .await
            .map_err(|_| io::Error::other("control drain unconfirmed"))?
    }
}
impl ControlDrained {
    pub fn validate(&self, identity: &str, epoch: u64) -> io::Result<()> {
        let hello = self.backend.hello();
        if hello.core.as_str() != identity || hello.instance_epoch.0 != epoch {
            return Err(io::Error::other(
                "control drain belongs to another core/epoch",
            ));
        }
        Ok(())
    }
}
