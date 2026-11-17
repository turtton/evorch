use super::*;
/// No supported target can be constructed on this platform.
pub enum FinalizationTarget {}
impl RestartCoordinator {
    pub(super) fn six_to_eight(
        &self,
        _: RestartId,
        _: Option<FinalizationTarget>,
        _: Arc<workbench_host::bootstrap::planned_exit::RestartHolds>,
        _: storage::recovery::restart::RestartMetadata,
    ) -> Result<(), String> {
        Err("planned restart is supported only on Linux".into())
    }
}
