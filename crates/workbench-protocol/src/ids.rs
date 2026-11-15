//! String identifiers.
//!
//! Every id is a validated newtype: non-empty, at most
//! [`MAX_ID_BYTES`](crate::limits::MAX_ID_BYTES) bytes and free of control
//! characters. Deserialization applies the same check, so a value of one of
//! these types is always well formed.

use crate::limits::MAX_ID_BYTES;
use crate::validation::{ValidationError, check_line};
use std::fmt;

/// Check an id string.
pub fn validate_id(field: &'static str, value: &str) -> Result<(), ValidationError> {
    check_line(field, value, MAX_ID_BYTES, false)
}

macro_rules! wire_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash,
                 serde::Serialize, serde::Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Build an id, rejecting malformed values.
            pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
                let value = value.into();
                validate_id(stringify!($name), &value)?;
                Ok(Self(value))
            }

            /// The id as text.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = ValidationError;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

wire_id!(
    /// A project.
    ProjectId
);
wire_id!(
    /// A conversation thread.
    ThreadId
);
wire_id!(
    /// One agent run.
    RunId
);
wire_id!(
    /// A client process instance (one desktop window, browser tab or CLI call).
    ClientId
);
wire_id!(
    /// One connection between a client and the core.
    SessionId
);
wire_id!(
    /// A request, unique within its session. Replies and receipts are keyed by it.
    RequestId
);
wire_id!(
    /// A composer message across its delivery states.
    SubmissionId
);
wire_id!(
    /// A live event subscription.
    SubscriptionId
);
wire_id!(
    /// A tool approval request.
    ApprovalId
);
wire_id!(
    /// A question an agent asked.
    QuestionId
);
wire_id!(
    /// A message body stored apart from its message.
    BodyId
);
wire_id!(
    /// A user attachment uploaded before the command that references it.
    AttachmentId
);
wire_id!(
    /// A goal on a thread.
    GoalId
);
wire_id!(
    /// A snapshot a subscription starts from.
    SnapshotId
);
wire_id!(
    /// Which core a peer talks to; derived from the configuration directory.
    CoreIdentity
);
wire_id!(
    /// Opaque position in a paged result.
    PageCursor
);
wire_id!(
    /// A capability as a peer spelled it. Unknown names are representable so a
    /// peer can name what it requires without this crate knowing it.
    CapabilityName
);

wire_id!(
    /// A process-local restart preparation attempt (not durable commit evidence).
    RestartId
);
