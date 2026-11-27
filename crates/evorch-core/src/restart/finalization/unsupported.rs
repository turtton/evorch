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
    ) -> Result<ArmedFinalization, String> {
        Err("planned restart is supported only on Linux".into())
    }
}

pub(super) enum ArmedFinalization {}
impl RestartCoordinator {
    pub(super) async fn nine_to_eleven(
        &self,
        _: RestartId,
        armed: ArmedFinalization,
        _: Arc<runtime::ownership::OwnerHost>,
        _: Arc<runtime::scratch::archive::ArchiveService>,
        _: storage::StorageHandle,
    ) -> Result<(), String> {
        match armed {}
    }
}
