# Self-Improvement 棚卸し (2026-09)

2026-09-21 棚卸し

ADR-0006 ([decisions/0006-self-improvement-and-diagnostics](../decisions/0006-self-improvement-and-diagnostics.md)) が掲げる目標:

> Harness 自身の不具合を外部に気づくまで放置すると、継続的に品質が低下する。dogfooding による自己改善 loop を作り、runtime fault を自動収集・Issue 化したい。

本ノートは現時点のスナップショットであり、実装計画を強制するものではない。

## 基盤として存在するもの

以下は 2026-09-21 のコード探索で実在を確認した項目。

- `crates/runtime/src/memory_lifecycle.rs:18-22`
  — `with_learning` post-run hook。`crates/runtime/src/runtime.rs:876-908` が run 終了後に `prepare_learning` + `learning.complete` を自動実行する。
- `crates/runtime/src/memory_queue.rs:74-151`
  — reviewer / interviewer / promotion の learning queue が既に稼働し、reviewer 承認済み lesson の promote まで処理する。
- `crates/gui/src/bin/evorch-gui.rs:739-751`
  — GUI が起動時に learning を配線する(quick route 未設定時は警告で無効化)。
- `crates/arena`
  — role evaluation 用の comparison / selection / promotion 等の基盤が存在する(詳細は次節参照)。

## 棚卸し依頼で列挙されたが、このリポジトリで確認できなかったもの

依頼文では「基盤として存在するもの」とされていたが、2026-09-21 の探索では `turtton/evorch` 内で実在を確認できなかった項目。誤記の可能性と他リポジトリ所在の可能性の両方があるため、存在扱いにはしない。

- `crates/config/src/extension_registry.rs` — 該当ファイルなし。`crates/config/src/` 配下に extension registry / stable_id / manifest drift 検出は見当たらない。
- `crates/arena` の DPO — arena 自体は存在するが、DPO (direct preference optimization) 相当のモジュール・シンボルは見当たらない。
- routing / model / catalog 公開型の `#[non_exhaustive]` — 該当属性は `crates/tools/src/result.rs` のみ。routing には builder の `finish_non_exhaustive()` 呼出しがあるのみで、公開型への属性付与は確認できない。
- sandbox preemption + baseline-refresh、および `.intent-cli/queue-state.preempted.json` — `crates/sandbox/src/` と `.intent-cli/` の双方に preempt / baseline 相当の痕跡なし。

## まだ存在しないもの (棚卸しにおける未達)

- learning record から新規 intent-cli issue / packet を自動生成する loop がない。現在の learn-loop は meta レベルで operator が起動する形であり、`.intent-cli/issues/*/packet.yaml` へ学習結果を promote する background harness は存在しない。
- learning からコード変更を提案する self-modification loop がない。arena の評価出力を routing / capability model へ自動フィードバックする経路も未整備。
- dogfooding の証跡が記録されていない。2026-09-12 の evorch-w2 worker restore クラッシュは dogfood 事例として self-improvement issue の種になるはずだが、issue 化されないまま残っている。

## 次のアクション候補

1. 「self-improvement loop」完了条件を定義する (どのシグナルで issue 自動生成を発火させるか)。
2. reviewer queue 溢れを `self-improvement` label 付き `.intent-cli/issues/` へ変換する最小 harness を実装する。
3. 2026-09-12 のクラッシュを最初の self-improvement issue として記録する。
4. 広い self-modification は条件 1 〜 3 が安定するまで延期する。
