# v02-web-search-tool Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

## Slice-specific review focus

- `web_search` が統一 Tool trait に実際に準拠し、`ToolExecutor` 経由でのみ実行されるか。tool 内部から transport を直接公開する bypass 経路を生やしていないか
- keyless MCP transport が「builtin tool 内部の transport プロトコル」として閉じ込められているか。rmcp client 層・汎用外部 MCP 接続機構を v0.2 に持ち込んでいないか（q02 の不採用案）
- fallback が Exa keyless 既定 → Tavily keyless 一次 fallback の直線構造に留まるか。2 社対等振分（OpenCode V2 式）や多段 fallback を持ち込んでいないか。`used_fallback` / `fallback_attempts` / `credential_status` で観測可能か
- provider credential が main process 環境変数のみで消費され、worker sandbox / bwrap 内子プロセス env に漏れていないか。unit test で検証されているか（ADR 0008）
- 3 層 AND の各 deny 経路（role capability / per-tool permission / session NetworkAccess mode）が単独でも拒否するか。1 層のみの通過で実行される fail-open 経路がないか
- `ContentOrigin::WebUntrusted` が `ToolExecutor` の結果正規化層で capability から機械導出され、tool 自己申告・上書き経路がないか（fail-closed、q06）
- 制御マーカーエスケープが `ToolExecutor` 結果正規化層で適用され、provider 応答が素通しになっていないか。ディスク書き込み（バイト一致の原則）に変更がないか
- `ToolCompleted` detail の追加が既存 event 消費者と下位互換か。新規イベント種別を追加していないか（q10 で不採用の新規イベント種別案）
- `SearchProvider` trait の拡張点が非 breaking か（mock provider 検証）。key 必須 provider / provider-native routing / cache layer / cross-session dedup を v0.2 に持ち込んでいないか

## Facet context

<!-- BEGIN GENERATED FACET CONTEXT (G530) -->
### vocabulary
- (none overlapping this packet's intent_references)
### invariant
- (none overlapping this packet's intent_references)
### decider
- (none overlapping this packet's intent_references)
### acceptance-property
- (none overlapping this packet's intent_references)
<!-- END GENERATED FACET CONTEXT (G530) -->

注: `intent-cli intent facet-check` は lexical な補助に留まる。上記 Slice-specific review focus が credential 非露出・fail-closed 導出・3 層 AND といった security boundary の意味的接続を確認する主たる観点である。

## Knowledge Writeback Expectation (G461)

`closeout_learning.write_back_required` は `true`。closeout で以下が host 側に記録されているか確認する（この PR 内または追跡可能な follow-up packet）。

- `features/tools-sandbox/overview.md`: web_search tool の実装確定（Exa keyless 既定 + Tavily keyless 一次 fallback の直線構造、`SearchProvider` trait の位置と非 breaking 拡張点、fallback 挙動と metadata schema、3 層 AND / bwrap 外 main process 実行の接続結果）
- 受け入れ基準「web_search / web_fetch が v0.2 で Librarian から利用可能…」のうち web_search 側の充足状態

記録が未実施の場合は、v0.2 確定節と実装の drift が残るため知識 writeback 不足として review 所見に残す。
