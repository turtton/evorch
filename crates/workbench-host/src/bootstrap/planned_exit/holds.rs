//! Retains restart transaction invalidation across uncertain writer acknowledgements.
use std::sync::Mutex;
use storage::recovery::restart::{
    RestartMetadata, RestartMutation, RestartPhase, RestartTransaction,
};

pub struct RestartTransactionGuard {
    storage: storage::StorageConfig,
    writer: storage::StorageHandle,
    pending: Mutex<Option<RestartMetadata>>,
}
impl RestartTransactionGuard {
    pub(super) fn new(storage: storage::StorageConfig, writer: storage::StorageHandle) -> Self {
        Self {
            storage,
            writer,
            pending: Mutex::new(None),
        }
    }
    pub fn read_transaction(
        &self,
        metadata: &RestartMetadata,
    ) -> Result<RestartTransaction, String> {
        let tx = storage::Database::open(&self.storage)
            .map_err(text)?
            .restart_transaction(&metadata.restart_id)
            .map_err(text)?
            .ok_or("restart transaction missing")?;
        if tx.metadata != *metadata {
            return Err("restart metadata changed".into());
        }
        Ok(tx)
    }
    fn mutate(
        &self,
        old: &RestartTransaction,
        mutation: RestartMutation,
        mut expected: RestartTransaction,
    ) -> Result<RestartTransaction, String> {
        expected.revision = old
            .revision
            .checked_add(1)
            .ok_or("restart revision overflow")?;
        let ack = self.writer.mutate_restart(
            old.metadata.restart_id.clone(),
            Some(old.revision),
            mutation,
        );
        // Read the SAME transaction/revision/nonce even on Err. Never reissue
        // Create/BeginCapture or pretend an unknown ACK is a rejection.
        let observed = self.read_transaction(&old.metadata)?;
        if observed != expected {
            return Err(ack
                .err()
                .map(text)
                .unwrap_or_else(|| "restart mutation readback mismatch".into()));
        }
        Ok(observed)
    }
    pub fn arm_and_read_back(
        &self,
        committed: &RestartTransaction,
        nonce_hash: String,
    ) -> Result<RestartTransaction, String> {
        if committed.phase != RestartPhase::Committed
            || self.read_transaction(&committed.metadata)? != *committed
        {
            return Err("Arm requires the unchanged committed transaction".into());
        }
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending
            .as_ref()
            .is_some_and(|metadata| metadata != &committed.metadata)
        {
            return Err("another restart transaction is retained".into());
        }
        // Remember before mutation, including when the writer commit ACK is lost.
        *pending = Some(committed.metadata.clone());
        let mut next = committed.clone();
        next.phase = RestartPhase::Armed;
        next.ticket_nonce_hash = Some(nonce_hash.clone());
        self.mutate(committed, RestartMutation::Arm { nonce_hash }, next)
    }
    /// Invalidate the exact armed transaction. No run or business work is resumed.
    pub fn cleanup(&self) -> Result<(), String> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(metadata) = pending.as_ref() else {
            return Ok(());
        };
        let tx = self.read_transaction(metadata)?;
        if tx.phase != RestartPhase::Invalidated {
            let mut next = tx.clone();
            next.phase = RestartPhase::Invalidated;
            next.failure_reason = Some("planned finalization ended without exec".into());
            self.mutate(
                &tx,
                RestartMutation::Invalidate {
                    reason: next.failure_reason.clone().unwrap(),
                },
                next,
            )?;
        }
        *pending = None;
        Ok(())
    }
}
fn text(e: impl std::fmt::Display) -> String {
    e.to_string()
}
