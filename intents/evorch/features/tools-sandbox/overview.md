# Feature: Tools & Sandbox（ツールシステムとサンドボックス）

[features 一覧](../) / [orchestration](../orchestration/overview.md) / [agent-runtime-kernel](../agent-runtime-kernel/overview.md)

## 概要

Tool は統一 interface（name / schema / permissions / execute）とし、Role ごとに capability を制限する。Shell / PTY と Sandbox もこの feature で扱う。

## 要件

- **Tool trait**: `fn name / fn schema / fn permissions / async fn execute` の統一 interface
- **初期 tool 実装候補**: read / write / edit / grep / glob / bash / git / diagnostics / definition / references / compact_context / delegate / delegate_background
- **Shell / PTY 分離**: 通常 command（cargo test / git diff / rg foo）は `tokio::process::Command`、interactive（ssh / REPL / interactive installer）は PTY（portable-pty 利用候補）
- **Code Intelligence**: LSP と Tree-sitter を独立機能として扱う。Tree-sitter（syntax-aware search / symbol extraction / AST navigation）、LSP（diagnostics / definition / references / hover / rename）
- **Sandbox**: agent ごとの能力に応じた sandbox policy（Codex 的）。Explorer = workspace read-only / network optional、Librarian = read-only / network allowed、Worker = workspace read-write / outside denied / network denied by default、Orchestrator = mutation tools unavailable。プラットフォーム候補: macOS Seatbelt / Linux Landlock・seccomp・namespaces(bwrap) / Windows restricted token・job object
- **MCP**: rmcp を利用

## 受け入れ基準

- Role ごとに tool capability が runtime レベルで制限され、拒否が観測可能であること
- exec と pty が分離され、interactive process を扱えること
- sandbox policy が role ごとに適用されること（v0.2 で sandbox 本格導入）

## Related decisions

- [ADR 0002: Role は capability boundary とし、prompt discipline ではなく権限で分離する](../../decisions/0002-role-capability-boundaries.md)
- [ADR 0006: Harness 自身の診断と自己改善](../../decisions/0006-self-improvement-and-diagnostics.md)

## Open questions

- Linux sandbox の第一実装の選択（Landlock vs bwrap）
- MCP server の接続単位（session ごと / workspace ごと）
