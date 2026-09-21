---
name: test-policy
description: "flaky にならないテスト作成のためのプロジェクトポリシー。テスト追加・作成・修正時に必ず適用する。時間依存 (sleep / 固定 timeout) による待機を禁止し、イベント駆動の検知に統一する。trigger: テスト作成, テスト追加, test policy, flaky 修正, sleep, timeout, nextest, integration test"
---

# test-policy — evorch テスト作成ポリシー

このリポジトリでテストを**作成・修正するときは必ず本ポリシーを適用する**。
目的は flaky テストを構造的に生ませないことであり、事後のリトライや timeout 値の
調整で誤魔化さない。

## 実績のある事故例

`runtime::budget_checkpoint::checkpoints_continue_after_escalation_latches`
(251 回の実ツール呼び出しを伴う integration test) が、テスト内の固定
`timeout(10s)` を共有 CI ランナーの負荷変動で超過し、正常な実行が FAIL と
判定された (正常時 2.5s / 負荷時 10.3s)。本ポリシーはこの種の事故を防ぐ。

## 禁止事項

1. **テスト内の `tokio::time::sleep` / `std::thread::sleep` による待機** — 
   「十分待てば状態が揃うはず」は CI 負荷で必ず破綻する。
2. **完了検知を兼ねた固定 timeout** — `timeout(N秒, 完了future)` で
   「N 秒以内に終わらなければ失敗」は実行時間を正しさの条件に組み込む行為。
3. **成否判定への `Instant` / 実時刻の使用** — 実行時間は正しさの条件でない。

## 代わりに使う手段

- **意味のある完了シグナルを await する**: `runtime.wait(run)` のような
  ライフサイクル待機、EventBus の `recv()`、`tokio::sync::oneshot` /
  `Notify` / `Barrier` など、対象が明示的に発行するイベントで待つ。
- **終端イベントでループを抜ける**: 「これ以上来ない」ではなく
  「terminal event (AgentRunPhase::Done | Error 等) を受信した」と判定する。
- **時間自体を検証する必要がある場合は仮想時間**:
  `#[tokio::test(start_paused = true)]` + `tokio::time::advance()` で
  wall-clock に依存しない時間進行を使う。

## ハング検知はランナーの責務

テストがデッドロックした場合の打ち切りはテストコードではなく
nextest の `slow-timeout` / `terminate-after` (`.config/nextest.toml`) が担う。
現行設定: `period = "180s", terminate-after = 1`。
テスト個別に timeout を書いてはいけない — 全体で一元管理されるため、
正当に長時間走るテスト (大量の実呼び出しを伴う integration test) が
ランナー負荷で誤判定されることを防げる。

## 例外的に時間待機を許容するケース

対象が**シグナルを一切発行できない**外部プロセス・ファイル監視などで
イベント駆動が物理的に不可能な場合のみ、ポーリング+余裕十分な期限を
許容する。その場合はテストコメントに「なぜイベント駆動でないか」の
根拠を書くこと。可能なら対象に通知機構を追加する側の変更を優先する。

## レビュー時のチェックリスト

- [ ] sleep / 固定 timeout / 実時刻がテストコードに含まれていないか
- [ ] 完了検知は対象からの明示的イベントか
- [ ] 「これ以上イベントが来ない」を timeout で判定していないか
- [ ] CI (共有ランナー) で正常系が 10 倍遅くなっても安定して通る設計か
