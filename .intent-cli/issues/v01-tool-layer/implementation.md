# v01-tool-layer Implementation Packet

## Goal

v0.1 用の tool 基盤を `crates/tools/` に実装する。統一 `Tool` trait（`name` / `schema`（JSON Schema）/ `execute` / 結果正規化。tools-sandbox overview の要件を具体化）と、v0.1 標準5ツール read / edit / grep / shell / git_diff を追加する。tool 実行結果は event stream へ emit する（v01-event-stream の tool_result イベントへ接続）。edit は atomic write に加え、ADR 0008 の制御マーカー（`<system-reminder>` 等）エスケープを tool result に適用する。

## Why

mvp-roadmap の v0.1 は「Tools: read / edit / grep / shell / git diff」で確定しており、agent がファイルを読み・編集・検索・実行するための最小ツールセットが必須である。ADR 0008 は制御マーカー エスケープを v0.1 必須としており、tool result の無害化を最初に実装する場所も本 slice である。後の sandbox（v01-sandbox-approval）や orchestration がこの trait と標準ツールに依存する。

## Scope

- 統一 `Tool` trait: `name` / `schema`（JSON Schema、モデルへの tool 定義としても使用）/ `execute`（引数 → 正規化された結果）/ permissions 表明
- `read`: 指定 path の内容を返す。存在しない path は typed error
- `edit`: 一時ファイル + rename による atomic write。partial write を起こさない。tool result に制御マーカー エスケープ（system 構文の無害化）を適用
- `grep`: 正規表現による内容検索。存在しない path は typed error
- `shell`: Shell/PTY 分離 — 非 interactive は `tokio::process::Command`、interactive（REPL / ssh / 対話 installer）は portable-pty で擬端末実行し出力ストリームを返す
- `git_diff`: working tree の diff を返す
- tool 実行結果の event stream への emit
- 各ツールの JSON Schema を返し、tool call 引数の検証に使用

## Out of scope

- MCP 連携（rmcp 採用判断は別途、ADR 0008 の project trust とも連動）— v0.1 非対象
- code-intel（tree-sitter / LSP）— v0.2（mvp-roadmap）
- sandbox 実行・approval policy・credential 隔離（v01-sandbox-approval。本 slice は tool としての実行と結果正規化のみ）
- write / glob / bash / git 一般 / diagnostics / delegate 等、overview の候補リスト中の他 tool（v0.2 以降）
- ContentOrigin 型付け（ADR 0008 では v0.1 設計組み込み・v0.2 実装。型はツール結果に付与するフィールドを残す設計に含める）

## Verification

- `cargo test`: 各ツールの正常系と異常系（存在しない path / atomic write / diff 内容 / マーカー エスケープ）
- shell ツール: 非 interactive の command 実行と portable-pty 経由の interactive 実行を分けたテスト
- tool result が event stream に期待形状で emit されること
- `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: `features/tools-sandbox` overview を primary intent とする。v0.1 標準5ツール確定は closeout で overview に記録する
- ADR candidate: decline — tool trait 形状は tools-sandbox overview に規定済み、マーカー エスケープは ADR 0008 に確定済み。新規 ADR は不要
- Diagram candidate: decline — 既存の概念構成に変更なし
- Docs update: decline — 本 slice は role-facing surface を持たない
- Closeout learning: tools-sandbox overview への v0.1 標準5ツール記録を write back する。`write_back_required: true`

- Guide reachability (G645): 本 slice は内部 crate（crates/tools/）のみを変更し、role-facing guide surface を追加しないため `no_role_facing_surface: true` を宣言する

`improve` (G456 / G460) は later safety net。packet-time で上記を宣言済み。