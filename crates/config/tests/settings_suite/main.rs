//! Config section parsing, defaults and validation contracts.
//!
//! One integration-test binary for the former per-file targets below; each
//! test is now named `<old_file>::<test>`.

mod additional_roles;
mod advisor_compatibility;
mod browser_tools;
mod budget;
mod comment_checker;
mod core;
mod edit_tool;
mod eval;
mod excluded_models;
mod image_generation_guidance;
mod model_kinds;
mod model_speed;
mod openai_compatible_schema;
mod orchestration;
mod ownership;
mod pricing;
mod provider_model_defaults;
mod pull_requests;
mod role_chains;
mod role_profiles;
mod run_js_tools;
mod sandbox_settings;
mod self_improvement;
mod subscription_providers;
mod team;
mod web_researcher_binding;
