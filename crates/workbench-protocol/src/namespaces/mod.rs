//! Typed protocol namespaces (ADR 0045, PR 08s).
//!
//! `Command`, `Query`, `QueryReply`, `CommandResult` and `UiEvent` are thin
//! routers: each variant names one namespace and wraps that namespace's own
//! type. A later PR adds operations by editing only its namespace module (and
//! the matching host adapter), never the shared enums or their dispatch.
//!
//! ## Operation table
//!
//! | namespace | commands | queries | events |
//! |---|---|---|---|
//! | `conversation` | `resolve_project`, `create_execution`, `create_goal`, `prepare_existing_thread`, `upload_image`, `discard_attachment`, `send_chat`, `continue_chat`, `compact_chat`, `deliver_follow_ups_next_turn`, `withdraw_follow_up` | `get_input_receipts`, `read_committed_history` | `input_receipt_changed`, `message_committed` |
//! | `workspace` | - | `list_projects`, `list_threads`, `get_thread` | `thread_changed` |
//! | `execution` | `stop_chat`, `stop_run`, `submit_goal`, `set_goal_review`, `set_goal_checks_paused`, `set_agent_web_tools_enabled` | `list_runs` | `run_changed` |
//! | `questions` | `answer_user_question` | `list_questions` | `question_changed` |
//! | `approvals` | `decide_tool_approval` | `list_approvals`, `get_approval_input` | `approval_changed` |
//! | `lanes` | `manage_lane` | `get_lane_management` | - |
//! | `lifecycle` | - | `get_core_status` | `core_status_changed` |
//! | `projection` | - | `get_command_receipt`, `read_body`, `read_snapshot` | `resync_required`, `subscription_closed` |
//!
//! ## Reserved namespaces
//!
//! `settings`, `auth`, `mcp`, `teams`, `diagnostics`, `memory`, `evaluation`,
//! `improvement`, `pull_requests`, `browser`, `artifacts` and `maintenance` are
//! reserved for later PRs. Every type in them is an empty (uninhabited) enum,
//! so no value of them exists: they cannot be constructed, answered or
//! advertised, and a peer naming one is refused as
//! [`ErrorCode::UnsupportedCapability`](crate::ErrorCode). There is no generic
//! `serde_json::Value` operation and no free-form method name anywhere.

/// An empty command enum: the namespace has no commands (yet).
macro_rules! no_commands {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
        pub enum $name {}
        impl $name {
            /// The capability a core must offer; never reached, the type is empty.
            pub fn required_capability(&self) -> crate::Capability {
                match *self {}
            }
            pub fn validate(&self) -> Result<(), crate::validation::ValidationError> {
                match *self {}
            }
        }
    };
}

macro_rules! no_results {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
        pub enum $name {}
    };
}

macro_rules! no_queries {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
        pub enum $name {}
        impl $name {
            pub fn required_capability(&self) -> crate::Capability {
                match *self {}
            }
            pub fn validate(
                &self,
                _limits: &crate::handshake::WireLimits,
            ) -> Result<(), crate::validation::ValidationError> {
                match *self {}
            }
        }
    };
}

macro_rules! no_replies {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
        pub enum $name {}
        impl $name {
            pub fn validate(
                &self,
                _limits: &crate::handshake::WireLimits,
            ) -> Result<(), crate::validation::ValidationError> {
                match *self {}
            }
        }
    };
}

macro_rules! no_events {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
        pub enum $name {}
        impl $name {
            pub fn validate(&self) -> Result<(), crate::validation::ValidationError> {
                match *self {}
            }
            pub fn ends_subscription(&self) -> bool {
                match *self {}
            }
        }
    };
}

/// Everything an unimplemented namespace consists of: five empty enums and
/// empty operation lists.
macro_rules! reserved_namespace {
    ($command:ident, $result:ident, $query:ident, $reply:ident, $event:ident) => {
        no_commands!($command);
        no_results!($result);
        no_queries!($query);
        no_replies!($reply);
        no_events!($event);
        /// Wire names of the namespace's commands (none).
        pub const COMMAND_OPERATIONS: &[&str] = &[];
        /// Wire names of the namespace's queries (none).
        pub const QUERY_OPERATIONS: &[&str] = &[];
        /// Wire names of the namespace's events (none).
        pub const EVENT_TYPES: &[&str] = &[];
    };
}

pub mod approvals;
pub mod artifacts;
pub mod auth;
pub mod browser;
pub mod conversation;
pub mod diagnostics;
pub mod evaluation;
pub mod execution;
pub mod improvement;
pub mod lanes;
pub mod lifecycle;
pub mod maintenance;
pub mod mcp;
pub mod memory;
pub mod projection;
pub mod pull_requests;
pub mod questions;
pub mod settings;
pub mod teams;
pub mod workspace;

/// How far a namespace is implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamespaceStatus {
    /// Has operations a core serves and capabilities it may advertise.
    Implemented,
    /// Only a typed placeholder; every request naming it is refused.
    Reserved,
}

/// One namespace as the wire names it, with its operations per request kind.
#[derive(Debug, Clone, Copy)]
pub struct NamespaceInfo {
    pub name: &'static str,
    pub status: NamespaceStatus,
    pub commands: &'static [&'static str],
    pub queries: &'static [&'static str],
    pub events: &'static [&'static str],
}

macro_rules! info {
    ($name:literal, $status:ident, $module:ident) => {
        NamespaceInfo {
            name: $name,
            status: NamespaceStatus::$status,
            commands: $module::COMMAND_OPERATIONS,
            queries: $module::QUERY_OPERATIONS,
            events: $module::EVENT_TYPES,
        }
    };
}

/// Every namespace, in the order `Command`, `Query` and `UiEvent` list them.
pub const NAMESPACES: [NamespaceInfo; 20] = [
    info!("conversation", Implemented, conversation),
    info!("workspace", Implemented, workspace),
    info!("execution", Implemented, execution),
    info!("questions", Implemented, questions),
    info!("approvals", Implemented, approvals),
    info!("lanes", Implemented, lanes),
    info!("lifecycle", Implemented, lifecycle),
    info!("projection", Implemented, projection),
    info!("settings", Reserved, settings),
    info!("auth", Reserved, auth),
    info!("mcp", Reserved, mcp),
    info!("teams", Reserved, teams),
    info!("diagnostics", Reserved, diagnostics),
    info!("memory", Reserved, memory),
    info!("evaluation", Reserved, evaluation),
    info!("improvement", Reserved, improvement),
    info!("pull_requests", Reserved, pull_requests),
    info!("browser", Reserved, browser),
    info!("artifacts", Reserved, artifacts),
    info!("maintenance", Reserved, maintenance),
];

/// Wire names of every namespace.
pub const NAMESPACE_NAMES: [&str; 20] = [
    "conversation",
    "workspace",
    "execution",
    "questions",
    "approvals",
    "lanes",
    "lifecycle",
    "projection",
    "settings",
    "auth",
    "mcp",
    "teams",
    "diagnostics",
    "memory",
    "evaluation",
    "improvement",
    "pull_requests",
    "browser",
    "artifacts",
    "maintenance",
];

/// The namespace named `name` on the wire.
pub fn namespace(name: &str) -> Option<&'static NamespaceInfo> {
    NAMESPACES.iter().find(|n| n.name == name)
}

/// What a request kind's payload names, for classifying a request that did
/// not decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classified {
    /// The namespace is not one this protocol names.
    UnknownNamespace,
    /// The namespace has no such operation (also: it has no operations at all).
    UnknownOperation,
    /// Namespace and operation exist; the payload is what is wrong.
    KnownOperation,
    /// The shape does not allow telling.
    Malformed,
}

/// Classify `payload` (`{"type": <namespace>, "payload": {"type": <operation>, ..}}`)
/// without reading anything but those two names.
pub fn classify(
    payload: &serde_json::Value,
    operations: impl Fn(&NamespaceInfo) -> &'static [&'static str],
) -> Classified {
    let Some(name) = payload.get("type").and_then(serde_json::Value::as_str) else {
        return Classified::Malformed;
    };
    let Some(info) = namespace(name) else {
        return Classified::UnknownNamespace;
    };
    let known = operations(info);
    if known.is_empty() {
        // Nothing in this namespace exists, whatever else was sent.
        return Classified::UnknownOperation;
    }
    match payload
        .pointer("/payload/type")
        .and_then(serde_json::Value::as_str)
    {
        Some(operation) if known.contains(&operation) => Classified::KnownOperation,
        Some(_) => Classified::UnknownOperation,
        None => Classified::Malformed,
    }
}

/// The error for a client that names a namespace or operation this core does
/// not have.
pub(crate) fn unsupported_operation() -> crate::WorkbenchError {
    crate::WorkbenchError::new(
        crate::ErrorCode::UnsupportedCapability,
        "unknown request type",
        crate::RetryAdvice::Never,
    )
}

pub use approvals::{
    ApprovalsCommand, ApprovalsEvent, ApprovalsQuery, ApprovalsReply, ApprovalsResult,
};
pub use conversation::{
    ConversationCommand, ConversationEvent, ConversationQuery, ConversationReply,
    ConversationResult,
};
pub use execution::{
    ExecutionCommand, ExecutionEvent, ExecutionQuery, ExecutionReply, ExecutionResult,
};
pub use lanes::{LanesCommand, LanesEvent, LanesQuery, LanesReply, LanesResult};
pub use lifecycle::{
    LifecycleCommand, LifecycleEvent, LifecycleQuery, LifecycleReply, LifecycleResult,
};
pub use projection::{
    ProjectionCommand, ProjectionEvent, ProjectionQuery, ProjectionReply, ProjectionResult,
};
pub use questions::{
    QuestionsCommand, QuestionsEvent, QuestionsQuery, QuestionsReply, QuestionsResult,
};
pub use workspace::{
    WorkspaceCommand, WorkspaceEvent, WorkspaceQuery, WorkspaceReply, WorkspaceResult,
};
