//! Shared lane vocabulary (ADR 0044 §1, §5, §6).
//!
//! These are the plain data types the lane ledger (`storage`), the runtime tools
//! and the GUI exchange, plus the size limits that make them bounded. Nothing here
//! grants authority: a brief is the lead's agent-authored input and a report is a
//! claim, never a verified completion.
//!
//! Oversized or empty input is reported as a [`LaneValidationError`]; nothing is
//! truncated silently.
#[path = "lane_phase2.rs"]
mod phase2;
pub use phase2::*;

use std::fmt;

use serde::{Deserialize, Serialize};

/// Largest summary of one report, in UTF-8 bytes.
pub const LANE_SUMMARY_MAX_BYTES: usize = 1024;
/// Largest report as a whole: its serialized JSON form (`LaneReport` as stored and
/// returned, escapes and structure included), not the sum of its texts.
pub const LANE_REPORT_MAX_BYTES: usize = 4096;
/// Most artifact references one report carries.
pub const LANE_ARTIFACT_REFS_MAX: usize = 8;
/// Most question references one report carries.
pub const LANE_QUESTION_REFS_MAX: usize = 8;
/// Largest single artifact or question reference, in UTF-8 bytes.
pub const LANE_REF_MAX_BYTES: usize = 256;
/// Largest reason code, in UTF-8 bytes.
pub const LANE_REASON_CODE_MAX_BYTES: usize = 64;
/// Largest objective of a brief, in UTF-8 bytes.
pub const LANE_OBJECTIVE_MAX_BYTES: usize = 2048;
/// Largest single list entry of a brief (input, deliverable, criterion, constraint).
pub const LANE_BRIEF_ITEM_MAX_BYTES: usize = 1024;
/// Most entries one list of a brief carries.
pub const LANE_BRIEF_ITEMS_MAX: usize = 32;
/// Largest branch or worktree declaration, in UTF-8 bytes.
pub const LANE_WORKSPACE_FIELD_MAX_BYTES: usize = 512;
/// Largest workspace note, in UTF-8 bytes.
pub const LANE_WORKSPACE_NOTES_MAX_BYTES: usize = 1024;
/// Largest workspace declaration, serialized. Keeps one `lane_list` entry far below the
/// page budget however many characters the JSON has to escape.
pub const LANE_WORKSPACE_MAX_BYTES: usize = 4096;
/// Largest brief as a whole, serialized.
pub const LANE_BRIEF_MAX_BYTES: usize = 16 * 1024;
/// Most entries one `lane_list` page returns.
pub const LANE_LIST_PAGE_MAX: usize = 20;
/// Largest serialized `lane_list` page, in bytes. A page that would pass it ends
/// early (at least one entry is always returned) and continues through its cursor.
pub const LANE_LIST_PAGE_MAX_BYTES: usize = 16 * 1024;

/// Who the lane reports to (ADR 0044 §1). Detached lanes have no lead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LanePolicy {
    /// Managed by its lead, which receives milestones and the closing report.
    Attached,
    /// Managed by its lead, which receives the closing report only.
    Summary,
    /// Independent: the lead's management ended (or never started).
    Detached,
}

impl LanePolicy {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Attached => "attached",
            Self::Summary => "summary",
            Self::Detached => "detached",
        }
    }

    #[must_use]
    pub fn parse(label: &str) -> Option<Self> {
        [Self::Attached, Self::Summary, Self::Detached]
            .into_iter()
            .find(|policy| policy.as_str() == label)
    }

    /// Whether a lead manages the lane under this policy.
    #[must_use]
    pub const fn is_managed(self) -> bool {
        !matches!(self, Self::Detached)
    }
}

/// What a report says about the lane (ADR 0044 §6). `Ready` is a claim, not a
/// verified completion, and is unrelated to a run's `Done` or a goal's completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneStatus {
    Working,
    Waiting,
    Blocked,
    Ready,
    Failed,
}

impl LaneStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Waiting => "waiting",
            Self::Blocked => "blocked",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }

    #[must_use]
    pub fn parse(label: &str) -> Option<Self> {
        [
            Self::Working,
            Self::Waiting,
            Self::Blocked,
            Self::Ready,
            Self::Failed,
        ]
        .into_iter()
        .find(|status| status.as_str() == label)
    }
}

/// Who produced a report. The runtime sets it; the model cannot choose it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneReportProducer {
    LaneAgent,
    Harness,
}

impl LaneReportProducer {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LaneAgent => "lane_agent",
            Self::Harness => "harness",
        }
    }

    #[must_use]
    pub fn parse(label: &str) -> Option<Self> {
        [Self::LaneAgent, Self::Harness]
            .into_iter()
            .find(|producer| producer.as_str() == label)
    }
}

/// The immutable execution scope the runtime gives a lane root (ADR 0044 §3).
/// It is set by the runtime, stored once, and never changed by a model argument,
/// a policy change or a detach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneScope {
    /// A lane root: it cannot open lanes of its own.
    Lane,
}

impl LaneScope {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lane => "lane",
        }
    }

    #[must_use]
    pub fn parse(label: &str) -> Option<Self> {
        (label == Self::Lane.as_str()).then_some(Self::Lane)
    }

    /// Whether a root with this scope may open lanes. Always `false`.
    #[must_use]
    pub const fn can_open_lanes(self) -> bool {
        false
    }
}

/// What an input of a brief refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneInputKind {
    Description,
    Thread,
    Message,
    Artifact,
}

/// One input of a brief: a description or a reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneInput {
    pub kind: LaneInputKind,
    pub value: String,
}

/// The branch and worktree a lane declares (ADR 0044 §14). A declaration only: it
/// reserves nothing, excludes nothing and widens no permission.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneWorkspaceDeclaration {
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub worktree: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

/// The first input of a lane (ADR 0044 §5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneBrief {
    pub objective: String,
    pub inputs: Vec<LaneInput>,
    pub deliverables: Vec<String>,
    pub acceptance_criteria: Vec<String>,
    pub constraints: Vec<String>,
    #[serde(default)]
    pub workspace: LaneWorkspaceDeclaration,
    /// The lane's policy. When a stored brief is read this is the lane's current
    /// policy, not the one it was created with.
    pub report_policy: LanePolicy,
}

/// A report as the runtime accepts it: everything but the identity and sequence the
/// ledger assigns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneReportInput {
    #[serde(default)]
    pub artifact_revision: Option<ArtifactRevision>,
    /// Runtime-held failure episode, not regenerated for every report.
    #[serde(default)]
    pub failure_episode: Option<String>,
    pub status: LaneStatus,
    pub summary: String,
    #[serde(default)]
    pub artifact_refs: Vec<String>,
    #[serde(default)]
    pub needs_human: bool,
    #[serde(default)]
    pub question_refs: Vec<String>,
    #[serde(default)]
    pub reason_code: Option<String>,
    pub producer: LaneReportProducer,
}

/// One stored report (ADR 0044 §6). Immutable once appended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneReport {
    #[serde(default)]
    pub artifact_revision: Option<ArtifactRevision>,
    #[serde(default)]
    pub failure_episode: Option<String>,
    pub lane_id: String,
    /// Assigned by the ledger: 1, 2, 3, … per lane, never reused.
    pub report_seq: u64,
    pub status: LaneStatus,
    pub summary: String,
    pub artifact_refs: Vec<String>,
    pub needs_human: bool,
    pub question_refs: Vec<String>,
    pub reason_code: Option<String>,
    pub producer: LaneReportProducer,
    /// The management revision the lane had when the report was appended.
    pub management_revision: u64,
    /// The brief revision the lane had when the report was appended.
    pub brief_revision: u64,
    pub created_at_ns: i64,
}

/// What was wrong with a field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneValidationKind {
    /// Required and empty.
    Empty,
    /// More bytes than allowed.
    TooLarge { max: usize, actual: usize },
    /// More entries than allowed.
    TooMany { max: usize, actual: usize },
    /// Not acceptable, for the stated reason.
    Invalid(&'static str),
}

/// An input the lane ledger refuses. Carries the field name and limit only, never
/// the offending text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneValidationError {
    pub field: &'static str,
    pub kind: LaneValidationKind,
}

impl LaneValidationError {
    #[must_use]
    pub const fn new(field: &'static str, kind: LaneValidationKind) -> Self {
        Self { field, kind }
    }
}

impl fmt::Display for LaneValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            LaneValidationKind::Empty => write!(formatter, "{} must not be empty", self.field),
            LaneValidationKind::TooLarge { max, actual } => write!(
                formatter,
                "{} is {actual} bytes; at most {max} are allowed",
                self.field
            ),
            LaneValidationKind::TooMany { max, actual } => write!(
                formatter,
                "{} has {actual} entries; at most {max} are allowed",
                self.field
            ),
            LaneValidationKind::Invalid(reason) => write!(formatter, "{}: {reason}", self.field),
        }
    }
}

impl std::error::Error for LaneValidationError {}

fn required_text(field: &'static str, text: &str, max: usize) -> Result<(), LaneValidationError> {
    if text.trim().is_empty() {
        return Err(LaneValidationError::new(field, LaneValidationKind::Empty));
    }
    bounded_text(field, text, max)
}

fn bounded_text(field: &'static str, text: &str, max: usize) -> Result<(), LaneValidationError> {
    if text.len() > max {
        return Err(LaneValidationError::new(
            field,
            LaneValidationKind::TooLarge {
                max,
                actual: text.len(),
            },
        ));
    }
    Ok(())
}

fn optional_text(
    field: &'static str,
    text: Option<&String>,
    max: usize,
) -> Result<(), LaneValidationError> {
    text.map_or(Ok(()), |text| required_text(field, text, max))
}

fn text_list(
    field: &'static str,
    list: &[String],
    max_items: usize,
    max_bytes: usize,
) -> Result<(), LaneValidationError> {
    if list.len() > max_items {
        return Err(LaneValidationError::new(
            field,
            LaneValidationKind::TooMany {
                max: max_items,
                actual: list.len(),
            },
        ));
    }
    list.iter()
        .try_for_each(|item| required_text(field, item, max_bytes))
}

impl LaneReportInput {
    /// Checks the report against the size limits (summary 1 KiB, 8 artifact
    /// references, 4 KiB in all). Nothing is cut: an oversized report is refused so
    /// the producer can shorten it.
    ///
    /// # Errors
    /// Returns the first field that is empty, too large or has too many entries.
    pub fn validate(&self) -> Result<(), LaneValidationError> {
        if self
            .artifact_revision
            .as_ref()
            .is_some_and(|revision| revision.fingerprint().is_none())
        {
            return Err(LaneValidationError::new(
                "artifact_revision",
                LaneValidationKind::Invalid("requires full commit SHA or SHA-256"),
            ));
        }
        if let Some(episode) = &self.failure_episode {
            required_text("failure_episode", episode, 256)?;
        }
        required_text("summary", &self.summary, LANE_SUMMARY_MAX_BYTES)?;
        text_list(
            "artifact_refs",
            &self.artifact_refs,
            LANE_ARTIFACT_REFS_MAX,
            LANE_REF_MAX_BYTES,
        )?;
        text_list(
            "question_refs",
            &self.question_refs,
            LANE_QUESTION_REFS_MAX,
            LANE_REF_MAX_BYTES,
        )?;
        if let Some(code) = &self.reason_code {
            required_text("reason_code", code, LANE_REASON_CODE_MAX_BYTES)?;
            if code.chars().any(char::is_whitespace) {
                return Err(LaneValidationError::new(
                    "reason_code",
                    LaneValidationKind::Invalid("must not contain whitespace"),
                ));
            }
        }
        // The report body alone must already fit; `LaneReport::validate_size` checks the
        // stored form with its identity and counters.
        let total = serde_json::to_string(self).map_or(usize::MAX, |json| json.len());
        if total > LANE_REPORT_MAX_BYTES {
            return Err(LaneValidationError::new(
                "report",
                LaneValidationKind::TooLarge {
                    max: LANE_REPORT_MAX_BYTES,
                    actual: total,
                },
            ));
        }
        Ok(())
    }

    /// Every free-text field, for the secret guard: `(field, text)`.
    #[must_use]
    pub fn guarded_texts(&self) -> Vec<(&'static str, &str)> {
        let mut texts = vec![("summary", self.summary.as_str())];
        texts.extend(
            self.failure_episode
                .as_deref()
                .map(|s| ("failure_episode", s)),
        );
        texts.extend(
            self.artifact_refs
                .iter()
                .map(|text| ("artifact_refs", text.as_str())),
        );
        texts.extend(
            self.question_refs
                .iter()
                .map(|text| ("question_refs", text.as_str())),
        );
        texts.extend(
            self.reason_code
                .as_deref()
                .map(|text| ("reason_code", text)),
        );
        texts
    }
}

impl LaneReport {
    /// Checks the report as it is stored and returned: its serialized form, escapes,
    /// structure, identity and counters included, is at most 4 KiB.
    ///
    /// # Errors
    /// `TooLarge` for the field `report`.
    pub fn validate_size(&self) -> Result<(), LaneValidationError> {
        let total = serde_json::to_string(self).map_or(usize::MAX, |json| json.len());
        if total > LANE_REPORT_MAX_BYTES {
            return Err(LaneValidationError::new(
                "report",
                LaneValidationKind::TooLarge {
                    max: LANE_REPORT_MAX_BYTES,
                    actual: total,
                },
            ));
        }
        Ok(())
    }
}

impl LaneBrief {
    /// Checks the brief: an objective and at least one acceptance criterion are
    /// required, the other lists may be explicitly empty, and every part is bounded.
    ///
    /// # Errors
    /// Returns the first field that is empty, too large or has too many entries.
    pub fn validate(&self) -> Result<(), LaneValidationError> {
        required_text("objective", &self.objective, LANE_OBJECTIVE_MAX_BYTES)?;
        if self.acceptance_criteria.is_empty() {
            return Err(LaneValidationError::new(
                "acceptance_criteria",
                LaneValidationKind::Empty,
            ));
        }
        for (field, list) in [
            ("deliverables", &self.deliverables),
            ("acceptance_criteria", &self.acceptance_criteria),
            ("constraints", &self.constraints),
        ] {
            text_list(field, list, LANE_BRIEF_ITEMS_MAX, LANE_BRIEF_ITEM_MAX_BYTES)?;
        }
        if self.inputs.len() > LANE_BRIEF_ITEMS_MAX {
            return Err(LaneValidationError::new(
                "inputs",
                LaneValidationKind::TooMany {
                    max: LANE_BRIEF_ITEMS_MAX,
                    actual: self.inputs.len(),
                },
            ));
        }
        self.inputs.iter().try_for_each(|input| {
            required_text("inputs", &input.value, LANE_BRIEF_ITEM_MAX_BYTES)
        })?;
        optional_text(
            "workspace.branch",
            self.workspace.branch.as_ref(),
            LANE_WORKSPACE_FIELD_MAX_BYTES,
        )?;
        optional_text(
            "workspace.worktree",
            self.workspace.worktree.as_ref(),
            LANE_WORKSPACE_FIELD_MAX_BYTES,
        )?;
        optional_text(
            "workspace.notes",
            self.workspace.notes.as_ref(),
            LANE_WORKSPACE_NOTES_MAX_BYTES,
        )?;
        let workspace =
            serde_json::to_string(&self.workspace).map_or(usize::MAX, |json| json.len());
        if workspace > LANE_WORKSPACE_MAX_BYTES {
            return Err(LaneValidationError::new(
                "workspace",
                LaneValidationKind::TooLarge {
                    max: LANE_WORKSPACE_MAX_BYTES,
                    actual: workspace,
                },
            ));
        }
        let total = serde_json::to_string(self).map_or(usize::MAX, |json| json.len());
        if total > LANE_BRIEF_MAX_BYTES {
            return Err(LaneValidationError::new(
                "brief",
                LaneValidationKind::TooLarge {
                    max: LANE_BRIEF_MAX_BYTES,
                    actual: total,
                },
            ));
        }
        Ok(())
    }

    /// Every free-text field, for the secret guard: `(field, text)`.
    #[must_use]
    pub fn guarded_texts(&self) -> Vec<(&'static str, &str)> {
        let mut texts = vec![("objective", self.objective.as_str())];
        texts.extend(
            self.inputs
                .iter()
                .map(|input| ("inputs", input.value.as_str())),
        );
        for (field, list) in [
            ("deliverables", &self.deliverables),
            ("acceptance_criteria", &self.acceptance_criteria),
            ("constraints", &self.constraints),
        ] {
            texts.extend(list.iter().map(|text| (field, text.as_str())));
        }
        texts.extend(
            self.workspace
                .branch
                .as_deref()
                .map(|text| ("workspace.branch", text)),
        );
        texts.extend(
            self.workspace
                .worktree
                .as_deref()
                .map(|text| ("workspace.worktree", text)),
        );
        texts.extend(
            self.workspace
                .notes
                .as_deref()
                .map(|text| ("workspace.notes", text)),
        );
        texts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> LaneReportInput {
        LaneReportInput {
            artifact_revision: None,
            failure_episode: None,
            status: LaneStatus::Working,
            summary: "step done".into(),
            artifact_refs: vec![],
            needs_human: false,
            question_refs: vec![],
            reason_code: None,
            producer: LaneReportProducer::LaneAgent,
        }
    }

    fn brief() -> LaneBrief {
        LaneBrief {
            objective: "ship it".into(),
            inputs: vec![],
            deliverables: vec![],
            acceptance_criteria: vec!["it works".into()],
            constraints: vec![],
            workspace: LaneWorkspaceDeclaration::default(),
            report_policy: LanePolicy::Attached,
        }
    }

    #[test]
    fn report_limits_are_errors_not_truncation() {
        // Given: reports one step past each limit.
        let mut long_summary = report();
        long_summary.summary = "x".repeat(LANE_SUMMARY_MAX_BYTES + 1);
        let mut many_refs = report();
        many_refs.artifact_refs = vec!["a".into(); LANE_ARTIFACT_REFS_MAX + 1];
        let mut whole = report();
        whole.summary = "x".repeat(LANE_SUMMARY_MAX_BYTES);
        whole.artifact_refs = vec!["y".repeat(LANE_REF_MAX_BYTES); LANE_ARTIFACT_REFS_MAX];
        whole.question_refs = vec!["q".repeat(LANE_REF_MAX_BYTES); LANE_QUESTION_REFS_MAX];

        // Then: each is refused with the field that overflowed.
        assert_eq!(long_summary.validate().unwrap_err().field, "summary");
        assert!(matches!(
            many_refs.validate().unwrap_err().kind,
            LaneValidationKind::TooMany { max: 8, actual: 9 }
        ));
        let error = whole.validate().unwrap_err();
        assert_eq!(error.field, "report");
        assert!(matches!(
            error.kind,
            LaneValidationKind::TooLarge {
                max: LANE_REPORT_MAX_BYTES,
                ..
            }
        ));

        // And: the limits themselves are accepted.
        let mut at_limit = report();
        at_limit.summary = "x".repeat(LANE_SUMMARY_MAX_BYTES);
        at_limit.artifact_refs = vec!["y".into(); LANE_ARTIFACT_REFS_MAX];
        assert_eq!(at_limit.validate(), Ok(()));
    }

    #[test]
    fn the_report_limit_counts_the_serialized_report_not_the_sum_of_its_texts() {
        // Given: texts that each fit and sum to 3,840 bytes, below the old 4 KiB sum.
        let mut sum_fits = report();
        sum_fits.summary = "x".repeat(LANE_SUMMARY_MAX_BYTES);
        sum_fits.artifact_refs = vec!["y".repeat(LANE_REF_MAX_BYTES); LANE_ARTIFACT_REFS_MAX];
        sum_fits.question_refs = vec!["q".repeat(LANE_REF_MAX_BYTES); 3];
        let texts: usize = sum_fits.summary.len()
            + sum_fits
                .artifact_refs
                .iter()
                .chain(&sum_fits.question_refs)
                .map(String::len)
                .sum::<usize>();
        assert!(texts < LANE_REPORT_MAX_BYTES);
        // Then: the report body alone still fits, but the report as stored and returned
        // (with its identity and counters) is over 4 KiB, so that form is refused.
        assert_eq!(sum_fits.validate(), Ok(()));

        // And: escapes count. 1,024 quotes are 2,048 bytes once serialized.
        let mut escaped = report();
        escaped.summary = "\"".repeat(LANE_SUMMARY_MAX_BYTES);
        escaped.question_refs = vec!["\"".repeat(LANE_REF_MAX_BYTES); 4];
        assert!(
            escaped
                .guarded_texts()
                .iter()
                .map(|(_, t)| t.len())
                .sum::<usize>()
                < LANE_REPORT_MAX_BYTES
        );
        assert_eq!(escaped.validate().unwrap_err().field, "report");

        // And: the stored form, with its identity and counters, is checked too.
        let stored = |summary: String| LaneReport {
            artifact_revision: None,
            failure_episode: None,
            lane_id: "lane".into(),
            report_seq: 1,
            status: LaneStatus::Working,
            summary,
            artifact_refs: vec![],
            needs_human: false,
            question_refs: vec![],
            reason_code: None,
            producer: LaneReportProducer::LaneAgent,
            management_revision: 1,
            brief_revision: 1,
            created_at_ns: 1,
        };
        assert_eq!(stored("s".into()).validate_size(), Ok(()));
        let mut big = stored(sum_fits.summary.clone());
        big.artifact_refs = sum_fits.artifact_refs.clone();
        big.question_refs = sum_fits.question_refs.clone();
        assert_eq!(big.validate_size().unwrap_err().field, "report");
    }

    #[test]
    fn a_workspace_that_escapes_to_a_large_form_is_refused() {
        let mut brief = brief();
        brief.workspace = LaneWorkspaceDeclaration {
            branch: Some("\u{1}".repeat(LANE_WORKSPACE_FIELD_MAX_BYTES)),
            worktree: Some("\u{1}".repeat(LANE_WORKSPACE_FIELD_MAX_BYTES)),
            notes: None,
        };
        let error = brief.validate().unwrap_err();
        assert_eq!(error.field, "workspace");
    }

    #[test]
    fn report_requires_a_summary_and_a_plain_reason_code() {
        let mut empty = report();
        empty.summary = "  ".into();
        assert_eq!(
            empty.validate().unwrap_err().kind,
            LaneValidationKind::Empty
        );
        let mut spaced = report();
        spaced.reason_code = Some("two words".into());
        assert_eq!(spaced.validate().unwrap_err().field, "reason_code");
    }

    #[test]
    fn brief_needs_objective_and_criteria_but_allows_empty_lists() {
        assert_eq!(brief().validate(), Ok(()));
        let mut no_objective = brief();
        no_objective.objective = String::new();
        assert_eq!(no_objective.validate().unwrap_err().field, "objective");
        let mut no_criteria = brief();
        no_criteria.acceptance_criteria.clear();
        assert_eq!(
            no_criteria.validate().unwrap_err().field,
            "acceptance_criteria"
        );
        let mut too_many = brief();
        too_many.constraints = vec!["c".into(); LANE_BRIEF_ITEMS_MAX + 1];
        assert!(matches!(
            too_many.validate().unwrap_err().kind,
            LaneValidationKind::TooMany { .. }
        ));
    }

    #[test]
    fn brief_as_a_whole_is_bounded() {
        let mut brief = brief();
        brief.deliverables = vec!["d".repeat(LANE_BRIEF_ITEM_MAX_BYTES); LANE_BRIEF_ITEMS_MAX];
        brief.constraints = vec!["c".repeat(LANE_BRIEF_ITEM_MAX_BYTES); LANE_BRIEF_ITEMS_MAX];
        assert_eq!(brief.validate().unwrap_err().field, "brief");
    }

    #[test]
    fn labels_round_trip() {
        for policy in [
            LanePolicy::Attached,
            LanePolicy::Summary,
            LanePolicy::Detached,
        ] {
            assert_eq!(LanePolicy::parse(policy.as_str()), Some(policy));
        }
        for status in [
            LaneStatus::Working,
            LaneStatus::Waiting,
            LaneStatus::Blocked,
            LaneStatus::Ready,
            LaneStatus::Failed,
        ] {
            assert_eq!(LaneStatus::parse(status.as_str()), Some(status));
        }
        assert!(!LaneScope::Lane.can_open_lanes());
    }
}

/// Immutable provenance of the original question, independent of its responder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionRequester {
    pub run_id: String,
    pub thread_id: String,
    pub root_run_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum QuestionResponder {
    User,
    Lead { thread_id: String },
}

/// Bound only after a durable lane-delivery host adapter is installed. The default
/// is deliberately fail-safe: Phase 1b's user routing survives partial rollout.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LaneDeliveryBinding {
    #[default]
    Unbound,
    Bound,
}
impl LaneDeliveryBinding {
    pub fn responder(self, policy: LanePolicy, lead: Option<&str>) -> QuestionResponder {
        match (self, policy, lead) {
            (Self::Bound, LanePolicy::Attached, Some(thread_id)) => QuestionResponder::Lead {
                thread_id: thread_id.into(),
            },
            _ => QuestionResponder::User,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionRouting {
    pub question_id: String,
    pub lane_id: String,
    pub requester: QuestionRequester,
    pub responder: QuestionResponder,
    pub management_revision: u64,
    pub routing_revision: u64,
}

/// A verifiable immutable artifact identifier, not a report sequence or mutable path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ArtifactRevision {
    CommitSha(String),
    Sha256(String),
}
impl ArtifactRevision {
    pub fn fingerprint(&self) -> Option<String> {
        let (prefix, value, lengths): (&str, &str, &[usize]) = match self {
            Self::CommitSha(v) => ("commit", v, &[40, 64]),
            Self::Sha256(v) => ("sha256", v, &[64]),
        };
        (lengths.contains(&value.len()) && value.bytes().all(|c| c.is_ascii_hexdigit()))
            .then(|| format!("{prefix}:{}", value.to_ascii_lowercase()))
    }
}

/// Both lead-bound notices and lead answers addressed to the lane use this outbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum LaneNotificationKind {
    Question {
        question_id: String,
        routing_revision: u64,
    },
    Answer {
        question_id: String,
        routing_revision: u64,
    },
    Ready {
        revision: Option<ArtifactRevision>,
    },
    Failed {
        episode: String,
        revision: Option<ArtifactRevision>,
    },
    Closed {
        close_id: String,
    },
    Nudge {
        episode_id: String,
    },
    Milestone,
    Intervention {
        input_id: String,
    },
}

/// References only: input text and full question/report bodies are not duplicated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneNotification {
    pub notification_id: String,
    pub lane_id: String,
    pub management_revision: u64,
    pub source_thread_id: String,
    pub destination_thread_id: String,
    pub kind: LaneNotificationKind,
    pub report_seq: Option<u64>,
    pub created_at_ns: i64,
}
