# v02-intent-gate-rules Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

## What the reviewer should check

- 振る舞いを保つリファクタであること。ポリシーモジュール抽出後も、`crates/runtime/src/prompt/assembly.rs:34-55` の組立順（role baseline → family → category overlay → Intent Gate → appendix）と `Role::Orchestrator` 限定挿入、keyTriggers 埋め込みの構造が変わっていないこと。
- PR #50 由来の byte-identical golden test（同一入力でバイト一致の決定論性）が変更なく pass すること。レンダリング経路が HashMap 反復など順序の揺れる構造に依存していないこと。
- ポリシーモジュールが単一の正本として機能していること。`crates/runtime` のコードと prompt 生成の両方が同じ型定義を参照し、ルールの記述がモジュール外に複製されていないこと。
- routing prompt（Layer A 用・将来）と Orchestrator prompt（既存）の両方を生成できる API が公開され、両 consumer が同じモジュールを経由すること。
- Orchestrator 向け gate 本文から再分類指示が削除され、pre-routing の選択結果を検証する説明に置き換わっていること（検証のフレーミングは残っている）。
- gate 本文の文言変更に伴い #49 由来の golden 文字列が更新されている場合、差分が「再分類指示の削除・検証説明への置換」に一致し、無関係な文言変更や余白の揺れが混入していないこと。
- `crates/routing` への型配置や Layer A 本体、GUI/CLI entry の変更など Out Of Scope の差分が混入していないこと。

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

## Knowledge Writeback Expectation (G461)

If the packet's `closeout_learning.write_back_required` is `true`, confirm the
expected intent-tree / ADR / diagram / docs writeback landed in this PR or was
captured as a follow-up packet. If the packet declined all knowledge maintenance,
that is acceptable — note it rather than blocking.

本パケットは `write_back_required: true`。Intent Gate のコード化（型付きポリシーモジュール抽出、単一ソース化、Orchestrator prompt からの再分類指示削除）が `intents/evorch/features/orchestration/overview.md` の v0.2 確定節に実装完了として反映されていること、またはフォローアップパケットとして捕捉されていることを確認する。
