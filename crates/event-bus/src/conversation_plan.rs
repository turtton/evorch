//! The change event of a conversation's plan and its progress (types shared with storage).
//!
//! The plan itself is a tree of tasks kept as a database projection; the projection, not an
//! event, is the source of truth. A [`ConversationPlanEvent::Changed`] is the record of one
//! revision of that projection. It is written by the same database transaction that changed
//! the projection (and by nothing else), so the event log never holds a change the
//! projection lacks, nor the reverse. The recorder therefore never stores it again; it only
//! reaches live subscribers and followers through the stored row.
//!
//! The event is scoped to a thread, names the run that caused the change only for audit, and
//! never reaches a model: it is part of no transcript, history or prompt.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

/// Smallest task weight.
pub const PLAN_WEIGHT_MIN: u8 = 1;
/// Largest task weight.
pub const PLAN_WEIGHT_MAX: u8 = 100;
/// The weight of a task that names none.
pub const PLAN_WEIGHT_DEFAULT: u8 = 1;
/// The scale of every ratio of a [`PlanProgress`]: parts per million.
pub const PLAN_PPM_SCALE: u32 = 1_000_000;

/// The relative weight of a task among its siblings: an integer in
/// [`PLAN_WEIGHT_MIN`]..=[`PLAN_WEIGHT_MAX`]. A fractional or out-of-range number never
/// deserializes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct PlanWeight(u8);

/// A weight outside the allowed range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanWeightRange(pub u64);

impl fmt::Display for PlanWeightRange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "weight {} is outside {PLAN_WEIGHT_MIN}..={PLAN_WEIGHT_MAX}",
            self.0
        )
    }
}

impl std::error::Error for PlanWeightRange {}

impl PlanWeight {
    /// [`PLAN_WEIGHT_MIN`].
    pub const MIN: Self = Self(PLAN_WEIGHT_MIN);
    /// [`PLAN_WEIGHT_MAX`].
    pub const MAX: Self = Self(PLAN_WEIGHT_MAX);
    /// [`PLAN_WEIGHT_DEFAULT`].
    pub const DEFAULT: Self = Self(PLAN_WEIGHT_DEFAULT);

    /// # Errors
    /// `value` is outside the allowed range.
    pub fn new(value: u64) -> Result<Self, PlanWeightRange> {
        match u8::try_from(value) {
            Ok(weight) if (PLAN_WEIGHT_MIN..=PLAN_WEIGHT_MAX).contains(&weight) => Ok(Self(weight)),
            _ => Err(PlanWeightRange(value)),
        }
    }

    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl Default for PlanWeight {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl<'de> Deserialize<'de> for PlanWeight {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = u64::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// The state of a leaf task. A task with subtasks has none: its progress is derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanTaskStatus {
    Pending,
    InProgress,
    Completed,
    Blocked,
    /// The run that worked on the task ended without finishing it. Only the runtime sets it
    /// (the model's update is refused); the model may move the task out of it.
    Interrupted,
}

impl PlanTaskStatus {
    /// Every status, in display order.
    pub const ALL: [Self; 5] = [
        Self::Pending,
        Self::InProgress,
        Self::Completed,
        Self::Blocked,
        Self::Interrupted,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Blocked => "blocked",
            Self::Interrupted => "interrupted",
        }
    }

    /// # Errors
    /// `text` names no status.
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|status| status.as_str() == text)
    }
}

/// One number per [`PlanTaskStatus`]: a count of leaves, or a share in parts per million.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStatusValues {
    pub pending: u32,
    pub in_progress: u32,
    pub completed: u32,
    pub blocked: u32,
    pub interrupted: u32,
}

impl PlanStatusValues {
    #[must_use]
    pub const fn get(&self, status: PlanTaskStatus) -> u32 {
        match status {
            PlanTaskStatus::Pending => self.pending,
            PlanTaskStatus::InProgress => self.in_progress,
            PlanTaskStatus::Completed => self.completed,
            PlanTaskStatus::Blocked => self.blocked,
            PlanTaskStatus::Interrupted => self.interrupted,
        }
    }

    pub fn slot(&mut self, status: PlanTaskStatus) -> &mut u32 {
        match status {
            PlanTaskStatus::Pending => &mut self.pending,
            PlanTaskStatus::InProgress => &mut self.in_progress,
            PlanTaskStatus::Completed => &mut self.completed,
            PlanTaskStatus::Blocked => &mut self.blocked,
            PlanTaskStatus::Interrupted => &mut self.interrupted,
        }
    }
}

/// The aggregate of a plan: how many tasks it has and how much of its weight is in each state.
///
/// Weights are relative within one parent (the top level counts as the children of a virtual
/// parent), and a parent's share of its own parent is the weight-averaged share of its
/// children, recursively. Every share is an integer in parts per million
/// ([`PLAN_PPM_SCALE`]) rounded down at each parent, so `weighted_ppm.completed` is exactly
/// [`PLAN_PPM_SCALE`] if and only if every leaf is completed. An empty plan has no progress
/// ([`Self::completion_ppm`] is `None`), which is not 100%.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanProgress {
    /// Every task, parents included.
    pub node_count: u32,
    /// Tasks without subtasks.
    pub leaf_count: u32,
    /// Leaves by state.
    pub leaf_counts: PlanStatusValues,
    /// The share of the plan's weight by state, in parts per million.
    pub weighted_ppm: PlanStatusValues,
}

impl PlanProgress {
    /// The completed share in parts per million, or `None` for a plan without tasks.
    #[must_use]
    pub const fn completion_ppm(&self) -> Option<u32> {
        if self.leaf_count == 0 {
            None
        } else {
            Some(self.weighted_ppm.completed)
        }
    }
}

/// Where a change came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanChangeCause {
    /// The conversation's model updated the plan.
    ModelUpdate,
    /// The runtime reconciled tasks left `in_progress` by a run that ended.
    RuntimeInterrupt,
}

/// One normalized effect of a batch. A request that changes nothing produces none, and a
/// field that kept its value is not named.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "change", rename_all = "snake_case")]
pub enum PlanChange {
    /// A task was added at `position` among the children of `parent_id` (`None`: top level).
    /// A leaf that receives its first child loses its own state.
    Added {
        task_id: String,
        parent_id: Option<String>,
        position: u32,
        title: String,
        description: String,
        weight: PlanWeight,
        status: PlanTaskStatus,
    },
    /// Fields of a task changed to these values.
    Updated {
        task_id: String,
        title: Option<String>,
        description: Option<String>,
        result: Option<String>,
        blocker: Option<String>,
        weight: Option<PlanWeight>,
        status: Option<PlanTaskStatus>,
    },
    /// A task (with its subtasks) moved to `position` among the children of `parent_id`.
    Moved {
        task_id: String,
        parent_id: Option<String>,
        position: u32,
    },
    /// A task and all its subtasks were deleted. `task_ids` lists them, `task_id` first.
    SubtreeRemoved {
        task_id: String,
        task_ids: Vec<String>,
    },
    /// Every task was deleted.
    Cleared { removed: u32 },
}

/// A change to a conversation's plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload")]
pub enum ConversationPlanEvent {
    /// The plan reached `revision` (1 for its first change). Published only after the
    /// transaction that stored it committed; one event per revision.
    Changed {
        /// The thread the plan was bound to when the change was made.
        thread_id: String,
        plan_id: String,
        revision: u64,
        cause: PlanChangeCause,
        /// The run that caused the change, for audit. Never an authority.
        source_run_id: Option<String>,
        changes: Vec<PlanChange>,
        /// The aggregate after the change.
        progress: PlanProgress,
    },
}

impl ConversationPlanEvent {
    /// Every variant's tag, for readers that look rows up by the `subkind` column.
    pub const SUBKINDS: [&'static str; 1] = ["Changed"];

    /// The thread the event is about.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        match self {
            Self::Changed { thread_id, .. } => thread_id,
        }
    }

    #[must_use]
    pub fn plan_id(&self) -> &str {
        match self {
            Self::Changed { plan_id, .. } => plan_id,
        }
    }

    #[must_use]
    pub fn revision(&self) -> u64 {
        match self {
            Self::Changed { revision, .. } => *revision,
        }
    }

    /// The variant's tag, as written in the serialized payload.
    #[must_use]
    pub const fn subkind_name(&self) -> &'static str {
        match self {
            Self::Changed { .. } => "Changed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weight_is_an_integer_in_range() {
        // Given: numbers at, inside and outside the range, and a fraction
        // Then: only whole numbers 1..=100 deserialize
        for ok in ["1", "50", "100"] {
            assert!(serde_json::from_str::<PlanWeight>(ok).is_ok(), "{ok}");
        }
        for bad in [
            "0", "101", "-1", "1.5", "2.0", "1e1", "\"3\"", "null", "256",
        ] {
            assert!(serde_json::from_str::<PlanWeight>(bad).is_err(), "{bad}");
        }
        assert_eq!(PlanWeight::default().get(), 1);
        assert_eq!(serde_json::to_string(&PlanWeight::MAX).unwrap(), "100");
    }

    #[test]
    fn status_names_round_trip() {
        for status in PlanTaskStatus::ALL {
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(json, format!("\"{}\"", status.as_str()));
            assert_eq!(PlanTaskStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(PlanTaskStatus::parse("done"), None);
    }

    #[test]
    fn changed_event_round_trips_through_the_adjacent_tag_shape() {
        // Given: an event with every kind of change
        let event = ConversationPlanEvent::Changed {
            thread_id: "thread".into(),
            plan_id: "plan-1".into(),
            revision: 3,
            cause: PlanChangeCause::ModelUpdate,
            source_run_id: Some("run".into()),
            changes: vec![
                PlanChange::Added {
                    task_id: "a".into(),
                    parent_id: None,
                    position: 0,
                    title: "t".into(),
                    description: String::new(),
                    weight: PlanWeight::DEFAULT,
                    status: PlanTaskStatus::Pending,
                },
                PlanChange::Cleared { removed: 1 },
            ],
            progress: PlanProgress::default(),
        };
        // When: it is stored as an event kind and read back
        let kind = crate::EventKind::ConversationPlan(event.clone());
        let json = serde_json::to_value(&kind).unwrap();
        // Then: storage finds the thread at the path its retention queries use
        assert_eq!(json["payload"]["payload"]["thread_id"], "thread");
        assert_eq!(json["payload"]["kind"], "Changed");
        let back: crate::EventKind = serde_json::from_value(json).unwrap();
        assert_eq!(back, kind);
    }
}
