//! Connect to a core that is already serving a configuration. This never
//! starts one: it takes no launch guard, reads no lifetime lock and has no
//! binary to run. The management CLI and a restart in progress use it; the
//! cold start stays [`crate::Launcher`]'s.
use crate::ConnectedCore;
use core_instance::{ConfigIdentity, validate_socket};
use std::{io, path::PathBuf};
use workbench_client::{ClientConfig, ClientError, TransportError, UnixTransport, WorkbenchClient};
use workbench_protocol::handshake::CoreState;

#[derive(Debug, Clone)]
pub struct ConnectTarget {
    config: ConfigIdentity,
    standard_socket: PathBuf,
}

impl ConnectTarget {
    pub fn new(config: ConfigIdentity) -> io::Result<Self> {
        let socket = config.default_socket()?;
        Ok(Self::with_socket(config, socket))
    }

    /// An explicit standard socket avoids reading the process environment.
    pub fn with_socket(config: ConfigIdentity, standard_socket: PathBuf) -> Self {
        Self {
            config,
            standard_socket,
        }
    }
}

/// What one attempt found. Nothing here started or stopped anything.
pub enum Attempt {
    /// No socket, or nothing accepted on it.
    NotRunning,
    /// A core answered but is not ready (or refused the handshake): it is
    /// going away, still starting, or not usable by this client. The
    /// connection was closed again.
    NotReady(String),
    Ready(ConnectedCore),
}

/// Where a connect-only attempt failed for a reason other than "no core".
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("core transport: {0}")]
    Transport(String),
}

/// Try once: the discovery record's socket first, then the standard one.
pub async fn attempt(
    target: &ConnectTarget,
    client: &ClientConfig,
) -> Result<Attempt, ConnectError> {
    let mut candidates = Vec::new();
    if let Some(record) = target.config.read_record()? {
        candidates.push(record.socket);
    }
    if !candidates.iter().any(|p| p == &target.standard_socket) {
        candidates.push(target.standard_socket.clone());
    }
    for socket in candidates {
        match validate_socket(&socket) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        }
        let transport = match UnixTransport::connect(&socket).await {
            Ok(t) => t,
            Err(TransportError::Disconnected) => continue,
            Err(e) => return Err(ConnectError::Transport(e.to_string())),
        };
        let connected = match WorkbenchClient::connect(transport, client.clone()).await {
            Ok(connected) => connected,
            // Accepted and gone before the hello answer: an owner on its way out.
            Err(ClientError::Transport(TransportError::Disconnected)) => {
                return Ok(Attempt::NotReady("closed during the handshake".into()));
            }
            Err(e) => return Ok(Attempt::NotReady(e.to_string())),
        };
        let hello = connected.server_hello();
        if hello.instance_epoch.get() == 0 || hello.status.state != CoreState::Ready {
            return Ok(Attempt::NotReady(format!(
                "core state {:?} at epoch {}",
                hello.status.state,
                hello.instance_epoch.get()
            )));
        }
        return Ok(Attempt::Ready(ConnectedCore {
            client: connected,
            endpoint: socket,
        }));
    }
    Ok(Attempt::NotRunning)
}
