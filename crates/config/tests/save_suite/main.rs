//! Config save/write-back contracts.
//!
//! One integration-test binary for the former per-file targets below; each
//! test is now named `<old_file>::<test>`.

mod agent_features_save;
mod save;
mod save_agent_bindings;
mod save_codex_provider;
mod save_context_contract;
mod save_model_kinds;
mod save_provider;
mod save_routing;
mod save_routing_and_agents;
mod save_scope_callers;
mod save_self_improvement;
