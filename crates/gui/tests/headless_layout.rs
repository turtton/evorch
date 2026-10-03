//! Related GUI layout contracts share a test binary to reduce repeated linking.
//! Each module keeps its own fixtures; nextest still runs each test in a separate process.

#[path = "headless_layout/additional_roles.rs"]
mod additional_roles;
#[path = "headless_layout/approvals_layout_headless.rs"]
mod approvals_layout_headless;
#[path = "headless_layout/conversation_header_controls.rs"]
mod conversation_header_controls;
#[path = "headless_layout/conversation_layout_regression.rs"]
mod conversation_layout_regression;
#[path = "headless_layout/demo_fixture_headless.rs"]
mod demo_fixture_headless;
#[path = "headless_layout/headless_options.rs"]
mod headless_options;
#[path = "headless_layout/pane_typography.rs"]
mod pane_typography;
#[path = "headless_layout/primary_project.rs"]
mod primary_project;
#[path = "headless_layout/right_panes_headless.rs"]
mod right_panes_headless;
#[path = "headless_layout/status_line_headless.rs"]
mod status_line_headless;
#[path = "headless_layout/subagent_tab_titles.rs"]
mod subagent_tab_titles;
#[path = "headless_layout/tasks_navigation_headless.rs"]
mod tasks_navigation_headless;
#[path = "headless_layout/terminal_default_layout.rs"]
mod terminal_default_layout;
#[path = "headless_layout/theme_hierarchy.rs"]
mod theme_hierarchy;
