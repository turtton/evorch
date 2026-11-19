//! Local native-human observation. These are not offline-renderer identities.
//! Text limits are resource bounds, not redaction. Never publish this metadata
//! to Web, receipts, replay, or the global event stream.
use crate::{
    ids::{ProjectId, RunId, ThreadId},
    validation::{ValidationError, check_count},
};
use serde::{Deserialize, Serialize};

pub const MAX_RUNS: usize = 32;
pub const MAX_TABS: usize = 32;
pub const MAX_ACTIVITY: usize = 32;
pub const MAX_TEXT_BYTES: usize = 2048;
pub const MAX_REPLY_BYTES: usize = 64 * 1024;
pub const MAX_WIDTH: u32 = 1280;
pub const MAX_HEIGHT: u32 = 800;
pub const MAX_FRAME_BYTES: u64 = MAX_WIDTH as u64 * MAX_HEIGHT as u64 * 4;
pub const MAX_STOPPED_IMAGES: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ObservationId(String);
impl ObservationId {
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        if value.len() != 32
            || !value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(super::invalid(
                "observation.id",
                "expected 32 lowercase hex digits",
            ));
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for ObservationId {
    type Error = ValidationError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}
impl From<ObservationId> for String {
    fn from(value: ObservationId) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationTarget {
    pub project: ProjectId,
    pub thread: ThreadId,
    pub run: RunId,
}

/// All fields must match; none may be inferred from a durable resume intent.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationScope {
    pub project: ProjectId,
    pub thread: ThreadId,
    pub run: RunId,
    pub incarnation: RunId,
    pub core_epoch: u64,
    pub browser_generation: u64,
    pub page_generation: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationState {
    Starting,
    Live,
    Stopped,
    Revoked,
    Unavailable,
    Capacity,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationTab {
    pub id: ObservationId,
    pub title: String,
    pub url: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ObservationActivity {
    Action {
        label: String,
        error: Option<String>,
    },
    Blocked(String),
    Dialog(String),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationSnapshot {
    pub scope: ObservationScope,
    pub revision: u64,
    pub state: ObservationState,
    pub tabs: Vec<ObservationTab>,
    pub active_tab: Option<ObservationId>,
    pub activity: Vec<ObservationActivity>,
}
impl ObservationSnapshot {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_count("observation.tabs", self.tabs.len(), MAX_TABS)?;
        check_count("observation.activity", self.activity.len(), MAX_ACTIVITY)?;
        let text = |s: &str| check_count("observation.text", s.len(), MAX_TEXT_BYTES);
        let mut ids = std::collections::HashSet::new();
        for tab in &self.tabs {
            text(&tab.title)?;
            text(&tab.url)?;
            if !ids.insert(&tab.id) {
                return Err(super::invalid("observation.tabs", "duplicate tab"));
            }
        }
        if self.active_tab.as_ref().is_some_and(|id| !ids.contains(id)) {
            return Err(super::invalid("observation.active_tab", "unknown tab"));
        }
        for entry in &self.activity {
            match entry {
                ObservationActivity::Action { label, error } => {
                    text(label)?;
                    if let Some(e) = error {
                        text(e)?;
                    }
                }
                ObservationActivity::Blocked(s) | ObservationActivity::Dialog(s) => text(s)?,
            }
        }
        super::json_size(self, MAX_REPLY_BYTES)
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ObservationReply {
    Runs {
        scopes: Vec<ObservationScope>,
    },
    Snapshot(Box<ObservationSnapshot>),
    Subscription {
        subscription: ObservationId,
        scope: ObservationScope,
        token: Option<ObservationId>,
        state: ObservationState,
    },
    State(ObservationState),
}
impl ObservationReply {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::Runs { scopes } => check_count("observation.runs", scopes.len(), MAX_RUNS)?,
            Self::Snapshot(s) => s.validate()?,
            Self::State(_) | Self::Subscription { .. } => (),
        }
        super::json_size(self, MAX_REPLY_BYTES)
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ObservationResult {
    /// Registration only. Live starts after the dedicated data channel binds.
    Subscribed {
        subscription: ObservationId,
        state: ObservationState,
    },
    Bound {},
    Unsubscribed {},
    State(ObservationState),
}
/// Subscriber-directed invalidation only: no URL, text, pixels or bind token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationEvent {
    pub subscription: ObservationId,
    pub revision: u64,
    pub state: ObservationState,
}

pub mod data;

#[cfg(test)]
mod tests {
    use super::data::AgentFrameStart;
    use super::*;
    fn start() -> AgentFrameStart {
        AgentFrameStart {
            scope: ObservationScope {
                project: ProjectId::new("p").unwrap(),
                thread: ThreadId::new("t").unwrap(),
                run: RunId::new("r").unwrap(),
                incarnation: RunId::new("i").unwrap(),
                core_epoch: 7,
                browser_generation: 1,
                page_generation: 2,
            },
            tab: ObservationId::new("a".repeat(32)).unwrap(),
            sequence: 1,
            width: 1280,
            height: 800,
            stride: 5120,
            total_bytes: 4_096_000,
        }
    }
    #[test]
    fn rgba_record_has_independent_kind_exact_bounds_and_identity() {
        let start = start();
        let bytes = start.encode().unwrap();
        let prefix =
            crate::namespaces::browser::data::Prefix::parse(bytes[..16].try_into().unwrap())
                .unwrap();
        assert_eq!(
            prefix.kind,
            crate::namespaces::browser::data::Kind::AgentFrameStart
        );
        assert_eq!(AgentFrameStart::decode(&bytes[16..]).unwrap(), start);
        assert!(crate::namespaces::browser::data::FrameStart::decode(&bytes[16..]).is_err());
        for (w, h) in [
            (0, 800),
            (1280, 0),
            (1281, 800),
            (1280, 801),
            (u32::MAX, u32::MAX),
        ] {
            let mut bad = start.clone();
            bad.width = w;
            bad.height = h;
            assert!(bad.validate().is_err());
        }
        let mut bad = start.clone();
        bad.stride += 1;
        assert!(bad.validate().is_err());
        let mut bad = start;
        bad.total_bytes -= 1;
        assert!(bad.validate().is_err());
        let mut malformed = bytes;
        malformed[16] = 1;
        assert!(AgentFrameStart::decode(&malformed[16..]).is_err());
    }
    #[test]
    fn maximal_escaped_scope_fits_the_bounded_header() {
        let mut start = start();
        let escaped = "\\".repeat(crate::limits::MAX_ID_BYTES);
        start.scope.project = ProjectId::new(escaped.clone()).unwrap();
        start.scope.thread = ThreadId::new(escaped.clone()).unwrap();
        start.scope.run = RunId::new(escaped.clone()).unwrap();
        start.scope.incarnation = RunId::new(escaped).unwrap();
        let bytes = start.encode().unwrap();
        assert_eq!(AgentFrameStart::decode(&bytes[16..]).unwrap(), start);
    }
    #[test]
    fn activity_and_text_are_byte_bounded_and_the_whole_reply_is_bounded() {
        let start = start();
        let mut snapshot = ObservationSnapshot {
            scope: start.scope,
            revision: 1,
            state: ObservationState::Live,
            tabs: vec![],
            active_tab: None,
            activity: vec![],
        };
        snapshot.activity =
            vec![ObservationActivity::Dialog("a".repeat(MAX_TEXT_BYTES)); MAX_ACTIVITY];
        assert!(
            snapshot.validate().is_err(),
            "per-string bounds alone are insufficient"
        );
        snapshot.activity = vec![ObservationActivity::Blocked("界".repeat(683))];
        assert!(
            snapshot.validate().is_err(),
            "2049 UTF-8 bytes, not 683 characters"
        );
        snapshot.activity = vec![ObservationActivity::Dialog("ok".into()); MAX_ACTIVITY];
        assert!(snapshot.validate().is_ok());
        snapshot
            .activity
            .push(ObservationActivity::Dialog("one too many".into()));
        assert!(snapshot.validate().is_err());
    }
}
