# 技術選定再評価（2026-08）

[architecture](architecture.md) / [mvp-roadmap](mvp-roadmap.md)

`agent-harness-concept.md` の構想段階の技術指定を、2026-08 時点の現状で再評価した記録。
変更を確定する場合は decisions/ に ADR として記録し、該当する feature overview / architecture を更新する。

## 1. プロバイダサブスクリプション — 実装ベースで再評価（方針維持）

構想書 §8.1 の `anthropic-subscription` / §32 v0.3 の「Claude subscription provider」は、2026-04-04 の Anthropic 課金カットオーバー以降も **実装可能** と判断を修正する。以下は実コードベースの調査結果（2026-08-29 時点）。

### Anthropic Claude Pro/Max — keep（senpi 方式）

- senpi（code-yeongyu/senpi v2026.8.23）の実装は、**正規の OAuth authorization-code + PKCE** フロー（`claude.ai/oauth/authorize` → `platform.claude.com/v1/oauth/token`、localhost callback）であり、scope に `user:sessions:claude_code` を含む。
- 取得した access token を Anthropic SDK の `apiKey` として通常の Messages API（`api.anthropic.com`）にそのまま渡す。User-Agent spoof、Claude CLI の keychain sync、system prompt の複製は使わない。
- トークン更新は refresh_token フロー、期限 5 分前に provider 単位の lock 下で refresh。
- 「Claude Code restriction」対策として最も近い実装は **Claude Code 風の tool 命名を模倣する Stealth mode**（Read/Write/Edit/Bash/Grep/Glob/...、Claude Code 2.1.75 相当の名前・casing）。これが実際の受理可否に効いているかはコード上は証明できないが、senpi はこの構成で現役動作している。
- pi-mono も同じ OAuth エンドポイント・scope の組み込み OAuth を **削除せず現役で保持** している（`~/.pi/agent/auth.json` 管理）。
- **評価**: `anthropic-subscription` provider type は構想どおり維持。実装は senpi 方式（PKCE OAuth + Claude Code 風 tool naming）を参照。規約リスクは残るため「動作しなくなったら watch に降格」の切り戻し線を明記する。

### OpenAI Codex（ChatGPT Plus/Pro）— keep（OpenCode/pi 方式）

- OpenCode と pi のいずれも、公式の **codex_cli_simplified_flow OAuth** を利用。client_id は共通（`app_EMoamEEZ73f0CkXaXp7hrann`）、issuer `auth.openai.com`、scope `openid profile email offline_access`。browser PKCE と headless device code の両方を実装。
- 推論は `https://chatgpt.com/backend-api/codex/responses`（Responses API 形式）へ。Authorization Bearer + JWT から抽出した `chatgpt_account_id` を `ChatGPT-Account-Id` ヘッダーに付与。session-id も送信。
- **重要**: `originator` ヘッダーに自分たちの名前（`opencode` / `pi`）を明示する。これは OpenAI がサードパーティ利用を識別・許容する公式の窓口であり、「偽装」ではなく公認経路。
- pi は SSE で `OpenAI-Beta: responses=experimental` を付与し、WebSocket/SSE/auto transport に対応。usage limit / 429 の本文を解析して専用メッセージ化。
- 注意点: Codex backend は時期・endpoint 構成によって body 制約（`store: false` / `stream: true` 必須、`max_output_tokens` 拒否等）を変える可能性があり（OpenCode PR #39197）、実装時は Codex CLI 現行版との挙動比較が必須。originator のホワイトリスト検査で 403 になる報告もある（pi Issue #1828）が、現時点で両ツールとも現役。
- **評価**: `openai-codex`（subscription 経由）は v0.3 スコープに構想どおり残す。API key 経由の `openai` type とは区別して実装する。

### GitHub Copilot

- 2026-06-01 から usage-based 課金（AI Credits 制）。device code OAuth は健在（`api.githubcopilot.com/chat/completions` + 短期トークン）。
- **評価**: 実装可能だが課金前提が「定額無制限」から変わった旨をユーザー向けに明示する。v0.3 スコープ維持。

## 2. GUI フレームワーク — 第一/第二候補の入れ替え推奨

| 候補 | 2026-08 現状 | 評価 |
|---|---|---|
| Floem | 安定版 v0.2.0（2024-11）のまま pre-1.0。汎用 dock API なし（Lapce は自前実装、dock 移動で panic する issue が 2026-06 にも存在）。breaking changes 継続 | **watch / 評価用プロトタイプに降格** |
| egui + egui_dock | `anhosh/egui_dock` が現行。0.21.1（2026-08-06）。tab 移動/resize/undock/floating window、DockEvent による layout persistence 対応。活発 | **第一候補に昇格推奨** |
| GPUI + gpui-component | gpui Apache-2.0 だが Zed 追随コスト。gpui-component が dock/nested split/floating/syntax highlight を提供し現実的選択肢に | 長期 watch、試作対象候補 |
| Tauri | Web 資産活用は魅力だが native GUI ではない。非目標に近い | 見送り |
| Slint / Makepad / Iced | それぞれ structured UI / shader / 一般用途に強いが dock workbench の第一候補ではない | watch |

**再評価**: v0.1 prototype は egui + egui_dock で進め、Floem は「評価用プロトタイプ」として docking UX を検証する用途に限定。即時モードの transcript 表示は行単位 chunking + virtualization の自前 widget が必要（どちらの framework でも同じ）。

## 3. PTY — portable-pty keep（pin + 隔離）

- 最新 0.9.0（2025-02-11）。2026 年の crates.io リリースなし。WezTerm 本体の活発さと crate の更新は分離。
- 明確な後継もない。**keep** だが、PTY resize / signal / ConPTY edge case の自動テストを用意し、crate 更新停止を前提に依存を隔離する。

## 4. MCP — rmcp keep（採用前に release cadence 再検証）

- 公式 `modelcontextprotocol/rust-sdk`（crate 名 `rmcp`）を第一候補に維持。
- 調査時点で 2026 年の正確な release 状況が未検証のため、採用時に release 履歴と MCP 仕様対応表の確認を必須条件とする。

## 5. ストレージ — rusqlite keep

- 単一プロセスの event-sourced store には rusqlite + WAL + append-only event tableが最小依存で最適。
- sqlx（async DB 層が別に必要な場合のみ）、libsql / cr-sqlite（replication 要件確定時のみ）は見送り。

## 6. LSP / Code Intelligence — lsp-types + 自前 session 管理

- `lsp-server` は server 側 transport crateであり client abstraction ではない。client 用途は `lsp-types` + 自前の process lifecycle / request correlation / cancellation / restart 管理。
- tree-sitter は keep。grammar ごとの ABI 互換性に注意。

## 変更しないもの

Rust / Tokio / reqwest / serde / clap / SQLite / tracing — いずれも現状で妥当。

## 確定待ちの事項

1. GUI 第一候補の egui + egui_dock への入れ替え（構想書 §22 および gui-workbench feature の更新）
2. ~~v0.3 のサブスクリプションプロバイダ（anthropic-subscription / openai-codex）スコープ~~ → **解決: 構想どおり v0.3 に維持（senpi / OpenCode / pi 方式で実装可能）**
