## Goal

v0.1 用の tool 基盤を実装する。統一 `Tool` trait（`name` / JSON Schema `schema` / `execute` / 結果正規化）と、v0.1 標準5ツール read / edit / grep / shell / git_diff を `crates/tools/` に追加する。tool 実行結果は event stream へ emit する。edit は atomic write と ADR 0008 の制御マーカー エスケープを tool result に適用する。

## Why This Slice Exists Now

mvp-roadmap の v0.1 で Tools 最小セット（read / edit / grep / shell / git diff）が確定しており、agent がファイルを読み・編集・検索・実行するため最初に必要である。ADR 0008 は制御マーカー エスケープを v0.1 必須としており、その無害化を最初に実装する場所もこの slice である。sandbox（v01-sandbox-approval）と orchestration がこの trait に依存する土台となる。

## Current Observed State

Greenfield 状態（Rust コードは未存在。`v01-scaffold` が crate 骨格を作る予定）。tool は一切なく、モデルからの tool call を実行する経路・結果の正規化・event への emit が存在しない。

## Accepted Baseline You May Assume

- tech stack: Rust / Tokio / portable-pty / serde / tracing（architecture.md）
- Tool trait の形状: name / schema / permissions / execute（tools-sandbox overview の要件）
- Shell/PTY 分離: 通常 command は `tokio::process::Command`、interactive（ssh / REPL）は portable-pty（tools-sandbox overview）
- ADR 0008: tool result 内の `<system-reminder>` 等の制御マーカーをエスケープ（v0.1 必須）。ContentOrigin 型付けは v0.2 実装（v0.1 では結果に付与するフィールド余地のみ）
- `v01-scaffold` が crates/ workspace と tracing 基盤を用意済み
- `v01-event-stream` が event stream のイベント型と publish 経路を用意済み
- sandbox / approval / credential は v01-sandbox-approval の責務。本 slice は tool 実行と結果正規化のみ

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/tools/`

Target part: tool trait・標準ツールセット

## In Scope

- 統一 `Tool` trait（`name` / `schema`（JSON Schema）/ `execute` / 結果正規化 / permissions 表明）
- `read`: 任意 path の読み込み。存在しない path は typed error
- `edit`: 一時ファイル + rename の atomic write + 制御マーカー エスケープの tool result 適用
- `grep`: 正規表現による内容検索。存在しない path は typed error
- `shell`: 非 interactive = `tokio::process::Command`、interactive = portable-pty（擬端末実行、出力ストリーム返却）
- `git_diff`: working tree の diff 返却
- tool 実行結果の event stream への emit
- JSON Schema による tool call 引数検証

## Out Of Scope

- MCP 連携（rmcp）と project trust（ADR 0008 v0.2）— v0.1 非対象
- code-intel（tree-sitter / LSP）— v0.2
- sandbox 実行・approval policy・credential 隔離・network deny（v01-sandbox-approval）
- write / glob / bash / git 一般 / diagnostics / delegate 等の他 tool（v0.2 以降）
- ContentOrigin 型付け（ADR 0008 v0.2 実装。ここでは設計余地のみ）

## Standalone Child Issue Contract

`turtton/evorch` に `crates/tools/` 配下で、統一 `Tool` trait（`name` / JSON Schema `schema` / `execute` / 結果正規化）を定義し、read（path 読み込み、存在しない path は typed error）、edit（一時ファイル + rename の atomic write、tool result に ADR 0008 の制御マーカー エスケープを適用）、grep（正規表現検索、存在しない path は typed error）、shell（非 interactive = `tokio::process::Command`、interactive = portable-pty の Shell/PTY 分離）、git_diff（working tree diff）の5標準ツールを実装する。ツール実行結果は event stream へ emit する。各ツールは JSON Schema を返し tool call 引数の検証に使う。MCP / code-intel / sandbox / approval は実装しない。PR は `main` をターゲットにする。

## Acceptance Criteria

- 統一 `Tool` trait（name / schema / execute / 結果正規化）を定義し、read / edit / grep / shell / git_diff の5標準ツールが実装する
- 各ツールが JSON Schema を返し、ツール呼び出し引数に対して検証できる
- read / grep が存在しない path に typed error を返す
- edit が一時ファイル + rename で atomic に書き込み、ADR 0008 の制御マーカー（`<system-reminder>` 等）エスケープを tool result へ適用する
- shell が非 interactive は `tokio::process::Command`、interactive は portable-pty で擬端末実行する（Shell/PTY 分離）
- git_diff が working tree の diff を返す
- tool 実行結果が event stream へ emit される

## Verification

- `cargo test`: 各ツールの正常系と異常系（存在しない path / atomic write / マーカー エスケープ / diff 内容 / Shell-PTY 分離）
- tool result の event stream への emit 形状テスト
- `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Related Links

- intents/evorch/features/tools-sandbox/overview.md
- intents/evorch/decisions/0008-threat-model-phased-adoption.md
- intents/evorch/technology/mvp-roadmap.md
- intents/evorch/technology/architecture.md
- Predecessor: v01-scaffold / v01-event-stream

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs
writeback is expected for this slice. Answer or explicitly decline:

- Intent placement: `features/tools-sandbox` overview（v0.1 標準5ツール確定を記録）
- ADR candidate: none（trait 形状は overview、マーカー エスケープは ADR 0008 で確定済み）
- Diagram candidate: none
- Docs update: none（role-facing surface を追加しないため）
- Closeout writeback expected: yes（tools-sandbox overview への v0.1 標準5ツール記録）

## Guide Reachability (G645)

While the author still knows the answer, name the guide surface and role that route to every
role-facing surface this slice adds, or explicitly say that no role-facing surface is added. A
blank answer is not treated as no-surface. The closeout record is a debt check, not a merge gate.

この slice は role-facing surface（CLI / GUI / 対話 surface）を追加しない。内部 crate（crates/tools/）のみの変更であり、`no_role_facing_surface: true` を宣言する。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.