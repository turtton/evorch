//! Redelivering lane notifications held for a thread (ADR 0044 Phase 1c).

use super::{CommandService, HostEvent, RejectKind};

impl CommandService {
    pub(super) fn resume_lane_deliveries(
        &mut self,
        thread_id: String,
        permit: Option<runtime::ownership::OwnerPermit>,
    ) -> Vec<HostEvent> {
        if permit
            .as_ref()
            .is_some_and(|permit| permit.thread_id != thread_id)
            || (self.ownership.is_some() && permit.is_none())
        {
            return vec![HostEvent::rejected(
                RejectKind::Fenced,
                runtime::ownership::OwnershipError::Fenced.to_string(),
            )];
        }
        let run = self.bindings.root_of(&thread_id).map_or_else(
            || self.runtime.latest_chat_run(&thread_id),
            |run| Ok(Some(run)),
        );
        let _guard = self.handle.enter();
        let result = match run {
            Ok(Some(run)) => self
                .runtime
                .resume_lane_deliveries(&thread_id, run, permit.as_ref()),
            Ok(None) => {
                return vec![HostEvent::notice(
                    thread_id,
                    "No conversation to resume lane notifications for",
                )];
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => Vec::new(),
            Err(error) => vec![HostEvent::chat_rejected(
                thread_id,
                RejectKind::Failed,
                format!("Could not resume lane notifications: {error}"),
            )],
        }
    }
}
