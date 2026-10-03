//! Related GUI settings contracts share a test binary to reduce repeated linking.
//! Each module keeps its own fixtures; nextest still runs each test in a separate process.

#[path = "headless_settings/composer_role.rs"]
mod composer_role;
#[path = "headless_settings/gpt_metrics.rs"]
mod gpt_metrics;
#[path = "headless_settings/provider_profiles.rs"]
mod provider_profiles;
#[path = "headless_settings/resolved_model_headless.rs"]
mod resolved_model_headless;
#[path = "headless_settings/role_settings_model.rs"]
mod role_settings_model;
#[path = "headless_settings/role_settings_reviewer_model.rs"]
mod role_settings_reviewer_model;
#[path = "headless_settings/routing_settings_model.rs"]
mod routing_settings_model;
#[path = "headless_settings/sandbox_composer_headless.rs"]
mod sandbox_composer_headless;
#[path = "headless_settings/sandbox_settings_headless.rs"]
mod sandbox_settings_headless;
#[path = "headless_settings/self_improvement_pane.rs"]
mod self_improvement_pane;
#[path = "headless_settings/self_improvement_tab_headless.rs"]
mod self_improvement_tab_headless;
