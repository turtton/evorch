# External MCP file ownership (T0 freeze)

Worktree: /home/turtton/.ghr/github.com/turtton/evorch/.worktrees/project-mcp-servers
Branch: feat/project-mcp-servers; base main at 6a1c41895bf44c9c76e76aac0c36843f7384379d.
All paths are repo-relative. One file, one owner. Stage/commit/push only E, serially
with explicit paths. Shared API/Cargo/module changes must be requested from E.

| Owner | Files | Start / handoff |
| --- | --- | --- |
| E (shared, permanently) | all Cargo.toml/Cargo.lock; crate module/export declarations; crates/config/src/mcp/{mod,types}.rs; crates/tools/src/mcp/{mod,types,error,wire}.rs; tools/src/mcp/transport/{mod,api}.rs; crates/runtime/src/mcp/**; crates/gui/src/mcp.rs; crates/gui/src/model/transcript/diagnostics.rs (shared safe diagnostic rendering); tools/src/mcp/contracts_tests.rs; crates/tools/tests/mcp_contracts.rs; crates/runtime/tests/mcp_contracts.rs; crates/tools/fixtures/mcp/** | T0; fixtures or API edits via E |
| A | crates/config/src/mcp/{load,expand,validate,save}.rs; crates/config/tests/mcp_config.rs | T0 committed, then F1 |
| B | crates/tools/src/mcp/{approval,secrets}.rs; crates/sandbox/src/{credential,keychain}.rs; crates/tools/tests/{mcp_approval,mcp_secrets}.rs | T0 committed; factory/export changes via E |
| C | crates/tools/src/mcp/{client,protocol,codec,headers}.rs; crates/tools/src/mcp/transport/http.rs; crates/tools/src/network_guard/{mcp,stream}.rs; crates/tools/tests/{mcp_client,mcp_session,mcp_protocol,mcp_http}.rs | T0 committed; no wire/search/mod.rs edits |
| D | crates/tools/src/mcp/transport/stdio.rs; crates/sandbox/src/process.rs as needed; crates/tools/tests/mcp_stdio.rs; crates/tools/tests/support/mcp_server.rs; crates/tools/shared-test-support/mcp/mod.rs | T0 committed; C requests shared driver edits from D |
| E (remaining implementation) | crates/tools/src/mcp/{registry,tool,names}.rs; crates/runtime/src/{compose,agent_loop,policy,restore,runtime}.rs + runtime MCP tests/integration; provider name conversion; crates/gui/src/bin/evorch-gui.rs; crates/evorch/src/headless.rs; existing executor/apply_patch_reserved_name/mcp_scope_e2e/mcp_scope_diagnostics tests | Pure names/snapshot after T0, live integration after A/B/C/D |
| F1 | crates/gui/src/panes/project_dialog.rs; crates/gui/src/app/project_dialog.rs; new MCP editor/controller modules (exports via E); crates/gui/tests/mcp_settings.rs | T0 + A; then hand these files to F2 |
| F2 | F1 files after explicit handoff; crates/gui/src/model/commands.rs; crates/gui/src/runtime_sink.rs; crates/gui/tests/mcp_actions.rs | F1 + B + E backend |
| G | crates/runtime/tests/mcp_cache_e2e.rs; existing cache E2E extensions; scripts/contract-test-filters.sh; docs/mcp.md; docs/providers.md; related config docs | E integration + F2; docs included in same PR by operator exception |

Protected unchanged files: crates/tools/src/search/{mcp,envelope}.rs and the four
existing crates/tools/src/mcp/wire.rs functions. E also leaves wire.rs byte-identical
in T0. Other pre-existing worktrees/untracked main files are outside this task.

No worker starts implementation before T0 commit. Once parallel work starts, E
owns synchronization, not per-worker pulls into a dirty shared tree. Report owned
paths, changes, exact tests/results, unknowns and explicit staging paths to E.
PR creation is deferred (or draft only); merge needs separate permission.
