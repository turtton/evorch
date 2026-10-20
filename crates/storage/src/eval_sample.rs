//! Threads pinned as evaluation samples (ADR 0036 §1).
//!
//! A sample names a thread with a replayable start state, the problem observed
//! in it and the roles the evaluation focuses on. While the sample exists the
//! start state is kept past its retention.

/// A pinned evaluation sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalSample {
    pub thread_id: String,
    /// The observed problem, as the user agreed to it.
    pub problem: String,
    /// Role config addresses such as `worker` or `roles.planner`.
    pub focus_roles: Vec<String>,
    pub created_at_ns: i64,
    pub updated_at_ns: i64,
}

impl EvalSample {
    /// Check the problem text.
    ///
    /// # Errors
    /// Rejects an empty or secret-bearing problem and empty or secret-bearing role names.
    pub fn validate(&self) -> Result<(), crate::StorageError> {
        if self.problem.trim().is_empty() {
            return Err(crate::StorageError::Serialization(
                "evaluation sample problem is empty".into(),
            ));
        }
        if self.focus_roles.iter().any(|role| role.trim().is_empty()) {
            return Err(crate::StorageError::Serialization(
                "evaluation sample focus role is empty".into(),
            ));
        }
        let guard = crate::entity::SecretGuard::from_env();
        guard.check_text("eval_sample", "problem", &self.problem)?;
        self.focus_roles
            .iter()
            .try_for_each(|role| guard.check_text("eval_sample", "focus_roles", role))
    }
}
