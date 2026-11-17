//! The conversation plan's writes and reads through the single writer.

use super::{Command, StorageHandle};
use crate::StorageError;
use crate::conversation_plan::{
    PlanApplied, PlanHistoryPage, PlanSummary, PlanTaskRead, PlanTreePage, PlanUpdate,
};
use crate::repo::conversation_plan::{self as repo, WriteRequest};

impl StorageHandle {
    /// Applies the model's batch of plan operations to the plan of `update.thread_id`.
    ///
    /// One transaction checks the thread, compares `expected_revision` with the plan's
    /// revision, validates the whole batch, rewrites the projection and stores the one
    /// change event with its history row. Any failure leaves nothing behind. A batch that
    /// changes nothing returns `changed == false` without a revision or an event.
    /// The returned `event` is the stored change: the caller publishes it to the bus as a
    /// notification only, because the recorder never stores this event kind.
    ///
    /// # Errors
    /// `StaleRevision { kind: "conversation_plan" }`, `ConversationPlan(..)` for a refused
    /// batch, `SecretDetected`, `LimitExceeded` (event size), `Tombstoned`, suspended
    /// writes or a closed writer.
    pub fn update_conversation_plan(
        &self,
        update: PlanUpdate,
    ) -> Result<PlanApplied, StorageError> {
        self.request(|reply| Command::ConversationPlan(WriteRequest::Update(update), reply))
    }

    /// The runtime's reconciliation: every `in_progress` leaf of the thread's plan becomes
    /// `interrupted` in one revision (cause `runtime_interrupt`). A plan with no such leaf,
    /// or a thread with no plan, is left alone (`changed == false`).
    ///
    /// # Errors
    /// As [`Self::update_conversation_plan`], except for the revision check.
    pub fn reconcile_conversation_plan(
        &self,
        thread_id: &str,
        source_run_id: Option<&str>,
    ) -> Result<PlanApplied, StorageError> {
        let request = WriteRequest::Reconcile {
            thread_id: thread_id.to_owned(),
            source_run_id: source_run_id.map(str::to_owned),
        };
        self.request(|reply| Command::ConversationPlan(request, reply))
    }

    /// Moves a plan, with its identity, revision and history, to another thread that has
    /// no plan. Returns whether a plan moved. A planned handoff does this itself when it
    /// commits; this call is for a caller that rebinds outside a handoff.
    ///
    /// # Errors
    /// `ConversationPlan(BindingConflict)` when the target has a plan, `Tombstoned`.
    pub fn rebind_conversation_plan(
        &self,
        from_thread: &str,
        to_thread: &str,
    ) -> Result<bool, StorageError> {
        let (from, to) = (from_thread.to_owned(), to_thread.to_owned());
        self.lane_job(true, move |conn| {
            let tx = rusqlite::Transaction::new_unchecked(
                conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            let moved = repo::rebind_thread(&tx, &from, &to)?;
            tx.commit()?;
            Ok(moved)
        })
    }

    /// Installs the gate that holds this writer's plan writes at a point the test arms.
    /// Only in builds with the `plan-write-gate` feature.
    ///
    /// # Errors
    /// A closed writer.
    #[cfg(feature = "plan-write-gate")]
    pub fn install_plan_write_gate(
        &self,
        gate: std::sync::Arc<crate::plan_write_gate::PlanWriteGate>,
    ) -> Result<(), StorageError> {
        self.lane_job(false, move |_| {
            crate::plan_write_gate::install(gate);
            Ok(())
        })
    }

    /// The plan's revision and progress (revision 0, no plan ID: never created).
    ///
    /// # Errors
    /// `Tombstoned` or a closed writer.
    pub fn conversation_plan_summary(&self, thread_id: &str) -> Result<PlanSummary, StorageError> {
        let thread = thread_id.to_owned();
        self.lane_job(false, move |conn| repo::summary(conn, &thread))
    }

    /// One page of the tree in display order. A cursor belongs to the revision that issued
    /// it: after the plan changed, it is refused with `StaleCursor`.
    ///
    /// # Errors
    /// `ConversationPlan(StaleCursor | InvalidCursor)`, `Tombstoned` or a closed writer.
    pub fn conversation_plan_tree(
        &self,
        thread_id: &str,
        cursor: Option<&str>,
        limit: Option<u32>,
    ) -> Result<PlanTreePage, StorageError> {
        let (thread, cursor) = (thread_id.to_owned(), cursor.map(str::to_owned));
        self.lane_job(false, move |conn| {
            repo::tree_page(conn, &thread, cursor.as_deref(), limit)
        })
    }

    /// The full detail of one task.
    ///
    /// # Errors
    /// `ConversationPlan(UnknownTask | ForeignTask)`, `Tombstoned` or a closed writer.
    pub fn conversation_plan_task(
        &self,
        thread_id: &str,
        task_id: &str,
    ) -> Result<PlanTaskRead, StorageError> {
        let (thread, task) = (thread_id.to_owned(), task_id.to_owned());
        self.lane_job(false, move |conn| repo::task(conn, &thread, &task))
    }

    /// Change events of the plan after `after_revision`, oldest first, found through the
    /// history index (no scan of the event log).
    ///
    /// # Errors
    /// `Tombstoned` or a closed writer.
    pub fn conversation_plan_history(
        &self,
        thread_id: &str,
        after_revision: Option<u64>,
        limit: Option<u32>,
    ) -> Result<PlanHistoryPage, StorageError> {
        let thread = thread_id.to_owned();
        self.lane_job(false, move |conn| {
            repo::history(conn, &thread, after_revision, limit)
        })
    }
}
