//! Config loading, layering, migration and schema contracts.
//!
//! One integration-test binary for the former per-file targets below; each
//! test is now named `<old_file>::<test>`.

mod chain_migration_precedence;
mod load_candidate_text;
mod load_file_overrides;
mod load_layers;
mod mcp_config;
mod migration_marker_visibility;
mod model_presets;
mod preset_resolver;
mod project_layout;
mod schema_artifact;
mod storage_thread_index;
mod strict_rejection;
mod tolerant_load;
