//! A reservation is released only before registration and only while its exact
//! descriptor still owns the stored consumption marker. History/evidence are not undone.
use super::*;

pub(crate) struct SnapshotReservation {
    handle: storage::StorageHandle,
    run_id: RunId,
    id: String,
    retry_restorable: bool,
    retry_descriptor_restorable: bool,
    retry_reason: Option<String>,
    expected_config: String,
    retry_config: String,
    armed: bool,
}

impl SnapshotReservation {
    pub(super) fn new(
        store: &crate::RunStore,
        run_id: RunId,
        record: &storage::RunContextRecord,
        descriptor: &RunRestoreDescriptor,
    ) -> Result<Self, RuntimeError> {
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes).map_err(|error| RuntimeError::Model {
            reason: format!("cannot reserve chat snapshot: {error}"),
        })?;
        Ok(Self {
            handle: store.handle.clone(),
            run_id,
            id: format!("{:032x}", u128::from_le_bytes(bytes)),
            retry_restorable: record.restorable,
            retry_descriptor_restorable: descriptor.restorable,
            retry_reason: descriptor.non_restorable_reason.clone(),
            expected_config: String::new(),
            retry_config: String::new(),
            armed: false,
        })
    }

    pub(super) fn id(&self) -> &str {
        &self.id
    }

    pub(super) fn arm(mut self, descriptor: &RunRestoreDescriptor) -> Self {
        self.expected_config = serde_json::to_string(descriptor).expect("serializable descriptor");
        let mut retry = descriptor.clone();
        retry.admission_reservation_id = None;
        retry.restorable = self.retry_descriptor_restorable;
        retry.non_restorable_reason = self.retry_reason.clone();
        self.retry_config = serde_json::to_string(&retry).expect("serializable descriptor");
        self.armed = true;
        self
    }

    pub(crate) fn commit(&mut self) {
        self.armed = false;
    }
}

impl Drop for SnapshotReservation {
    fn drop(&mut self) {
        if self.armed
            && let Err(error) = self.handle.compare_and_swap_run_context_restore_state(
                &self.run_id.to_string(),
                &self.expected_config,
                &self.retry_config,
                self.retry_restorable,
            )
        {
            // Keep consumption fail-closed if storage cannot confirm the release.
            tracing::error!(run_id = %self.run_id, %error, "failed to release chat snapshot admission reservation");
        }
    }
}
