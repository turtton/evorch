## Goal

builtin `web_fetch` tool を実装し、Librarian の調査相棒として web ドキュメント取得を提供する。明示 selector 優先 → Readability 系本文抽出 → full document fallback の extracter チェーン、text / markdown / html の format 分岐、5MB 応答上限 + 50KB model-facing byte-prefix 切詰め、NetworkGuard 連携、`ContentOrigin::WebUntrusted` の ToolExecutor 層での機械導出を含む。

## Why This Slice Exists Now

v0.2 roadmap の「Librarian/Oracle 追加」「ContentOrigin 実装」に対になる slice。grill `web-tools-v02`（10/10 accepted、`intents/evorch/interviews/web-tools-v02.json`）で web_fetch の設計が確定した。ADR 0008 は ContentOrigin 型を「v0.1 設計に組み込み、実装は v0.2」と位置付けており、network 要求 tool は `WebUntrusted` 型付けが必須である。escape と origin 型付けが ToolExecutor 結果正規化層に集約される現状構成のまま web コンテンツの流入経路を開けないため、この保証枠組みの中で web_fetch を実装するのが本 slice である。

## Current Observed State

- `crates/tools/src/tool.rs:10-47` の `Permissions` は fs_read / fs_write / process_spawn の 3 フラグのみで、tool が network を要求することを表現できない
- `crates/tools/src/result.rs:8-15` の `ToolResult` は `#[non_exhaustive]` の content + is_error のみ。doc には「v0.2 で出力の由来を表す `ContentOrigin` フィールドを追加する予定（ADR 0008）」と明記済みで、未実装
- `crates/event-bus/src/event.rs:204-231` の `ToolEvent::ToolCompleted` は tool_name / call_id / is_error のみで、tool-specific metadata（url / status_code / truncated 等）を運べない
- `crates/tools/src/executor.rs` の ToolExecutor は schema 検証・ToolStarted / ToolCompleted emit・制御マーカーエスケープ（`crates/tools/src/sanitize.rs:15-21`）を既に担う。web_fetch はこの正規化経路に乗る
- 標準 5 tool（read / edit / grep / shell / git_diff、`crates/tools/src/executor.rs:92-108`）に network 要求 tool はなく、bwrap 外 main process で動く builtin tool も存在しない（provider client のみ）
- `crates/agents/src/capability.rs:12-19` の `NetworkAccess` と `crates/runtime/src/network.rs:33-51` の sandbox network mode mapping は存在するが、web tool の 3 層 AND 権限判定と main process 実行経路は未接続
- Librarian role は未実装（`crates/agents/src/role.rs:27` に「新ロールの追加はロール定義の追加のみで完結」とあるのみ）

## Accepted Baseline You May Assume

- grill web-tools-v02 確定（`intents/evorch/features/tools-sandbox/overview.md` v0.2 確定節）: web_search との 2 本分離（q01）/ extracter チェーン（q04）/ senpi 型 size 制限・spill-to-file 不採用（q05）/ ContentOrigin は ToolExecutor 機械導出（q06）/ 3 層 AND + bwrap 外 main process（q07）/ NetworkGuard 境界（q08）/ `network.browser` facet 予約のみ（q09）/ 既存イベント + metadata detail（q10）
- ADR 0002: Librarian は read / search / network allowed、Worker は network denied by default。role capability は runtime レベルで強制する
- ADR 0008: escape は ToolExecutor 結果正規化層でのみ適用、ディスク書き込みはバイト一致の原則。untrusted web コンテンツをファイルへ落とす方式は ContentOrigin / escape 保証が外れるため不採用（Q5 確定）
- NetworkGuard（v02-network-guard）が HTTPS 強制 / redirect 最大 10 回で先も同一ガード再適用 / DNS pinning / link-local（169.254/16）・CGNAT（100.64/10）・IPv6 link-local（fe80::/10）遮断 / loopback・RFC 1918 許可を main process 層で提供する
- reqwest 0.12（workspace 依存、rustls-tls + stream feature）が既存で使用中
- v0.1.1（PR #20）の provider client パターン: main process 実行、credential は main process 環境変数のみ、worker sandbox 内非露出（ADR 0008 credential 分離の延長）

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/tools/`, `crates/runtime/`

Target part: builtin web_fetch tool。明示 selector 優先 → Readability 系本文抽出 → full document fallback の extracter チェーン、format 分岐 (text/markdown/html)、5MB 応答上限 + 50KB model-facing byte-prefix 切詰め、NetworkGuard 連携

## In Scope

- 統一 Tool trait 準拠の `web_fetch`（引数 schema は `{url: string, format?: "text"|"markdown"|"html", timeout_secs?: int}` 程度）
- extracter チェーン: 明示 selector（`<article>` / `<main>`、sidebar / comment / ad 除去）→ Readability 系 → full document fallback。site-aware extractor を先頭に差し込める構造
- format 分岐（html は抽出スキップで生 HTML 返却）
- size 制限: 5MB 三面チェック（Content-Length 事前 / 実読み streaming 累計 / 解凍後累計）+ 50KB model-facing UTF-8 安全 byte-prefix 切詰め + truncation metadata（truncated / original_bytes / 続き取得ヒント）。spill-to-file は採用しない
- 実行権限: NetworkGuard 介した 3 層 AND 判定（role capability ∧ per-tool permission ∧ session NetworkAccess mode）+ main process 実行（bwrap 外）
- `ContentOrigin::WebUntrusted` の ToolExecutor 層での機械導出（fail-closed、tool 自己申告は不可）
- ToolStarted / ToolCompleted + metadata detail（url / final_url / status_code / content_length / decompressed_bytes / truncated / original_bytes / redirect_count / redirect_blocked / extraction_method）。新規イベント種別は追加しない
- 制御マーカーエスケープは ToolExecutor 結果正規化層の既存機構を踏襲
- crate 選定検証: dom_smoothie / readability-rs 系 or scraper + htmd の成熟度比較、reqwest redirect Policy カスタマイズ可能性確認（v02-network-guard と共有）

## Out Of Scope

- サイト専用 extractor（GitHub / arXiv / StackOverflow / npm / docs 系）— v0.3+ backlog（チェーン先頭に差し込める設計のみ保証）
- browser escalation / headless Chromium — v0.2 は `network.browser` facet 予約のみで実装なし（q09）
- context window 連動の動的 model-facing cap（omo 方式）— v0.3+ backlog
- RSS / Atom feed 専用処理
- JavaScript レンダリング
- web_search 本体（別 packet v02-web-search-tool）

## Standalone Child Issue Contract

`turtton/evorch` で、統一 Tool trait に準拠する builtin `web_fetch` tool を実装する。fetch は NetworkGuard（v02-network-guard）経由で bwrap 外 main process で行い、権限は role capability / per-tool permission / session NetworkAccess mode の 3 層 AND とする（どれか 1 層でも deny なら拒否）。本文抽出は明示 selector（`<article>` / `<main>`、sidebar / comment / ad 除去）→ Readability 系 → full document fallback のチェーンで、`format` 引数（text / markdown / html）で分岐し html は抽出なしで生 HTML を返す。response は 5MB（Content-Length 事前 / 実読み streaming 累計 / 解凍後累計の三面）で遮断し、model-facing は 50KB で UTF-8 安全に byte-prefix 切詰めする（超過は失敗でなく切詰め、`truncated: true, original_bytes: N` と続き取得ヒントを metadata で返す）。`ContentOrigin::WebUntrusted` は ToolExecutor 層で capability から機械導出し、tool 自己申告では変わらない。観測は既存 ToolStarted / ToolCompleted を継続し、url / final_url / status_code / content_length / decompressed_bytes / truncated / original_bytes / redirect_count / redirect_blocked / extraction_method を ToolCompleted detail に含める（新規イベント種別なし）。fetch 結果の制御マーカーは既存の ToolExecutor エスケープ機構で無害化する。サイト専用 extractor、browser escalation、動的 cap、RSS 専用処理、JS レンダリングは実装しない。PR は `main` をターゲットにする。

## Acceptance Criteria

- web_fetch が統一 Tool trait に準拠し、ToolExecutor 経由で ToolStarted / ToolCompleted を emit する
- 明示 selector 優先 / Readability 系本文抽出 / full document fallback の 3 段 extracter チェーンが単体テストで検証される
- 代表的な docs サイト（MDN / Rust docs 等）で本文抽出の E2E が通る（mode-lock 可能な fixture または deny by default の network-test として）
- format 分岐が text / markdown / html で期待どおりに動作し、html は抽出をスキップして生 HTML を返す
- 5MB 超 response が Content-Length 事前 / 実読み streaming 累計 / gzip・deflate 解凍後累計の三面チェックで遮断され、metadata で可視化される
- 50KB 超 model-facing 出力が UTF-8 安全な byte-prefix 切詰めで切り詰められ、truncation metadata（truncated: true, original_bytes: N と続き取得ヒント）が付く
- Content-Length 詐称 / 解凍膨張 attack が 5MB guard で防がれる
- ContentOrigin::WebUntrusted が ToolExecutor 層で fail-closed に機械導出される（tool 自己申告では変わらない）
- 権限 3 層 AND（role capability / per-tool permission / session NetworkAccess mode）の各 deny 経路が単独でも拒否される
- 制御マーカーを含む fetch 結果が ToolExecutor 結果正規化層の既存機構でエスケープされる
- redirect_blocked（link-local 等のガード遮断）が metadata で可視化される
- extracter チェーン先頭に site-aware extractor を後から差し込める構造であることを design review で確認する
- cargo test / cargo clippy -- -D warnings / cargo fmt --check / git diff --check が pass する

## Verification

- extracter チェーン / format 分岐 / truncation（UTF-8 境界）/ size 三面 / ContentOrigin 機械導出 / 3 層 AND deny 経路 / escape / metadata の unit tests
- 代表的 docs サイト（MDN / Rust docs 等）の本文抽出 E2E（mode-lock 可能な fixture または deny by default の network-test）
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Related Links

- intents/evorch/features/tools-sandbox/overview.md（v0.2 web ツール確定節）
- intents/evorch/decisions/0002-role-capability-boundaries.md
- intents/evorch/decisions/0008-threat-model-phased-adoption.md
- intents/evorch/interviews/web-tools-v02.json
- 依存 slice: v01-tool-layer、v01-secure-tool-composition-root、v02-network-guard

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs
writeback is expected for this slice. Answer or explicitly decline:

- Intent placement: `features/tools-sandbox/overview.md` primary、ADR 0002 / 0008 と interview `web-tools-v02.json` supporting
- ADR candidate: none（Q1〜Q10 の確定は interview artifact と overview に記録済み）
- Diagram candidate: none
- Docs update: none（role-facing docs なし）
- Closeout writeback expected: yes。web_fetch 実装確定・extracter チェーン構成・採用 crate・size pipeline 結果を overview に記録する

## Guide Reachability (G645)

While the author still knows the answer, name the guide surface and role that route to every
role-facing surface this slice adds, or explicitly say that no role-facing surface is added. A
blank answer is not treated as no-surface. The closeout record is a debt check, not a merge gate.

この slice は Librarian 等 network allowed role が利用する `web_fetch` tool surface を追加するため、`no_role_facing_surface: false` を宣言する。route: `guide workflow task implementation-loop` / implementation role / target surface は web_fetch tool（builtin tool surface）。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
