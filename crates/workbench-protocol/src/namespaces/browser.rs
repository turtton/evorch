//! Offline renderer API. Results are private queries, never receipt/event data.
//! DTOs intentionally do not depend on renderer-ipc.
use crate::{
    Capability,
    handshake::WireLimits,
    ids::ClientId,
    validation::{ValidationError, check_count, check_line},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::artifacts::ArtifactRef;

pub mod data;
pub mod observation;

pub const COMMAND_OPERATIONS: &[&str] = &[
    "create_renderer_session",
    "resume_renderer_session",
    "close_renderer_session",
    "resize_renderer_tab",
    "set_renderer_streaming",
    "set_renderer_frame_rate",
    "renderer_cdp",
    "settle_renderer_tab",
    "take_renderer_notices",
    "capture_renderer_tab",
    "close_renderer_tab",
    "grant_renderer_observer",
    "subscribe_renderer_frames",
    "unsubscribe_renderer_frames",
    "bind_renderer_data",
    "subscribe_agent_frames",
    "unsubscribe_agent_frames",
    "bind_agent_data",
];
pub const QUERY_OPERATIONS: &[&str] = &[
    "get_renderer_status",
    "list_renderer_tabs",
    "get_renderer_tab",
    "get_renderer_operation",
    "list_agent_runs",
    "get_agent_observation",
    "get_agent_subscription",
];
pub const EVENT_TYPES: &[&str] = &[
    "renderer_changed",
    "tab_changed",
    "operation_changed",
    "agent_observation_changed",
];
pub const NESTED_COMMANDS: &[super::NestedOperation] = &[];
/// Bound on the complete serialized JSON result of a public CDP operation.
pub const MAX_CDP_RESULT_BYTES: usize = 256 * 1024;
pub const MAX_RENDERER_TABS: usize = 4;
pub const MAX_RENDERER_OPERATIONS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RendererId(String);
impl RendererId {
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        if value.len() != 32
            || !value
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(invalid("renderer.id", "expected 32 lowercase hex digits"));
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for RendererId {
    type Error = ValidationError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}
impl From<RendererId> for String {
    fn from(value: RendererId) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererSessionRef {
    pub core_epoch: u64,
    pub id: RendererId,
}
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererTabRef {
    pub core_epoch: u64,
    pub generation: u64,
    pub id: RendererId,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererViewport {
    pub width: u32,
    pub height: u32,
}
impl RendererViewport {
    pub fn validate(self) -> Result<(), ValidationError> {
        if self.width == 0 || self.height == 0 || self.width > 3840 || self.height > 2160 {
            return Err(invalid(
                "renderer.viewport",
                "dimensions must be in 1x1..=3840x2160",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum BrowserCommand {
    SubscribeAgentFrames {
        scope: observation::ObservationScope,
    },
    UnsubscribeAgentFrames {
        subscription: observation::ObservationId,
    },
    BindAgentData {
        subscription: observation::ObservationId,
        token: observation::ObservationId,
        channel_id: RendererId,
    },
    CreateRendererSession {},
    ResumeRendererSession {
        session: RendererSessionRef,
        token: RendererId,
    },
    CloseRendererSession {
        session: RendererSessionRef,
    },
    ResizeRendererTab {
        tab: RendererTabRef,
        viewport: RendererViewport,
    },
    SetRendererStreaming {
        tab: RendererTabRef,
        enabled: bool,
    },
    SetRendererFrameRate {
        tab: RendererTabRef,
        fps: u8,
    },
    RendererCdp {
        tab: RendererTabRef,
        method: String,
        params: Value,
    },
    SettleRendererTab {
        tab: RendererTabRef,
    },
    TakeRendererNotices {
        tab: RendererTabRef,
    },
    CaptureRendererTab {
        tab: RendererTabRef,
    },
    CloseRendererTab {
        tab: RendererTabRef,
    },
    GrantRendererObserver {
        tab: RendererTabRef,
        client: ClientId,
        metadata: bool,
        frames: bool,
    },
    SubscribeRendererFrames {
        tab: RendererTabRef,
    },
    UnsubscribeRendererFrames {
        subscription: RendererId,
    },
    /// Binds a data connection (opened with a data hello) to a frame
    /// subscription or a Capture result. Only this control connection can
    /// grant the binding: it is where the caller's provenance is known.
    BindRendererData {
        channel_id: RendererId,
        source: RendererDataSource,
    },
}

/// What a bound data connection receives. The ids are the ones a caller got
/// from `Subscription` and `Capture` outputs on its own control connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RendererDataSource {
    /// Raw frames of the subscribed tab. The token is consumed once.
    Frames {
        subscription: RendererId,
        token: RendererId,
    },
    /// The PNG of one Capture operation of this caller.
    Capture {
        operation_id: RendererId,
        capture_id: RendererId,
    },
}
impl BrowserCommand {
    pub fn required_capability(&self) -> Capability {
        Capability::Commands
    }
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::ResizeRendererTab { viewport, .. } => viewport.validate(),
            Self::SetRendererFrameRate { fps, .. } if *fps == 0 || *fps > 60 => {
                Err(invalid("renderer.fps", "must be in 1..=60"))
            }
            Self::RendererCdp { method, params, .. } => {
                check_line("renderer.cdp.method", method, 128, false)?;
                if method == "Page.captureScreenshot" {
                    return Err(invalid(
                        "renderer.cdp.method",
                        "use capture_renderer_tab for pixel results",
                    ));
                }
                json_size(params, MAX_CDP_RESULT_BYTES)
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum BrowserResult {
    AgentObservation(observation::ObservationResult),
    Accepted { operation_id: RendererId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum BrowserQuery {
    ListAgentRuns {
        project: crate::ids::ProjectId,
        thread: crate::ids::ThreadId,
    },
    GetAgentObservation {
        scope: observation::ObservationScope,
    },
    GetAgentSubscription {
        subscription: observation::ObservationId,
    },
    GetRendererStatus {},
    ListRendererTabs {
        offset: u32,
        limit: u32,
    },
    GetRendererTab {
        tab: RendererTabRef,
    },
    GetRendererOperation {
        operation_id: RendererId,
    },
}
impl BrowserQuery {
    pub fn required_capability(&self) -> Capability {
        Capability::Queries
    }
    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::ListRendererTabs { limit, .. } => {
                check_count("renderer.limit", *limit as usize, MAX_RENDERER_TABS)
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererPhase {
    Stopped,
    Starting,
    Ready,
    Idle,
    Stopping,
    Failed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererStatus {
    pub core_epoch: u64,
    pub generation: u64,
    pub phase: RendererPhase,
    pub tabs_used: u32,
    pub tabs_limit: u32,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererOwnerKind {
    ClientPreview,
    Run,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererTab {
    pub reference: RendererTabRef,
    pub session: RendererSessionRef,
    pub owner: RendererOwnerKind,
    pub artifact: ArtifactRef,
    pub viewport: RendererViewport,
    pub fps: u8,
    pub streaming: bool,
}

/// Structured failures contain no helper-provided arbitrary text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererFailure {
    Unavailable,
    Busy,
    StaleGeneration,
    FrameTooLarge,
    ResultTooLarge,
    InvalidRequest,
    TimedOut,
    Restart,
    Closed,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RendererOutput {
    Done,
    Session {
        session: RendererSessionRef,
        resume_token: RendererId,
    },
    Tab(Box<RendererTab>),
    Cdp {
        result: Value,
    },
    Notices {
        blocked: Vec<String>,
        dialogs: Vec<String>,
    },
    Capture {
        capture_id: RendererId,
        byte_len: u64,
    },
    Subscription {
        subscription: RendererId,
        token: RendererId,
        tab: RendererTabRef,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RendererOperationState {
    Running,
    Completed { output: RendererOutput },
    Failed { reason: RendererFailure },
    Cancelled { reason: RendererFailure },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererOperation {
    pub operation_id: RendererId,
    pub state: RendererOperationState,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum BrowserReply {
    AgentObservation(observation::ObservationReply),
    Status(RendererStatus),
    Tabs { tabs: Vec<RendererTab>, more: bool },
    Tab(Box<RendererTab>),
    Operation(RendererOperation),
}
impl BrowserReply {
    pub fn validate(&self, limits: &WireLimits) -> Result<(), ValidationError> {
        if let Self::AgentObservation(reply) = self {
            reply.validate()?;
        }
        if let Self::Tabs { tabs, .. } = self {
            check_count("renderer.tabs", tabs.len(), MAX_RENDERER_TABS)?;
        }
        if let Self::Operation(RendererOperation {
            state:
                RendererOperationState::Completed {
                    output: RendererOutput::Cdp { result },
                },
            ..
        }) = self
        {
            json_size(result, MAX_CDP_RESULT_BYTES)?;
        }
        validate_wire_size(self, limits)
    }
}

/// Only opaque ids: safe in global subscriptions, replay and snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum BrowserEvent {
    AgentObservationChanged(observation::ObservationEvent),
    RendererChanged { id: RendererId },
    TabChanged { id: RendererId },
    OperationChanged { id: RendererId },
}
impl BrowserEvent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        Ok(())
    }
}

fn invalid(field: &'static str, reason: &'static str) -> ValidationError {
    ValidationError::OutOfRange { field, reason }
}
fn json_size(value: &impl Serialize, max: usize) -> Result<(), ValidationError> {
    // The incoming frame is already bounded; this also checks constructed replies.
    let bytes =
        serde_json::to_vec(value).map_err(|_| invalid("renderer.result", "not serializable"))?;
    check_count("renderer.result", bytes.len(), max)
}
pub(super) fn validate_wire_size(
    value: &impl Serialize,
    limits: &WireLimits,
) -> Result<(), ValidationError> {
    // Leave room for QueryReply and ServerFrame envelopes.
    json_size(
        value,
        (limits.max_frame_bytes as usize).saturating_sub(1024),
    )
}

#[cfg(test)]
mod tests;
