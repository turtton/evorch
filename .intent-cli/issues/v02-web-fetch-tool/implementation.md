# v02-web-fetch-tool Implementation Packet

## Goal

`crates/tools/` に builtin `web_fetch` tool を追加し、grill `web-tools-v02` で確定した設計（`intents/evorch/features/tools-sandbox/overview.md` の v0.2 確定節、q01 / q04 / q05 / q06 / q07 / q08 / q10）を実装に落とす。統一 Tool trait に準拠した `web_fetch`（引数 schema は `{url: string, format?: "text"|"markdown"|"html", timeout_secs?: int}` 程度）を、明示 selector 優先 → Readability 系本文抽出 → full document fallback の extracter チェーン、text / markdown / html の format 分岐、5MB 応答上限 + 50KB model-facing byte-prefix 切詰めの size pipeline、NetworkGuard（v02-network-guard）連携付きで実装する。実行は bwrap 外 main process（v0.1.1 provider client パターンの拡張）とし、`ContentOrigin::WebUntrusted` を ToolExecutor 層で機械導出する。

## Why

v0.2 roadmap の「Librarian/Oracle 追加」「ContentOrigin 実装」に対になる slice で、Librarian の調査相棒として web ドキュメント閲覧を提供する。ADR 0008 は ContentOrigin 型を「v0.1 の設計に組み込み、実装は v0.2」と位置付けており（`intents/evorch/decisions/0008-threat-model-phased-adoption.md:28-31`）、network 要求 tool である web_fetch は `WebUntrusted` 型付けが最初に必要になる適用先である。untrusted web コンテンツは制御マーカーエスケープと origin 型付けの両方を ToolExecutor 結果正規化層で保証する必要があり、spill-to-file（ディスク経由の返却）は「ローカルファイル」として扱われた瞬間に両保証が外れるため Q5 確定で不採用済み。この保証を壊さないまま web コンテンツの流入経路を開くのが本 slice の security 要件である。

## Scope

- 統一 Tool trait（`crates/tools/src/tool.rs:55-69`）準拠の `web_fetch` tool。tool 名は `web_fetch`、引数 schema は `{url: string, format?: "text"|"markdown"|"html", timeout_secs?: int}` 程度
- extracter チェーン設計:
  - 第1段: 明示 selector 優先（`<article>` / `<main>`、sidebar / comment / ad 要素は除去）
  - 第2段: Readability 系本文抽出（失敗時 → 第3段）
  - 第3段: full document fallback
  - チェーン構造として、将来 v0.3+ で site-aware extractor（GitHub / arXiv 等）を先頭に差し込める形（trait 抽象で順序と fallback を表現）
- format 分岐（text / markdown / html、html は抽出スキップで生 HTML 返却）。OpenCode / senpi と同型
- size 制限（NetworkGuard と連携、Q5 確定）:
  - response 上限 5MB: Content-Length 事前 + 実読み streaming 累計 + gzip / deflate 解凍後累計の三面
  - model-facing 上限 50KB、UTF-8 安全 byte-prefix 切詰め。超過は失敗でなく切詰め
  - truncation metadata: `truncated: true, original_bytes: N` + 続き取得ヒント（range fetch 可否 / 狭いクエリ誘導）
  - spill-to-file は採用しない（ContentOrigin / escape 保証外れ + sandbox 境界衝突のため、tools-sandbox overview に確定済み）
- 実行権限: NetworkGuard（v02-network-guard）を介した 3 層 AND 判定（role capability ∧ per-tool permission ∧ session NetworkAccess mode）+ main process 実行、bwrap 外
- `ContentOrigin::WebUntrusted` を ToolExecutor 層で機械導出（tool が要求する network capability から決定。Q6 確定の mapping に従い、tool 自己申告は不可、fail-closed）
- ToolStarted / ToolCompleted + metadata detail: url, final_url（redirect 後）, status_code, content_length, decompressed_bytes, truncated（bool）, original_bytes（truncated=true 時）, redirect_count, redirect_blocked（Q8 ガード遮断時 true）, extraction_method（selector / readability / full）。新規イベント種別は追加せず ToolCompleted の detail に包含する（Q10）
- 制御マーカーエスケープは ToolExecutor 結果正規化層の既存機構（`escape_control_markers`、`crates/tools/src/sanitize.rs`）踏襲
- crate 選定検証タスク: dom_smoothie / readability-rs 系 or scraper + htmd の組合せの成熟度比較、reqwest redirect Policy カスタマイズ可能性確認（v02-network-guard と共有）

## Out of scope

- サイト専用 extractor（GitHub / arXiv / StackOverflow / npm / docs 系）→ v0.3+ backlog（chains 先頭に差し込める設計のみ今回保証）
- browser escalation / headless Chromium → v0.2 は `network.browser` facet 予約のみで実装なし（q09）
- context window 連動の動的 model-facing cap（omo 方式）→ v0.3+ backlog
- RSS / Atom feed 専用処理
- JavaScript レンダリング
- web_search 本体の実装（別 packet v02-web-search-tool。ToolCompleted detail の metadata 機構は汎用として共有する）

## Verification

- extracter チェーン unit tests: selector 成功 / readability fallback / full document fallback の各経路と `extraction_method` 記録
- format 分岐 unit tests: text / markdown / html（html は抽出なし生 HTML）
- truncation unit tests: 50KB 境界とマルチバイト UTF-8 文字の中途切断なし、truncated / original_bytes metadata
- size guard tests: 5MB 超の Content-Length 詐称・実読み超過・解凍膨張が三面で遮断される
- ContentOrigin tests: network 要求 tool が fail-closed に WebUntrusted になり、tool 自己申告では変わらない
- 3 層 AND tests: role capability deny / per-tool permission deny / session NetworkAccess deny の各経路が単独でも拒否される
- escape tests: `<system-reminder>` を含む fetch 結果が ToolExecutor 正規化層でエスケープされる
- metadata tests: redirect_blocked / redirect_count / extraction_method が ToolCompleted detail で観測可能
- 代表的 docs サイト（MDN / Rust docs 等）の本文抽出 E2E（mode-lock 可能な fixture または deny by default の network-test）
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: `features/tools-sandbox/overview.md` を primary とし、ADR 0002 / 0008 と interview `web-tools-v02.json` を supporting とする。新規 intent は不要
- ADR candidate: decline — Q1〜Q10 の確定は interview artifact と overview v0.2 確定節に記録済みで、本 slice で新たに ADR に値する決定は発生しない
- Diagram candidate: decline — extracter チェーンと size pipeline は overview の記述で十分
- Docs update: decline — role-facing docs は追加しない（tool surface は Librarian 用で guide_reachability に記録）
- Closeout learning: web_fetch 実装確定・extracter チェーン構成・採用 crate・size pipeline 結果を overview に記録する。`write_back_required: true`

- Guide reachability (G645): web_fetch は Librarian 等 network allowed role が使う tool surface を追加するため `no_role_facing_surface: false`。route: guide workflow task implementation-loop / implementation role / target surface は web_fetch tool

`improve` (G456 / G460) は later safety net。packet-time で writeback を宣言済み。
