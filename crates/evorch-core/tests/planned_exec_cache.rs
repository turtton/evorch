#![cfg(target_os = "linux")]
//! M2 -> raw exec -> production successor, with an input-derived HTTP cache.
//! The predecessor is a fixture ELF, not the product: both real identities are
//! recorded. Same-build matrix is covered by recovery_activation_cache; actual
//! same-build product -> product exec belongs to Stage 16a-2 F4.
#[path = "planned_exec_cache/mod.rs"]
mod planned_exec_cache;
#[path = "../shared-test-support/signal_entry.rs"]
mod signal_entry;
#[path = "renderer_support/without_artifact_tool.rs"]
mod without_artifact_tool;

#[tokio::test(flavor = "multi_thread")]
async fn raw_exec_different_build_unchanged_catalog_preserves_cache_prefix() {
    planned_exec_cache::check(false, false).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn raw_exec_different_build_changed_catalog_has_one_tools_only_boundary() {
    planned_exec_cache::check(true, false).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn raw_exec_changed_builtin_catalog_preserves_disconnected_mcp_prefix() {
    planned_exec_cache::check(true, true).await;
}
#[test]
#[ignore = "private predecessor process; entry constructor masks signals before libtest"]
fn cache_predecessor() {
    planned_exec_cache::worker::run();
}
