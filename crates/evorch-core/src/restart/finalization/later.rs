use super::*;
use core_launcher::handoff::{CurrentElf, targets::CaptureCheckedTarget};
use sha2::{Digest, Sha256};
use storage::recovery::restart::RestartMetadata;
use workbench_host::bootstrap::planned_exit::RestartHolds;

/// Consumes only the result of the reversible pre-capture child reprobe.
/// No transaction, target or exec capability is returned to a waiter.
pub struct FinalizationTarget {
    current: Arc<CurrentElf>,
    checked: CaptureCheckedTarget,
    #[cfg(test)]
    pub(crate) observe: Option<Arc<dyn Fn(ExitStage) + Send + Sync>>,
    #[cfg(test)]
    pub(crate) after: Option<Arc<dyn Fn(ExitStage) + Send + Sync>>,
}
impl FinalizationTarget {
    pub fn new(current: Arc<CurrentElf>, checked: CaptureCheckedTarget) -> Self {
        Self {
            current,
            checked,
            #[cfg(test)]
            observe: None,
            #[cfg(test)]
            after: None,
        }
    }
    fn check(&self, metadata: &RestartMetadata) -> Result<(), String> {
        let target = self
            .checked
            .before_exec(&self.current)
            .map_err(|e| e.to_string())?;
        let request = self.checked.request();
        let description = target.description();
        if metadata.target_identity != request.id()
            || metadata.successor_build != description.build_identity
            || metadata.handoff_version != description.handoff_format
            || metadata.snapshot_version != description.snapshot_format
            || metadata.storage_version < description.schema.minimum
            || metadata.storage_version > description.schema.maximum
        {
            return Err("checked target differs from captured transaction".into());
        }
        Ok(())
    }
    fn observe(&self, stage: ExitStage) {
        #[cfg(test)]
        if let Some(observe) = &self.observe {
            observe(stage);
        }
        #[cfg(not(test))]
        let _ = stage;
    }
}
impl RestartCoordinator {
    pub(super) fn six_to_eight(
        &self,
        id: RestartId,
        target: Option<FinalizationTarget>,
        holds: Arc<RestartHolds>,
        metadata: RestartMetadata,
    ) -> Result<(), String> {
        let target = target.ok_or("capture-checked target is not owned; exec prohibited")?;
        target.observe(ExitStage::TargetChecked);
        self.finalization_checkpoint(id, Some(ExitStage::StorageDrained))?;
        target.check(&metadata)?;
        #[cfg(test)]
        if let Some(after) = &target.after {
            after(ExitStage::TargetChecked);
        }
        self.finalization_checkpoint(id, Some(ExitStage::TargetChecked))?;

        target.observe(ExitStage::OwnersHeld);
        self.finalization_checkpoint(id, Some(ExitStage::TargetChecked))?;
        let committed = holds.read_transaction(&metadata)?;
        let held = holds.save_and_read_back(&committed)?;
        #[cfg(test)]
        if let Some(after) = &target.after {
            after(ExitStage::OwnersHeld);
        }
        self.finalization_checkpoint(id, Some(ExitStage::OwnersHeld))?;

        target.observe(ExitStage::Armed);
        self.finalization_checkpoint(id, Some(ExitStage::OwnersHeld))?;
        let nonce_hash = format!(
            "sha256:{:x}",
            Sha256::digest(uuid::Uuid::new_v4().as_bytes())
        );
        let _armed = holds.arm_and_read_back(&held, nonce_hash)?;
        #[cfg(test)]
        if let Some(after) = &target.after {
            after(ExitStage::Armed);
        }
        self.finalization_checkpoint(id, Some(ExitStage::Armed))?;
        // D-2c-2c will continue with 9–11 in this owning driver. D-3 alone
        // introduces raw exec; today every path invalidates and reclaims holds.
        Ok(())
    }
}
