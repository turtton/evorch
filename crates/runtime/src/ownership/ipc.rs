use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use event_bus::{Event, EventBus, OwnershipAction, OwnershipEvent};
use serde::{Deserialize, Serialize};

use super::{Lease, Registry, RegistryError, ThreadOwner};

const MAX_FRAME: usize = 16 * 1024;

/// Probe an OwnerHost endpoint without blocking or taking any registry lock.
/// A responsive or uncertain endpoint protects its runs; only absence/refusal
/// proves death.
/// This observation is not authority to mutate or claim an ownership generation.
pub fn owner_may_be_live(root: &Path, owner_id: &str) -> bool {
    super::host::socket_path(root, owner_id).map_or(true, |socket| endpoint_may_be_live(&socket))
}

fn endpoint_may_be_live(socket: &Path) -> bool {
    let Ok(address) = socket2::SockAddr::unix(socket) else {
        return true;
    };
    let Ok(client) = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)
    else {
        return true;
    };
    // Configure this before connect: a live owner's accept backlog can be full
    // while its server is stalled. EAGAIN/EINPROGRESS still protect that owner.
    if client.set_nonblocking(true).is_err() {
        return true;
    }
    match client.connect(&address) {
        Ok(()) => true,
        Err(error) => !matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
        ),
    }
}

pub fn claim(
    registry: &mut Registry,
    request: ClaimRequest<'_>,
) -> Result<ThreadOwner, RegistryError> {
    claim_inner(registry, request, None, false)
}

pub(super) fn claim_configured(
    registry: &mut Registry,
    request: ClaimRequest<'_>,
    settings: &config::OwnershipConfig,
) -> Result<ThreadOwner, RegistryError> {
    claim_inner(registry, request, Some(settings), false)
}

/// Automatic cold claim: all eligibility checks use the row re-read under the
/// IMMEDIATE transaction. Unlike explicit claim, Released is not an exception.
pub(super) fn claim_abandoned(
    registry: &mut Registry,
    request: ClaimRequest<'_>,
    settings: &config::OwnershipConfig,
) -> Result<ThreadOwner, RegistryError> {
    claim_inner(registry, request, Some(settings), true)
}

fn claim_inner(
    registry: &mut Registry,
    request: ClaimRequest<'_>,
    settings: Option<&config::OwnershipConfig>,
    abandoned: bool,
) -> Result<ThreadOwner, RegistryError> {
    // Recorded as a claim, not a generic update. Like every callback-driven
    // update, a claim is never retried.
    registry.update_as(super::DbOperation::Claim, request.thread_id, |owner| {
        owner.validate(request.expected)?;
        if abandoned {
            if owner.has_planned_hold() {
                return Err(super::OwnershipError::PlannedHeld);
            }
            if owner
                .lease
                .observe(request.now_ms, owner.settings.grace_ms.get())
                != super::OwnerState::Stale
            {
                return Err(super::OwnershipError::NotClaimable);
            }
        }
        if (abandoned || owner.state != super::OwnerState::Released)
            && endpoint_may_be_live(request.previous_socket)
        {
            return Err(super::OwnershipError::OwnerResponsive);
        }
        let grace_ms = settings.map_or(request.grace_ms, |_| owner.settings.grace_ms.get());
        if let Some(settings) = settings {
            owner.settings = settings.clone();
        }
        owner.claim(request.expected, request.owner_id, request.now_ms, grace_ms)
    })
}

/// Limited claim: caller supplies Adopted evidence, never a PID-death check.
/// Connecting to the predecessor endpoint is conservative: even a listener that
/// cannot answer still blocks adoption. exec successors have the same PID.
/// Liveness and generation/hold/run-set checks occur inside the registry write
/// transaction; the claim is never retried implicitly.
pub(super) fn claim_planned(
    registry: &mut Registry,
    hold: &super::PlannedHold,
    root: &Path,
    successor: &str,
    now_ms: u64,
    settings: &config::OwnershipConfig,
    evidence: &dyn super::AdoptedOwnershipEvidence,
) -> Result<ThreadOwner, RegistryError> {
    evidence.validate_hold(hold)?;
    let socket = super::host::socket_path(root, &hold.owner_id)?;
    registry.update_as(super::DbOperation::Claim, &hold.thread_id, |owner| {
        // ACK-loss retry reads back the exact receipt without clearing active
        // runs again (activation may already have begun).
        if owner.planned_claim.as_deref() == Some(hold)
            && owner.lease.owner_id == successor
            && hold.generation.checked_add(1) == Some(owner.lease.generation)
            && owner.state != super::OwnerState::Released
        {
            return Ok(());
        }
        owner.validate_planned_generation(hold)?;
        if endpoint_may_be_live(&socket) {
            return Err(super::OwnershipError::OwnerResponsive);
        }
        owner.claim_planned(hold, successor, now_ms)?;
        owner.settings = settings.clone();
        owner.lease.expires_at = now_ms.saturating_add(settings.lease_ms.get());
        Ok(())
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
    request_with_timeout(path, request, Duration::from_millis(500))
}

/// Test seam for ownership assertions that do not measure IPC latency.
/// Uses one connection with no retry: a timed-out request may have committed.
#[doc(hidden)]
pub fn request_with_timeout(
    path: &Path,
    request: &Request,
    timeout: Duration,
) -> Result<Response, IpcError> {
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    write_frame(&mut stream, request)?;
    read_frame(&mut stream)
}

/// Event-driven test request: wait for the response frame, not a test deadline.
#[cfg(test)]
pub(super) fn request_without_timeout(
    path: &Path,
    request: &Request,
) -> Result<Response, IpcError> {
    let mut stream = UnixStream::connect(path)?;
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
    serve_connection_inner(stream, registry, now_ms, None, &|_, _, _| {})
}

pub fn serve_connection_with_bus(
    stream: &mut UnixStream,
    registry: &mut Registry,
    now_ms: u64,
    bus: &EventBus,
) -> Result<(), IpcError> {
    serve_connection_inner(stream, registry, now_ms, Some(bus), &|_, _, _| {})
}

/// Like [`serve_connection_with_bus`], calling `released(thread, previous lease, current owner)`
/// when a request releases or hands off a thread, before the requester can
/// observe the transition.
pub(super) fn serve_connection_releasing(
    stream: &mut UnixStream,
    registry: &mut Registry,
    now_ms: u64,
    bus: &EventBus,
    released: &dyn Fn(&str, &Lease, &ThreadOwner),
) -> Result<(), IpcError> {
    serve_connection_inner(stream, registry, now_ms, Some(bus), released)
}

fn serve_connection_inner(
    stream: &mut UnixStream,
    registry: &mut Registry,
    now_ms: u64,
    bus: Option<&EventBus>,
    released: &dyn Fn(&str, &Lease, &ThreadOwner),
) -> Result<(), IpcError> {
    timeouts(stream)?;
    let request: Request = read_frame(stream)?;
    let previous_owner = match &request {
        Request::Quiesce { token, .. } | Request::Handoff { token, .. } => Some(token.clone()),
        Request::Attach { .. } | Request::Heartbeat { .. } => None,
    };
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
            if let Some(previous) = &previous_owner
                && (owner.state == super::OwnerState::Released
                    || matches!(action, Some(OwnershipAction::Handoff)))
            {
                released(&owner.thread_id, previous, &owner);
            }
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
