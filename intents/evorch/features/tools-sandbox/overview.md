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

## 脅威モデル（ADR 0008、2026-08-29 確定）

OpenCode / pi / senpi は prompt injection を「防げない」と公式に明記し、Codex CLI 以外は OS-level sandbox を持たない調査結果を踏まえ、以下を段階導入する。

- **v0.1**: approval policy（on-request/on-failure/never）と OS enforcement（fs allowlist / network deny / workspace write scope）の二層分離（Codex 方式。承認しても sandbox 外は実行不可）／ credential を agent プロセス・子プロセス・環境変数へ渡さない（keychain 優先、0600 fallback）／ network egress 既定 deny（provider endpoint のみ allowlist）／ tool result 内の `<system-reminder>` 等の制御マーカーをエスケープ
- **v0.1 設計に組み込み・v0.2 実装**: tool result の `ContentOrigin` 型付け（RepositoryUntrusted / WebUntrusted / ToolTrusted 等）／ project trust（`AGENTS.md` / skills / MCP 設定等を未承認時はロードしない。pi/senpi の「trust 解決前に context file を読む」弱点は避ける）
- **v0.3**: untrusted mode（fs read-only / 一時コピー / network deny / credential 不可 / project 拡張無効）

既存 harness 同様、injection は「低減するが根除しない」ものと位置づける（product/overview.md の non-goals 参照予定）。

## v0.1 標準5ツールの実装確定（2026-08-29）

read / edit / grep / shell / git_diff の 5 ツールが `crates/tools/` にコード確定（PR #14、issue #5）。統一 `Tool` trait（`name` / JSON Schema `schema` / `permissions` / async `execute`）+ `ToolExecutor` が `ToolStarted` / `ToolCompleted` を event stream へ emit。要点:

- **制御マーカー エスケープの適用位置**: ディスク書き込みはバイト一致（ファイルは絶対に書き換えない）。エスケープ（`<system-reminder>` / `</system-reminder>` の `<` の直後に `\` 挿入、冪等）は `ToolExecutor` の結果正規化でのみ行う（ADR 0008 整合）
- **edit**: 同一親ディレクトリ上の一時ファイル + `persist` による atomic rename
- **shell**: 非 interactive = `tokio::process::Command`、interactive = portable-pty（one-shot）
- **引数検証**: 各ツールの JSON Schema で実施（新規依存 jsonschema 0.52 no-default-features）

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
