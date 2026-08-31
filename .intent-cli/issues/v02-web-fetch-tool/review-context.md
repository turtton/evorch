# v02-web-fetch-tool Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

## Slice-specific review focus

- extracter チェーンが実際に selector → readability → full document の順序と fallback 条件を持ち、単体テストで検証されているか。site-aware extractor をチェーン先頭に差し込める構造（trait 抽象）が design review で確認できるか
- 50KB model-facing 切詰めが UTF-8 文字境界で安全か（マルチバイト文字の中途切断がないか）。超過が失敗ではなく切詰め + truncation metadata（truncated / original_bytes / 続き取得ヒント）で返るか
- 5MB 三面チェック（Content-Length 事前 / 実読み streaming 累計 / 解凍後累計）が Content-Length 詐称と解凍膨張 attack を実際に防ぐか。Content-Length 欠落時も fail-safe か
- ContentOrigin::WebUntrusted が tool 自己申告でなく capability からの機械導出か（fail-closed）。tool 側が origin を上書きできる経路を残していないか
- 3 層 AND（role capability / per-tool permission / session NetworkAccess mode）の各 deny 経路が単独でも拒否されるか。1 層の通過だけで実行される fail-open 構成になっていないか
- redirect_blocked / redirect_count / extraction_method 等 metadata が Q10 契約どおり ToolCompleted detail で観測可能か。新規イベント種別を追加して既存消費者を壊していないか
- fetch 結果の制御マーカーが ToolExecutor 結果正規化層の既存 `escape_control_markers` 経路で無害化されるか。tool 内に独自 escape を持ち込んでいないか
- spill-to-file（untrusted コンテンツの disk 書き込み）を持ち込んでいないか。Q5 確定で不採用 — ContentOrigin / escape 保証が外れ ADR 0008 の脅威モデルを破る
- サイト専用 extractor / browser escalation / 動的 model-facing cap / RSS 専用処理 / JS レンダリングを v0.2 に持ち込んでいないか（scope widening の目印は新 extractor 実装・Chromium 依存・context engine 接続）
- web_fetch が bwrap 外 main process で実行され、credential 非露出（ADR 0008）と worker sandbox の NetworkAccess 引き締め（v0.1.1）を保つか。tool 実行を worker sandbox 内へ移動していないか

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

注: `intent-cli intent facet-check` は lexical な補助に留まる。上記 Slice-specific review focus が extracter チェーン・size pipeline・権限合成・origin 型付けの意味的な接続を確認する主たる観点である。

## Knowledge Writeback Expectation (G461)

`closeout_learning.write_back_required` は `true`。closeout で以下が host 側に記録されているか確認する（この PR 内または追跡可能な follow-up packet）。

- `features/tools-sandbox/overview.md`: web_fetch 実装確定（extracter チェーン構成・採用 crate の検証結果・size pipeline の 5MB 三面 / 50KB 切詰めの実装結果）を v0.2 確定節へ反映

記録が未実施の場合は、v0.2 web ツールの設計確定と実装の drift が残るため知識 writeback 不足として review 所見に残す。
