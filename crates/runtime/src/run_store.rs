//! run の永続化 writer と読み取り接続。

use std::collections::HashSet;
use std::sync::Mutex;

use storage::{
    Database, RunContextRecord, RunLedgerEntry, StorageConfig, StorageError, StorageHandle,
};

use crate::RunId;

pub struct RunStore {
    pub(crate) handle: StorageHandle,
    database: Mutex<Database>,
    pub(crate) next_run_id: u64,
    failed_snapshots: Mutex<HashSet<RunId>>,
}

impl RunStore {
    /// 同じ DB を使用する writer handle と読み取り接続を束ねる。
    ///
    /// # Errors
    /// DB を開けない場合に storage のエラーを返す。
    pub fn open(config: &StorageConfig, handle: StorageHandle) -> Result<Self, StorageError> {
        let database = Database::open(config)?;
        let next_run_id = database
            .max_run_context_run_id()?
            .checked_add(1)
            .ok_or_else(|| StorageError::Serialization("run ID overflow".into()))?;
        Ok(Self {
            handle,
            database: Mutex::new(database),
            next_run_id,
            failed_snapshots: Mutex::new(HashSet::new()),
        })
    }

    pub(crate) fn snapshot_failed(&self, run_id: RunId) -> bool {
        self.failed_snapshots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&run_id)
    }

    pub(crate) fn invalidate_snapshot(&self, run_id: RunId) -> Result<(), StorageError> {
        self.failed_snapshots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(run_id);
        if let Some(mut record) = self.restore_record(run_id)? {
            // Retain prior messages for diagnosis, but never accept them as current history.
            let mut descriptor: crate::restore::RunRestoreDescriptor =
                serde_json::from_str(&record.config_json)
                    .map_err(|error| StorageError::Serialization(error.to_string()))?;
            descriptor.restorable = false;
            descriptor.non_restorable_reason = Some("persist_failed".into());
            record.restorable = false;
            record.config_json = serde_json::to_string(&descriptor)
                .map_err(|error| StorageError::Serialization(error.to_string()))?;
            self.handle.upsert_run_context(&record)?;
        }
        Ok(())
    }

    pub(crate) fn restore_record(
        &self,
        run_id: RunId,
    ) -> Result<Option<RunContextRecord>, StorageError> {
        self.database
            .lock()
            .map_err(|error| StorageError::Io(error.to_string()))?
            .run_context(&run_id.to_string())
    }

    pub(crate) fn restore_parent(
        &self,
        run_id: RunId,
    ) -> Result<Option<Option<String>>, StorageError> {
        self.database
            .lock()
            .map_err(|error| StorageError::Io(error.to_string()))?
            .run_context_parent(&run_id.to_string())
    }

    pub(crate) fn ledger_entries(
        &self,
        run_id: RunId,
    ) -> Result<Vec<RunLedgerEntry>, StorageError> {
        self.database
            .lock()
            .map_err(|error| StorageError::Io(error.to_string()))?
            .run_ledger(&run_id.to_string())
    }
}
