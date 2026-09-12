## Goal

run をバックグラウンドで fire-and-forget 実行し、完了・失敗・承認待ちを event 駆動の
通知として GUI に surface するモデルを実装する。orchestrator/ユーザーはポーリングせず
通知で起きる。

## Why This Slice Exists Now

2026-09-12 の wave で実証: 最大5タスクの並列 background 実行 + 完了 notification で
orchestrator がポーリングなしに捌けた。ポーリングモデルはトークン効率・応答性ともに劣る。
現状 evorch の GUI はフォアグラウンドの chat 送信が中心で、複数 run の並行状態を
一覧する surface が薄く、バックグラウンド実行の結果を受け取る経路がない。

## Current Observed State

- EventBus（crates/event-bus）は schema_version 付き envelope で RequestCompleted 等の
  run 終端 event が既存。通知の原料は揃っている。
- F8 の AttentionAck モデル（crates/gui/src/model/ack.rs、未読=強調・既読=枠のみ、
  displayed+focused+revision 一致で ack）が既読管理の基盤として存在する。
- 複数 run の並行状態（実行中/待機/完了/失敗）を一覧する GUI surface はない。

## Accepted Baseline You May Assume

- EventBus / run 終端 event / AttentionAck モデルは実装済み（ADR 0017）。
- v07-e2-team-mode の team pane 計画と表示を統合できると望ましいが、本 unit は
  単発 run の通知モデルを先に成立させる。

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/runtime/ crates/event-bus/ crates/gui/`

Target part: background run 実行モデル（fire-and-forget 起動）+ event 駆動通知
（完了/失敗/承認待ち）+ GUI 通知 surface（ack 連動・run 一覧）

## In Scope

- run をバックグラウンドで開始でき、開始元の chat/GUI 操作をブロックしない起動経路
- run の完了・失敗・承認待ちを通知として未読強調表示し、表示+focus で既読になる
  通知モデル（AttentionAck 再利用）
- 通知から該当 run の transcript へ1操作で遷移する導線
- 進行中 run の一覧と状態（実行中/待機/完了/失敗）の GUI surface
- 回帰テスト: 通知の未読/既読遷移、通知からの遷移、複数 run 同時進行時の表示

## Out Of Scope

- team pane との表示統合（v07-e2-team-mode の領域。本 unit は単発 run の通知を先に成立させる）
- 中間 event（進捗差分等）の通知粒度の拡張 — run 終端 event をまず成立させる
  （粒度の使い勝手は closeout learning で回収する）
- EventBus transport 自体の変更（ADR 0017 の既存 envelope を使う）

## Standalone Child Issue Contract

evorch に「run をバックグラウンドで fire-and-forget 実行する起動経路」と「run の
完了・失敗・承認待ちを event 駆動で GUI に通知する surface（未読強調・表示+focus で
既読・通知から transcript へ1操作遷移）」および「進行中 run の一覧と状態表示」を追加し、
通知の未読/既読遷移・遷移導線・複数 run 同時進行表示の回帰テストを添えて PR として
提出する。

## Acceptance Criteria

- run をバックグラウンドで開始でき、開始元の chat/GUI 操作をブロックしない
- run の完了・失敗・承認待ちが通知として未読強調表示され、表示+focus で既読になる
- 通知から該当 run の transcript へ1操作で遷移できる
- 進行中 run の一覧と状態（実行中/待機/完了/失敗）が GUI で確認できる
- 回帰テスト: 通知の未読/既読遷移、通知からの遷移、複数 run 同時進行時の表示

## Verification

- `cargo test -p runtime -p event-bus -p gui` 全緑
- 新規回帰テスト: 通知の未読/既読遷移、通知からの遷移、複数 run 同時進行時の表示
- `git diff --check`

## Related Links

- ADR 0017: intents/evorch/decisions/0017-event-bus-transport.md
- ADR 0025: intents/evorch/decisions/0025-agent-experience-evaluation.md
- intents/evorch/features/gui-workbench/overview.md
- intents/evorch/features/orchestration/overview.md

## Knowledge Maintenance

- Intent placement: gui-workbench overview（既存 intent、新規不要）
- ADR candidate: none
- Diagram candidate: none
- Docs update: none
- Closeout writeback expected: no

## Guide Reachability (G645)

- guide surface: `guide workflow task implementation-loop`（role: implementation）
- target surface: background run API + 通知 pane + ack 連動

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
