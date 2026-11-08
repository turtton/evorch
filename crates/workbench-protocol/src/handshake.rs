//! The first frames on a connection: version and capability negotiation.
//!
//! A client sends [`ClientHello`]; the core answers with a [`HandshakeReply`].
//! Nothing else may be sent first. [`Capability`] names are declarations of
//! what the protocol can express; whether a given core implements one is
//! reported by that core in [`ServerHello::capabilities`], never assumed.

use crate::ids::{CapabilityName, ClientId, CoreIdentity};
use crate::limits::{
    DEFAULT_BODY_CHUNK_BYTES, DEFAULT_FRAME_BYTES, HARD_MAX_FRAME_BYTES, MAX_NAME_BYTES,
};
use crate::validation::{ValidationError, check_count, check_line};
use crate::wire::InstanceEpoch;
use serde::{Deserialize, Serialize};

/// Protocol version; a plain integer that changes only on incompatible wire changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProtocolVersion(pub u32);

impl ProtocolVersion {
    /// The only version this crate speaks.
    pub const CURRENT: Self = Self(1);
}

/// A feature a core may offer. Definitions only: the presence of a variant
/// here says nothing about any core implementing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Capability {
    #[serde(rename = "commands.v1")]
    Commands,
    #[serde(rename = "queries.v1")]
    Queries,
    #[serde(rename = "snapshot_events.v1")]
    SnapshotEvents,
    #[serde(rename = "input_receipts.v1")]
    InputReceipts,
    #[serde(rename = "approvals.v1")]
    Approvals,
    #[serde(rename = "questions.v1")]
    Questions,
    #[serde(rename = "committed_bodies.v1")]
    CommittedBodies,
    #[serde(rename = "safe_restart.v1")]
    SafeRestart,
}

impl Capability {
    /// Every capability this crate can name, in canonical order.
    pub const ALL: [Capability; 8] = [
        Self::Commands,
        Self::Queries,
        Self::SnapshotEvents,
        Self::InputReceipts,
        Self::Approvals,
        Self::Questions,
        Self::CommittedBodies,
        Self::SafeRestart,
    ];

    /// The wire spelling.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Commands => "commands.v1",
            Self::Queries => "queries.v1",
            Self::SnapshotEvents => "snapshot_events.v1",
            Self::InputReceipts => "input_receipts.v1",
            Self::Approvals => "approvals.v1",
            Self::Questions => "questions.v1",
            Self::CommittedBodies => "committed_bodies.v1",
            Self::SafeRestart => "safe_restart.v1",
        }
    }

    /// Resolve a wire spelling; `None` for names this crate does not know.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == name)
    }
}

impl From<Capability> for CapabilityName {
    fn from(capability: Capability) -> Self {
        CapabilityName::new(capability.name()).expect("capability names are valid ids")
    }
}

/// Which program and version a peer is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildInfo {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

impl BuildInfo {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_line("build.name", &self.name, MAX_NAME_BYTES, false)?;
        check_line("build.version", &self.version, MAX_NAME_BYTES, false)?;
        if let Some(revision) = &self.revision {
            check_line("build.revision", revision, MAX_NAME_BYTES, false)?;
        }
        Ok(())
    }
}

/// First frame, client to core.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientHello {
    pub supported_versions: Vec<ProtocolVersion>,
    /// The connection is useless without these; the core rejects if it lacks one.
    pub required_capabilities: Vec<CapabilityName>,
    /// Used when present; unknown names are ignored.
    pub optional_capabilities: Vec<CapabilityName>,
    pub client_build: BuildInfo,
    pub client_instance_id: ClientId,
    /// Refuse to talk to any other core.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired_core: Option<CoreIdentity>,
}

const MAX_HELLO_VERSIONS: usize = 16;
const MAX_HELLO_CAPABILITIES: usize = 32;

impl ClientHello {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.supported_versions.is_empty() {
            return Err(ValidationError::Empty {
                field: "supported_versions",
            });
        }
        check_count(
            "supported_versions",
            self.supported_versions.len(),
            MAX_HELLO_VERSIONS,
        )?;
        check_count(
            "required_capabilities",
            self.required_capabilities.len(),
            MAX_HELLO_CAPABILITIES,
        )?;
        check_count(
            "optional_capabilities",
            self.optional_capabilities.len(),
            MAX_HELLO_CAPABILITIES,
        )?;
        self.client_build.validate()
    }
}

/// Core lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoreState {
    Starting,
    Ready,
    /// Finishing in-flight work before it stops accepting new work.
    Quiescing,
    /// Refusing new work while shutting down.
    Draining,
    Restarting,
    Failed,
}

/// Why a core stays alive when no client is connected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionReason {
    ActiveTurn,
    ToolExecution,
    WaitingLane,
    BackgroundShellJob,
    PendingApproval,
    PendingQuestion,
    ConnectedClients,
    /// Web delivery is enabled even when no browser is connected.
    WebServing,
}

/// How much work is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivityCounts {
    pub turns: u32,
    pub tool_calls: u32,
    pub waiting_lanes: u32,
    pub shell_jobs: u32,
    pub clients: u32,
}

/// What a core is doing and why it stays up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoreStatus {
    pub state: CoreState,
    pub retention_reasons: Vec<RetentionReason>,
    pub running: ActivityCounts,
}

/// Sizes both peers honour for the life of a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireLimits {
    /// Longest frame payload, in bytes.
    pub max_frame_bytes: u32,
    /// Longest body chunk, in bytes.
    pub max_body_chunk_bytes: u32,
    /// Requests a client may have unanswered at once.
    pub max_in_flight_requests: u32,
    /// Events queued per subscription before it is declared slow.
    pub subscription_queue_events: u32,
    /// Bytes queued per subscription before it is declared slow.
    pub subscription_queue_bytes: u32,
}

impl Default for WireLimits {
    fn default() -> Self {
        Self {
            max_frame_bytes: DEFAULT_FRAME_BYTES,
            max_body_chunk_bytes: DEFAULT_BODY_CHUNK_BYTES,
            max_in_flight_requests: 64,
            subscription_queue_events: 1024,
            subscription_queue_bytes: 4 * 1024 * 1024,
        }
    }
}

impl WireLimits {
    /// Reject limits a peer must not accept: zero, absurd, or inconsistent.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let out_of_range = |field, reason| ValidationError::OutOfRange { field, reason };
        if self.max_frame_bytes == 0 || self.max_frame_bytes > HARD_MAX_FRAME_BYTES {
            return Err(out_of_range("max_frame_bytes", "must be 1..=16 MiB"));
        }
        if self.max_body_chunk_bytes == 0
            || u64::from(self.max_body_chunk_bytes) * 6 + 4096 > u64::from(self.max_frame_bytes)
        {
            return Err(out_of_range(
                "max_body_chunk_bytes",
                "a chunk must fit a frame after JSON escaping",
            ));
        }
        if !(1..=1024).contains(&self.max_in_flight_requests) {
            return Err(out_of_range("max_in_flight_requests", "must be 1..=1024"));
        }
        if !(1..=65_536).contains(&self.subscription_queue_events) {
            return Err(out_of_range(
                "subscription_queue_events",
                "must be 1..=65536",
            ));
        }
        if self.subscription_queue_bytes < self.max_frame_bytes {
            return Err(out_of_range(
                "subscription_queue_bytes",
                "must hold at least one frame",
            ));
        }
        Ok(())
    }
}

/// Core's answer when it accepts a connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerHello {
    pub protocol_version: ProtocolVersion,
    pub capabilities: Vec<Capability>,
    pub server_build: BuildInfo,
    pub core: CoreIdentity,
    pub instance_epoch: InstanceEpoch,
    pub limits: WireLimits,
    pub status: CoreStatus,
}

impl ServerHello {
    pub fn has(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }
}

/// Why a core refused a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    UnsupportedVersion,
    MissingCapabilities,
    CoreMismatch,
    InvalidHello,
    CoreNotReady,
}

/// What the user or client should do about a rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecommendedAction {
    UpgradeClient,
    UpgradeCore,
    /// Restart the client; running work in the core is untouched.
    RestartClient,
    /// Restart the core at a safe point.
    RestartCore,
    RetryLater,
    None,
}

/// Core's answer when it refuses a connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandshakeRejected {
    pub reason: RejectReason,
    pub message: String,
    pub supported_versions: Vec<ProtocolVersion>,
    pub missing_capabilities: Vec<CapabilityName>,
    pub recommended_action: RecommendedAction,
}

/// The core's answer to a [`ClientHello`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum HandshakeReply {
    Accepted(ServerHello),
    Rejected(HandshakeRejected),
}

/// What a core is willing to speak, as input to [`negotiate`].
#[derive(Debug, Clone, Copy)]
pub struct ServerOffer<'a> {
    pub core: &'a CoreIdentity,
    pub versions: &'a [ProtocolVersion],
    pub capabilities: &'a [Capability],
}

/// The outcome of a successful negotiation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Negotiated {
    pub version: ProtocolVersion,
    /// Offered capabilities the client asked for, in canonical order.
    pub capabilities: Vec<Capability>,
}

/// Pure negotiation rule shared by every core implementation and test fake.
pub fn negotiate(
    hello: &ClientHello,
    offer: &ServerOffer<'_>,
) -> Result<Negotiated, HandshakeRejected> {
    let reject = |reason, message: String, missing, action| HandshakeRejected {
        reason,
        message,
        supported_versions: offer.versions.to_vec(),
        missing_capabilities: missing,
        recommended_action: action,
    };

    if let Err(error) = hello.validate() {
        return Err(reject(
            RejectReason::InvalidHello,
            error.to_string(),
            Vec::new(),
            RecommendedAction::UpgradeClient,
        ));
    }
    if let Some(desired) = &hello.desired_core
        && desired != offer.core
    {
        return Err(reject(
            RejectReason::CoreMismatch,
            "this core is not the one the client asked for".to_owned(),
            Vec::new(),
            RecommendedAction::None,
        ));
    }

    let Some(version) = hello
        .supported_versions
        .iter()
        .filter(|v| offer.versions.contains(v))
        .max()
        .copied()
    else {
        let client_max = hello.supported_versions.iter().max();
        let server_max = offer.versions.iter().max();
        let action = if client_max < server_max {
            RecommendedAction::UpgradeClient
        } else {
            RecommendedAction::UpgradeCore
        };
        return Err(reject(
            RejectReason::UnsupportedVersion,
            "no protocol version is supported by both sides".to_owned(),
            Vec::new(),
            action,
        ));
    };

    let offered = |name: &CapabilityName| {
        Capability::from_name(name.as_str()).filter(|c| offer.capabilities.contains(c))
    };
    let missing: Vec<CapabilityName> = hello
        .required_capabilities
        .iter()
        .filter(|name| offered(name).is_none())
        .cloned()
        .collect();
    if !missing.is_empty() {
        return Err(reject(
            RejectReason::MissingCapabilities,
            "the core lacks a capability the client requires".to_owned(),
            missing,
            RecommendedAction::UpgradeCore,
        ));
    }

    let wanted: Vec<Capability> = hello
        .required_capabilities
        .iter()
        .chain(&hello.optional_capabilities)
        .filter_map(offered)
        .collect();
    let capabilities = Capability::ALL
        .into_iter()
        .filter(|c| wanted.contains(c))
        .collect();
    Ok(Negotiated {
        version,
        capabilities,
    })
}

impl CoreStatus {
    /// Bound a status received from a peer.
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_count("retention_reasons", self.retention_reasons.len(), 16)
    }
}

const MAX_REJECTION_MESSAGE_BYTES: usize = 8 * 1024;

impl ServerHello {
    /// Check everything a peer-supplied hello carries before a connection is
    /// built on it: build identity, status and limits. The version, core
    /// identity and capabilities are checked against the client's request by
    /// the caller.
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.server_build.validate()?;
        self.status.validate()?;
        check_count(
            "capabilities",
            self.capabilities.len(),
            Capability::ALL.len(),
        )?;
        self.limits.validate()
    }
}

impl HandshakeRejected {
    /// Bound a rejection received from a peer.
    pub fn validate(&self) -> Result<(), ValidationError> {
        crate::validation::check_bytes(
            "message",
            &self.message,
            MAX_REJECTION_MESSAGE_BYTES,
            true,
        )?;
        check_count(
            "supported_versions",
            self.supported_versions.len(),
            MAX_HELLO_VERSIONS,
        )?;
        check_count(
            "missing_capabilities",
            self.missing_capabilities.len(),
            MAX_HELLO_CAPABILITIES,
        )
    }
}
