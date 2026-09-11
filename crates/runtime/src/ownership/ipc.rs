use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use event_bus::{Event, EventBus, OwnershipAction, OwnershipEvent};
use serde::{Deserialize, Serialize};

use super::{Lease, Registry, RegistryError, ThreadOwner};

const MAX_FRAME: usize = 16 * 1024;

pub fn claim(
    registry: &mut Registry,
    request: ClaimRequest<'_>,
) -> Result<ThreadOwner, RegistryError> {
    claim_inner(registry, request, None)
}

pub(super) fn claim_configured(
    registry: &mut Registry,
    request: ClaimRequest<'_>,
    settings: &config::OwnershipConfig,
) -> Result<ThreadOwner, RegistryError> {
    claim_inner(registry, request, Some(settings))
}

fn claim_inner(
    registry: &mut Registry,
    request: ClaimRequest<'_>,
    settings: Option<&config::OwnershipConfig>,
) -> Result<ThreadOwner, RegistryError> {
    registry.update(request.thread_id, |owner| {
        owner.validate(request.expected)?;
        if owner.state != super::OwnerState::Released {
            match UnixStream::connect(request.previous_socket) {
                Ok(_) => return Err(super::OwnershipError::OwnerResponsive),
                Err(error) => match error.kind() {
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {}
                    _ => return Err(super::OwnershipError::OwnerResponsive),
                },
            }
        }
        let grace_ms = settings.map_or(request.grace_ms, |_| owner.settings.grace_ms.get());
        if let Some(settings) = settings {
            owner.settings = settings.clone();
        }
        owner.claim(request.expected, request.owner_id, request.now_ms, grace_ms)
    })
}

pub struct ClaimRequest<'a> {
    pub thread_id: &'a str,
    pub expected: &'a Lease,
    pub previous_socket: &'a Path,
    pub owner_id: &'a str,
    pub now_ms: u64,
    pub grace_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Request {
    Attach {
        thread_id: String,
    },
    Quiesce {
        thread_id: String,
        token: Lease,
    },
    Heartbeat {
        thread_id: String,
        token: Lease,
    },
    Handoff {
        thread_id: String,
        token: Lease,
        successor_id: String,
        settings: config::OwnershipConfig,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Status(ThreadOwner),
    Rejected(String),
}

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("IPC frame exceeds limit")]
    FrameTooLarge,
}

fn write_frame(stream: &mut UnixStream, value: &impl Serialize) -> Result<(), IpcError> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_FRAME {
        return Err(IpcError::FrameTooLarge);
    }
    let length = u32::try_from(bytes.len()).map_err(|_| IpcError::FrameTooLarge)?;
    stream.write_all(&length.to_be_bytes())?;
    stream.write_all(&bytes)?;
    Ok(())
}

fn read_frame<T: serde::de::DeserializeOwned>(stream: &mut UnixStream) -> Result<T, IpcError> {
    let mut length = [0; 4];
    stream.read_exact(&mut length)?;
    let length =
        usize::try_from(u32::from_be_bytes(length)).map_err(|_| IpcError::FrameTooLarge)?;
    if length > MAX_FRAME {
        return Err(IpcError::FrameTooLarge);
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn request(path: &Path, request: &Request) -> Result<Response, IpcError> {
    let mut stream = UnixStream::connect(path)?;
    timeouts(&stream)?;
    write_frame(&mut stream, request)?;
    read_frame(&mut stream)
}

fn timeouts(stream: &UnixStream) -> Result<(), std::io::Error> {
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    stream.set_write_timeout(Some(Duration::from_millis(500)))
}

pub fn serve_connection(
    stream: &mut UnixStream,
    registry: &mut Registry,
    now_ms: u64,
) -> Result<(), IpcError> {
    serve_connection_inner(stream, registry, now_ms, None)
}

pub fn serve_connection_with_bus(
    stream: &mut UnixStream,
    registry: &mut Registry,
    now_ms: u64,
    bus: &EventBus,
) -> Result<(), IpcError> {
    serve_connection_inner(stream, registry, now_ms, Some(bus))
}

fn serve_connection_inner(
    stream: &mut UnixStream,
    registry: &mut Registry,
    now_ms: u64,
    bus: Option<&EventBus>,
) -> Result<(), IpcError> {
    timeouts(stream)?;
    let request: Request = read_frame(stream)?;
    let action = match &request {
        Request::Quiesce { .. } => Some(OwnershipAction::Quiescing),
        Request::Heartbeat { .. } => Some(OwnershipAction::Heartbeat),
        Request::Handoff { .. } => Some(OwnershipAction::Handoff),
        Request::Attach { .. } => None,
    };
    let result: Result<ThreadOwner, RegistryError> = match request {
        Request::Attach { thread_id } => registry.attach(&thread_id),
        Request::Quiesce { thread_id, token } => registry.update(&thread_id, |owner| {
            owner.quiesce(&token)?;
            if !owner.active_turn {
                owner.release(&token)?;
            }
            Ok(())
        }),
        Request::Heartbeat { thread_id, token } => {
            registry.update(&thread_id, |owner| owner.heartbeat(&token, now_ms))
        }
        Request::Handoff {
            thread_id,
            token,
            successor_id,
            settings,
        } => registry.update(&thread_id, |owner| {
            owner.quiesce(&token)?;
            owner.release(&token)?;
            owner.settings = settings;
            owner.claim(&token, &successor_id, now_ms, 0)
        }),
    };
    let response = match result {
        Ok(owner) => {
            if let (Some(bus), Some(action)) = (bus, action) {
                let action = if owner.state == super::OwnerState::Released {
                    OwnershipAction::Released
                } else {
                    action
                };
                bus.emit(Event::new(OwnershipEvent {
                    thread_id: owner.thread_id.clone(),
                    owner_id: owner.lease.owner_id.clone(),
                    generation: owner.lease.generation,
                    action,
                }));
            }
            Response::Status(owner)
        }
        Err(error) => Response::Rejected(error.to_string()),
    };
    write_frame(stream, &response)
}
