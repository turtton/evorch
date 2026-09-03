# v02-entry-pre-routing Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

## Slice-specific review focus

- v02-intent-gate-rulesが提供する型付きポリシーモジュールを分類ルールの単一ソースとして参照しているか。runtimeコードに独自ハードコードしたルールセットや、prompt内にprompt-onlyの分類分岐を新設・残留させていないか
- 分類APIが`crates/runtime`に置かれ、GUI entry（`evorch-gui.rs`）がその消費者になっているか。gui-localに再実装されていないか。将来のCLI entryから再利用可能な形状か
- ローカルキーワード検出がomoのulwパターン（`/\b(ultrawork|ulw)\b/i`）をモデルにしたregexで、コードブロック・スラッシュコマンド・内部メッセージを除外しているか。word boundaryやcase insensitiveの仕様が欠けていないか
- 起動条件が「explicit directキーワード検出時のみWorker直接起動、通常メッセージはOrchestrator起動」になっているか。デフォルトをWorkerへ反転させていないか
- fail-safeの向きがOrchestrator側（Coordinated）へ倒れているか。判定不能・低確度・矛盾をWorker側へ流していないか
- フォールバック再分類にOrchestratorと同じモデルを使っているか。別モデル・別サービス分類器を新設していないか。2段階（ローカル→モデル再分類）を飛ばして直接モデル分類だけにしていないか
- RoutingDecision eventがshape、判定理由、使用ルールorモデルをpayloadに含み、既存Event Busへ発行されているか。event発行がunit testで検証されているか
- headlessテストがDirectキーワード有り（Worker起動）と無し（Orchestrator起動）の両ケースをカバーしているか。片方だけのhappy pathテストになっていないか
- `evorch-gui.rs`のRole::Orchestrator固定生成（旧246-255行相当）を削除・置換し、分類APIの結果に従う起動になっているか。古い固定生成経路がdead pathとして残っていないか
- 範囲外のmid-run escalation、新規CLI entry、手動shape選択UI、モデル変更を本sliceに混入させていないか。`crates/evorch/src/main.rs`が空のまま維持されているか

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

注: facet-checkはfail-safe向きや2段階分類の実装正しさを証明しない。unit/table-driven/headless testとreview focusを主たる判定にする。

## Knowledge Writeback Expectation (G461)

`closeout_learning.write_back_required`は`true`。以下を確認する。

- `intents/evorch/features/orchestration/overview.md`のv0.2確定節へ、entry pre-routing完了（ローカルキーワード＋同モデル再分類の2段階）の書き戻しが行われているか

writebackが行われず、かつfollow-up packetとして捕捉されていない場合は指摘する。
