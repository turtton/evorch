//! Preparation belongs to the detached continuation, never the lifecycle loop.
use crate::{
    lifecycle::{
        finalization::{FinalizationError, FinalizationHandle},
        product_exit::ProductExitContext,
    },
    restart::{FinalizationEnvironment, RestartCoordinator, RestartId, public::FinalizationOwner},
};
use core_launcher::handoff::{CurrentElf, targets::CaptureCheckedTarget};
use std::{io, sync::Arc};
use storage::recovery::restart::{
    RestartMetadata, RestartMutation, RestartPhase, RestartTransaction,
};
use workbench_host::{
    admin::{AdminContinuation, AdminFailure, AdminReader},
    projection::Projection,
};
use workbench_protocol::errors::{ErrorCode, RetryAdvice, WorkbenchError};

/// Only shared entry owners and allowlisted typed options. No I/O or waiting.
pub(super) fn environment(context: &ProductExitContext) -> io::Result<FinalizationEnvironment> {
    Ok(FinalizationEnvironment::new(
        context.instance().clone(),
        context.mask().clone(),
        context.signals().clone(),
        context.control().clone(),
        context.options().successor_args(),
    ))
}

#[derive(Clone)]
pub(super) struct ProductFinalizationOwner {
    handle: FinalizationHandle,
    reader: Arc<AdminReader>,
    writer: storage::StorageHandle,
    storage: storage::StorageConfig,
    projection: Arc<Projection>,
    config_identity: String,
    old_build: String,
    builtin_names: std::collections::BTreeSet<String>,
}
impl ProductFinalizationOwner {
    pub(super) fn new(
        inputs: &super::super::Inputs<'_>,
        handle: FinalizationHandle,
        context: &ProductExitContext,
    ) -> Result<Self, AdminFailure> {
        Ok(Self {
            handle,
            reader: inputs.host.admin.reader().clone(),
            writer: inputs.storage_writer().clone(),
            storage: inputs.storage_config().clone(),
            projection: inputs.backend.projection.clone(),
            config_identity: context.instance().identity.clone(),
            old_build: crate::handoff_description::describe()
                .map_err(|_| unavailable())?
                .build_identity,
            builtin_names: inputs.host.runtime.restart_builtin_tool_names(),
        })
    }
    fn prepare(
        &self,
        id: RestartId,
        current: &CurrentElf,
        target: &CaptureCheckedTarget,
    ) -> Result<RestartTransaction, AdminFailure> {
        current.revalidate().map_err(|_| unavailable())?;
        let description = target.request().description();
        let epoch = self.projection.instance_epoch().0;
        let successor_epoch = epoch
            .checked_add(1)
            .filter(|next| *next != u64::MAX && epoch != 0)
            .ok_or_else(unavailable)?;
        let db = storage::Database::open(&self.storage).map_err(|_| unavailable())?;
        let schema = u32::try_from(db.pragma_i64("user_version").map_err(|_| unavailable())?)
            .map_err(|_| unavailable())?;
        if schema < description.schema.minimum || schema > description.schema.maximum {
            return Err(unavailable());
        }
        let metadata = RestartMetadata {
            restart_id: id.public_id().as_str().into(),
            capture_id: uuid::Uuid::new_v4().to_string(),
            config_identity: self.config_identity.clone(),
            old_epoch: epoch,
            successor_epoch,
            old_build: self.old_build.clone(),
            successor_build: description.build_identity.clone(),
            target_identity: target.request().id().into(),
            handoff_version: description.handoff_format,
            snapshot_version: description.snapshot_format,
            storage_version: schema,
            old_builtin_tool_names: self.builtin_names.clone(),
            old_compiled_catalog_fingerprint: None,
            new_compiled_catalog_fingerprint: None,
            old_effective_tools_fingerprint: None,
            new_effective_tools_fingerprint: None,
        };

        create_and_read_back(&self.writer, &db, metadata)
    }
}
impl FinalizationOwner for ProductFinalizationOwner {
    fn finalize(
        &self,
        core: RestartCoordinator,
        id: RestartId,
        current: Arc<CurrentElf>,
        target: CaptureCheckedTarget,
    ) -> AdminContinuation {
        // Claim before metadata, writer jobs or readback. A second caller cannot
        // cancel the first caller or manufacture a fresh delivery after failure.
        let permit = self.handle.claim(id);
        let reader = self.reader.clone();
        let owner = self.clone();
        Box::pin(async move {
            let permit = permit.map_err(delivery_error)?;
            let prepared = reader
                .read(move || {
                    let tx = owner.prepare(id, &current, &target)?;
                    Ok((tx, current, target))
                })
                .await;
            let (tx, current, target) = match prepared {
                Ok(prepared) => prepared,
                Err(error) => {
                    // Still before capture. Preserve the consumed delivery latch.
                    let _ = core.cancel(id);
                    return Err(error);
                }
            };
            let result = permit.start(tx, current, target).await;
            if matches!(
                result,
                Err(FinalizationError::OwnerStopping
                    | FinalizationError::Rejected
                    | FinalizationError::Unavailable)
            ) {
                // cancel() itself refuses once capture won. Never reopen Unknown:
                // loss of the receipt does not establish that start did not run.
                let _ = core.cancel(id);
            }
            result.map_err(delivery_error)
        })
    }
}

fn create_and_read_back(
    writer: &storage::StorageHandle,
    db: &storage::Database,
    metadata: RestartMetadata,
) -> Result<RestartTransaction, AdminFailure> {
    // Exactly one Create, regardless of ACK outcome. Even an acknowledged write
    // is read back; the ACK is not proof of a different transaction's contents.
    let _ack = writer.mutate_restart(
        metadata.restart_id.clone(),
        None,
        RestartMutation::Create(Box::new(metadata.clone())),
    );
    let tx = db
        .restart_transaction(&metadata.restart_id)
        .map_err(|_| unavailable())?
        .ok_or_else(unavailable)?;
    if tx.metadata != metadata || tx.phase != RestartPhase::Preparing {
        return Err(unavailable());
    }
    Ok(tx)
}
pub(super) fn unavailable() -> AdminFailure {
    WorkbenchError::new(
        ErrorCode::StorageUnavailable,
        "restart finalization unavailable",
        RetryAdvice::Never,
    )
}
fn delivery_error(error: FinalizationError) -> AdminFailure {
    let code = match error {
        FinalizationError::Conflict => ErrorCode::Conflict,
        FinalizationError::Unknown => ErrorCode::Internal,
        _ => ErrorCode::Draining,
    };
    WorkbenchError::new(
        code,
        "restart start not confirmed; inspect status without resending",
        RetryAdvice::Never,
    )
}

#[cfg(test)]
mod tests;
