//! Request, reply and server-push frames.
//!
//! Wire shape of a request:
//! `{"request_id":"..","session_id":"..","kind":"command","payload":{"type":"send_chat","payload":{..}}}`.
//! A reply is `{"request_id":"..","status":"success"|"error","payload":..}`.

use crate::commands::{Command, CommandReply};
use crate::errors::{ErrorCode, RetryAdvice, WorkbenchError};
use crate::events::{EventEnvelope, Subscribe, SubscriptionStarted};
use crate::handshake::{Capability, WireLimits};
use crate::ids::{ClientId, RequestId, RunId, SessionId, SubscriptionId};
use crate::queries::{Query, QueryReply};
use crate::validation::ValidationError;
use crate::wire::Nonce;
use serde::{Deserialize, Serialize};

/// Who performed an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    Human,
    Agent,
}

/// Which interface the action came through. No device or network detail is
/// recorded (ADR 0045).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    Desktop,
    Web,
    Cli,
    None,
}

/// Who answered a question, decided an approval or sent an input.
///
/// **The core's ingress attaches this; clients never send it.** It is derived
/// from the authenticated transport and the session, so a client cannot claim
/// to be an agent or another client. No request type has a provenance field,
/// and the request envelope, every command and query payload (including the
/// fields of struct-like enum variants) reject unknown fields, so a client
/// that includes one is refused instead of ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub actor: Actor,
    pub surface: Surface,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<ClientId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_run_id: Option<RunId>,
}

impl Provenance {
    pub fn human(surface: Surface, client_id: ClientId) -> Self {
        Self {
            actor: Actor::Human,
            surface,
            client_id: Some(client_id),
            agent_run_id: None,
        }
    }

    pub fn agent(run: RunId) -> Self {
        Self {
            actor: Actor::Agent,
            surface: Surface::None,
            client_id: None,
            agent_run_id: Some(run),
        }
    }

    /// An agent names its run and has no interface; a human has an interface and no run.
    pub fn validate(&self) -> Result<(), ValidationError> {
        match (self.actor, self.surface, &self.agent_run_id) {
            (Actor::Agent, Surface::None, Some(_)) => Ok(()),
            (Actor::Agent, Surface::None, None) => Err(ValidationError::Inconsistent {
                field: "provenance.agent_run_id",
                reason: "an agent actor must name its run",
            }),
            (Actor::Agent, _, _) => Err(ValidationError::Inconsistent {
                field: "provenance.surface",
                reason: "an agent actor has no interface",
            }),
            (Actor::Human, _, Some(_)) => Err(ValidationError::Inconsistent {
                field: "provenance.agent_run_id",
                reason: "a human actor has no agent run",
            }),
            (Actor::Human, Surface::None, None) => Err(ValidationError::Inconsistent {
                field: "provenance.surface",
                reason: "a human actor acts through an interface",
            }),
            (Actor::Human, _, None) => Ok(()),
        }
    }
}

/// Connection-level requests that are neither commands nor queries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Control {
    Subscribe(Subscribe),
    Unsubscribe { subscription_id: SubscriptionId },
    Ping { nonce: Nonce },
    Pong { nonce: Nonce },
}

impl Control {
    /// Wire names of every variant, for rejecting unknown ones precisely.
    pub const TYPES: [&'static str; 4] = ["subscribe", "unsubscribe", "ping", "pong"];
}

/// What a request asks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum RequestBody {
    Command(Command),
    Query(Query),
    Control(Control),
}

impl RequestBody {
    /// The capability the core must have offered, if the request needs one.
    pub fn required_capability(&self) -> Option<Capability> {
        match self {
            Self::Command(command) => Some(command.required_capability()),
            Self::Query(query) => Some(query.required_capability()),
            Self::Control(Control::Subscribe(_) | Control::Unsubscribe { .. }) => {
                Some(Capability::SnapshotEvents)
            }
            Self::Control(Control::Ping { .. } | Control::Pong { .. }) => None,
        }
    }

    pub fn validate(&self, limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::Command(command) => command.validate(),
            Self::Query(query) => query.validate(limits),
            Self::Control(Control::Subscribe(subscribe)) => subscribe.validate(),
            Self::Control(_) => Ok(()),
        }
    }
}

/// Client to core, after the handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestEnvelope {
    /// Unique within the session; the reply and any receipt are keyed by it.
    pub request_id: RequestId,
    pub session_id: SessionId,
    #[serde(flatten)]
    pub body: RequestBody,
}

/// A request the core cannot act on, with the best correlation available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestRejection {
    pub request_id: Option<RequestId>,
    pub error: WorkbenchError,
}

/// Decode and validate one request frame.
///
/// An unknown command, query or control type is reported as
/// [`ErrorCode::UnsupportedCapability`] (a newer client talking to an older
/// core); every other defect is [`ErrorCode::InvalidRequest`] or
/// [`ErrorCode::LimitExceeded`]. Diagnostics must not quote input tokens: a
/// transport can reject the frame before the backend output guard is reached.
pub fn decode_request(
    bytes: &[u8],
    limits: &WireLimits,
) -> Result<RequestEnvelope, RequestRejection> {
    let probe: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|error| RequestRejection {
            request_id: None,
            // Parser diagnostics may quote attacker-controlled tokens. Only
            // structural position information is safe before backend protection.
            error: WorkbenchError::invalid_request(format!(
                "frame is not valid JSON at line {}, column {}",
                error.line(),
                error.column()
            )),
        })?;
    let request_id = probe
        .get("request_id")
        .and_then(serde_json::Value::as_str)
        .and_then(|id| RequestId::new(id).ok());
    let rejection = |error: WorkbenchError| RequestRejection {
        error: match &request_id {
            Some(id) => error.for_request(id.clone()),
            None => error,
        },
        request_id: request_id.clone(),
    };

    // The envelope is a closed shape too: a stray top-level field (a client
    // trying to supply provenance, say) is refused, not ignored.
    if let Some(object) = probe.as_object()
        && object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "request_id" | "session_id" | "kind" | "payload"
            )
        })
    {
        return Err(rejection(WorkbenchError::invalid_request(
            "unexpected request field",
        )));
    }

    let envelope: RequestEnvelope = match serde_json::from_value(probe.clone()) {
        Ok(envelope) => envelope,
        Err(_) => {
            let known: Option<&[&str]> = match probe.get("kind").and_then(|k| k.as_str()) {
                Some("command") => Some(&Command::TYPES),
                Some("query") => Some(&Query::TYPES),
                Some("control") => Some(&Control::TYPES),
                _ => None,
            };
            let ty = probe
                .pointer("/payload/type")
                .and_then(serde_json::Value::as_str);
            if let (Some(known), Some(ty)) = (known, ty)
                && !known.contains(&ty)
            {
                return Err(rejection(WorkbenchError::new(
                    ErrorCode::UnsupportedCapability,
                    "unknown request type",
                    RetryAdvice::Never,
                )));
            }
            return Err(rejection(WorkbenchError::invalid_request(
                "malformed request",
            )));
        }
    };
    envelope
        .body
        .validate(limits)
        .map_err(|error| rejection(error.into()))?;
    Ok(envelope)
}

/// What a successful request returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum ReplyPayload {
    Command(CommandReply),
    Query(QueryReply),
    SubscriptionStarted(SubscriptionStarted),
    Unsubscribed { subscription_id: SubscriptionId },
    Pong { nonce: Nonce },
    Ack,
}

/// Success or failure of one request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "payload", rename_all = "snake_case")]
pub enum ReplyResult {
    Success(ReplyPayload),
    Error(WorkbenchError),
}

/// Core to client: the answer to one request. Replies may arrive in any order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyEnvelope {
    pub request_id: RequestId,
    #[serde(flatten)]
    pub result: ReplyResult,
}

/// Everything the core sends after the handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum ServerFrame {
    Reply(ReplyEnvelope),
    Event(Box<EventEnvelope>),
}

impl ReplyPayload {
    /// Bound a success payload received from a peer.
    pub fn validate(&self, limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::Query(reply) => reply.validate(limits),
            Self::SubscriptionStarted(started) => started.validate(),
            Self::Command(_) | Self::Unsubscribed { .. } | Self::Pong { .. } | Self::Ack => Ok(()),
        }
    }
}

impl ServerFrame {
    /// Check everything a peer-supplied frame carries against the protocol and
    /// negotiated limits. Deserialization only checks shape; a receiver calls
    /// this before acting on a frame.
    pub fn validate(&self, limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::Reply(reply) => match &reply.result {
                ReplyResult::Success(payload) => payload.validate(limits),
                ReplyResult::Error(error) => error.validate(),
            },
            Self::Event(event) => event.validate(),
        }
    }
}
