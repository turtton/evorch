//! Related GUI settings contracts share a test binary to reduce repeated linking.
//! Each module keeps its own fixtures; nextest still runs each test in a separate process.

#[path = "headless_settings/model_kinds.rs"]
mod model_kinds;
#[path = "headless_settings/role_settings_model.rs"]
mod role_settings_model;
#[path = "headless_settings/role_settings_reviewer_model.rs"]
mod role_settings_reviewer_model;
#[path = "headless_settings/routing_settings_model.rs"]
mod routing_settings_model;
