//! Typed sidebar intents. The host repeats validation on the locked, repaired sidebar.
use crate::{LanePolicy, SidebarState, ThreadChatRole, ThreadId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FamilyRevision {
    pub key: ThreadId,
    pub membership_epoch: u64,
    pub archive_revision: u64,
    pub pin_revision: u64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneDragIntent {
    pub thread_id: ThreadId,
    pub lane_id: String,
    pub expected_management_revision: u64,
    pub expected_family_revision: FamilyRevision,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneDropIntent {
    pub source: LaneDragIntent,
    pub target_thread_id: ThreadId,
    pub expected_lead_root_run_id: Option<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneDropError {
    SourceMissing,
    NotDetached,
    StaleManagement,
    Pinned,
    Archived,
    StaleFamily,
    TargetMissing,
    Child,
    Worker,
    LaneRoot,
    DifferentProject,
    ReplacedRoot,
}
impl std::fmt::Display for LaneDropError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::SourceMissing => "The lane no longer exists.",
            Self::NotDetached => "Only a detached lane can be attached.",
            Self::StaleManagement => "Lane management changed; refresh and drag again.",
            Self::Pinned => "Unpin the detached lane before attaching it.",
            Self::Archived => "Restore the detached lane from archive before attaching it.",
            Self::StaleFamily => "Lane family changed; refresh and drag again.",
            Self::TargetMissing => "The destination no longer exists.",
            Self::Child => "A child thread cannot lead a lane; choose a root Orchestrator.",
            Self::Worker => {
                "The destination must be a root Orchestrator, not a Worker or other role."
            }
            Self::LaneRoot => "A lane root cannot lead another lane.",
            Self::DifferentProject => "A lane can only be attached within the same project.",
            Self::ReplacedRoot => "The destination root was replaced; refresh and drag again.",
        })
    }
}
impl std::error::Error for LaneDropError {}
impl SidebarState {
    pub fn lane_family_revision(&self, id: &ThreadId) -> Result<FamilyRevision, LaneDropError> {
        let state = self
            .resolve_states()
            .remove(id)
            .ok_or(LaneDropError::SourceMissing)?;
        let key = state.family.map_err(|_| LaneDropError::StaleFamily)?;
        let epoch = self.membership_epoch(&key);
        Ok(FamilyRevision {
            key,
            membership_epoch: epoch,
            archive_revision: state.archive_revision,
            pin_revision: state.pin_revision,
        })
    }
    pub fn lane_drag_intent(&self, id: &ThreadId) -> Option<LaneDragIntent> {
        let thread = self.threads.iter().find(|thread| &thread.id == id)?;
        let lane = thread.lane.as_ref()?;
        Some(LaneDragIntent {
            thread_id: id.clone(),
            lane_id: lane.lane_id.clone(),
            expected_management_revision: lane.management_revision,
            expected_family_revision: self.lane_family_revision(id).ok()?,
        })
    }
    pub fn validate_lane_drop(&self, intent: &LaneDropIntent) -> Result<(), LaneDropError> {
        let source = self
            .threads
            .iter()
            .find(|t| t.id == intent.source.thread_id)
            .ok_or(LaneDropError::SourceMissing)?;
        let lane = source.lane.as_ref().ok_or(LaneDropError::SourceMissing)?;
        if lane.lane_id != intent.source.lane_id
            || lane.management_revision != intent.source.expected_management_revision
        {
            return Err(LaneDropError::StaleManagement);
        }
        if lane.policy != LanePolicy::Detached || lane.lead_thread_id.is_some() {
            return Err(LaneDropError::NotDetached);
        }
        let states = self.resolve_states();
        let state = states.get(&source.id).ok_or(LaneDropError::StaleFamily)?;
        if state.pinned {
            return Err(LaneDropError::Pinned);
        }
        if state.archived {
            return Err(LaneDropError::Archived);
        }
        if self.lane_family_revision(&source.id)? != intent.source.expected_family_revision {
            return Err(LaneDropError::StaleFamily);
        }
        let target = self
            .threads
            .iter()
            .find(|t| t.id == intent.target_thread_id)
            .ok_or(LaneDropError::TargetMissing)?;
        if target.project_id != source.project_id {
            return Err(LaneDropError::DifferentProject);
        }
        if target.lane.is_some() {
            return Err(LaneDropError::LaneRoot);
        }
        if target.tree_parent().is_some() {
            return Err(LaneDropError::Child);
        }
        if target.chat_role != Some(ThreadChatRole::Orchestrator) {
            return Err(LaneDropError::Worker);
        }
        if target.superseded
            || target.root_run_id.is_none()
            || target.root_run_id != intent.expected_lead_root_run_id
        {
            return Err(LaneDropError::ReplacedRoot);
        }
        Ok(())
    }
}
