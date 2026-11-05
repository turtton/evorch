//! Errors a caller of the client can see.

use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::handshake::{Capability, HandshakeRejected};
use workbench_protocol::ids::RequestId;
use workbench_protocol::validation::ValidationError;

/// Failures of the connection itself, as opposed to the core refusing a request.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    /// The connection is closed. A request that failed this way before being
    /// written definitely did not run; it may be sent again.
    #[error("disconnected from the core")]
    Disconnected,
    /// The peer sent something this protocol does not allow. The connection is closed.
    #[error("protocol violation: {0}")]
    ProtocolViolation(String),
    /// A frame exceeded the negotiated size, in either direction.
    #[error("frame of {len} bytes exceeds the limit of {max}")]
    FrameTooLarge { len: u64, max: u32 },
    /// Too many requests are unanswered, or the write queue is full. Nothing was sent.
    #[error("too many requests in flight")]
    QueueFull,
    /// The command may have been written but its reply never arrived. The
    /// client never resends it; ask the core with `GetCommandReceipt` for this
    /// request id once reconnected.
    #[error("outcome of request {request_id} is unknown")]
    OutcomeUnknown { request_id: RequestId },
}

/// Everything `connect`, `command`, `query` and `subscribe` can fail with.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClientError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    /// The core understood the request and refused it.
    #[error("core refused the request: {0}")]
    Server(WorkbenchError),
    /// The request broke a protocol rule; it was not sent.
    #[error("invalid request: {0}")]
    Invalid(#[from] ValidationError),
    /// The core did not negotiate the capability this request needs; nothing was sent.
    #[error("capability {} was not negotiated", .0.name())]
    UnsupportedCapability(Capability),
    /// The core refused the connection.
    #[error("handshake rejected: {0:?}")]
    HandshakeRejected(HandshakeRejected),
}

impl From<WorkbenchError> for ClientError {
    fn from(error: WorkbenchError) -> Self {
        Self::Server(error)
    }
}
