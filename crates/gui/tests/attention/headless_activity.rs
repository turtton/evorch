//! Related GUI activity contracts share a test binary to reduce repeated linking.
//! Each module keeps its own fixtures; nextest still runs each test in a separate process.

#[path = "headless_activity/approval_input_priority.rs"]
mod approval_input_priority;
#[path = "headless_activity/subagents_metrics_headless.rs"]
mod subagents_metrics_headless;
#[path = "headless_activity/tasks_concurrent_runs_headless.rs"]
mod tasks_concurrent_runs_headless;
#[path = "headless_activity/transcript_lag.rs"]
mod transcript_lag;
#[path = "headless_activity/transcript_tool_cli.rs"]
mod transcript_tool_cli;
#[path = "headless_activity/transcript_tool_pending.rs"]
mod transcript_tool_pending;
#[path = "headless_activity/transcript_tool_sticky.rs"]
mod transcript_tool_sticky;
