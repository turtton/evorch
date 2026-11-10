//! Tools contracts for the executor, approvals, shell execution and sandbox composition.
//!
//! One integration-test binary for the former per-file targets below; each
//! test is now named `<old_file>::<test>`.

mod approval;
mod execution_mode;
mod executor;
mod origin;
mod prepared_calls;
mod production_sandbox;
mod shell_cwd;
mod shell_escalation;
mod shell_extra_env;
mod tool_specs;
mod two_layer;
