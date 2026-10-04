//! Related GUI activity contracts share a test binary to reduce repeated linking.
//! Each module keeps its own fixtures; nextest still runs each test in a separate process.

#[path = "headless_activity/approval_input_priority.rs"]
mod approval_input_priority;
#[path = "headless_activity/background_notifications_headless.rs"]
mod background_notifications_headless;
#[path = "headless_activity/durable_tasks_model.rs"]
mod durable_tasks_model;
#[path = "headless_activity/legacy_notifications_headless.rs"]
mod legacy_notifications_headless;
#[path = "headless_activity/loop_ui_headless.rs"]
mod loop_ui_headless;
#[path = "headless_activity/notifications_panel_headless.rs"]
mod notifications_panel_headless;
#[path = "headless_activity/scoped_approval_notifications.rs"]
mod scoped_approval_notifications;
#[path = "headless_activity/subagents_metrics_headless.rs"]
mod subagents_metrics_headless;
#[path = "headless_activity/tasks_concurrent_runs_headless.rs"]
mod tasks_concurrent_runs_headless;
#[path = "headless_activity/tasks_work_headless.rs"]
mod tasks_work_headless;
#[path = "headless_activity/thread_metrics_isolation.rs"]
mod thread_metrics_isolation;
#[path = "headless_activity/transcript_lag.rs"]
mod transcript_lag;
#[path = "headless_activity/transcript_tool_cli.rs"]
mod transcript_tool_cli;
#[path = "headless_activity/transcript_tool_pending.rs"]
mod transcript_tool_pending;
