#[path = "../support/paused_process_clock.rs"]
mod paused_process_clock;
#[path = "../support/mod.rs"]
mod support;

mod approval_registry;
mod capability_enforcement;
mod catalog_unknown_tools;
mod file_tool_boundary;
mod identical_tool_calls;
mod malformed_tool_arguments;
mod progress_tool_calls;
mod run_js_dispatch;
mod shell_escalation_off_composition;
mod shell_escalation_runtime;
mod shell_escalation_settings_race;
mod shell_jobs_integration;
mod shell_merge_review_runtime;
mod thread_scratch;
mod tool_scope_gate;
mod tool_specs;
mod unobserved_shell_jobs;
mod workspace_runtime;
