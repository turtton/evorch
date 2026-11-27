//! Maintenance-only command. No foreground admission, seal or memory callback.
#[cfg(test)]
#[path = "tool_body_retention_tests.rs"]
mod tests;
use super::WriterState;
use crate::{
    StorageError, WriterLeases,
    thread_retention::tool_bodies::{Progress, Request},
};
pub(super) fn retain(
    state: &mut WriterState,
    leases: &WriterLeases,
    request: &Request,
) -> Result<Progress, StorageError> {
    leases.validate_archive_identity(
        &state.config.db_path,
        std::path::Path::new(&request.binding.ownership_root),
    )?;
    let tx = super::immediate_transaction(&state.conn)?;
    let result = crate::repo::event::tool_body_retention::apply(&tx, request)?;
    tx.commit()?;
    if result.changed != 0 {
        state.accounting = crate::repo::event::EventAccounting::default();
    }
    Ok(result)
}
