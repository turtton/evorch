# ADR 0017: Event Bus の transport を in-process tokio broadcast に確定する

## Status

Accepted（2026-08-29、PR #11 で実装確定。issue #2 / v01-event-stream）

## Context

architecture.md の Open question「Event Bus の transport 実装」が未解決だった。v0.1 の全購読者（gui-panes / session-storage / metrics 集計）は同一プロセス内の Tokio runtime 上に存在し、ADR 0012 は「raw はメモリ保持・downsampled のみ永続化」を定めている。分散 transport の有無は購読 API とイベント schema の設計に直接効くため、最初に固定する必要があった。

## Decision

### transport

v0.1 の transport は **in-process の tokio broadcast 固定**とする（`crates/event-bus/` の `EventBus`）。

- capacity は利用側が指定（tokio は 2 の冪に切り上げ）
- 受信者が追従できない場合は tokio broadcast 標準の「最古から上書き drop」。受信者は `Lagged(n)` で取りこぼし件数を検知
- slow-consumer 検知: `recv()` が `Lagged(n)` を返すたびに `tracing::warn!`（subscriber_id / skipped 付き）。さらに lag エピソードごとに 1 回 `FaultEvent::SubscriberLagged` をバスへ emit し、他の購読者（GUI / 診断）が slow consumer の存在を観測可能にする
- fault 再 emit は **1 エピソード 1 回に抑制**する（`fault_suppressed` フラグ）。無条件再 emit は fault 自身が受信者の再配置先を押し出して無限ループになることが判明しているため

### 将来の分散化への接続方針

- Event schema は serde 隣接タグ（`{"kind","payload"}`）+ `schema_version` で自己記述的であり、wire format として外部 transport に流せる形に既に揃っている
- 将来の分散化は「bus の上流に gateway subscriber を 1 つ置き、serde_json 化して外部 transport へ bridge する」形で接続し、**イベント型自体と購読 API（`EventReceiver::recv`）は変更しない**
- 分散 transport（外部ブローカ等）は観測レイテンシ・運用コスト・順序保証の複雑化を招くため、単一プロセス構成が揺らぐまでは導入しない

### schema versioning

`EventMeta.schema_version`（現在 1）が拡張の単一ゲート。新カテゴリ・新 variant の追加は後方互換（既存 reader は無視可能）として進め、破壊的変更が必要になった時点でバージョンを上げ、旧バージョン event の受け入れ期間を設ける。

## Consequences

- architecture.md の transport Open question は解決
- GUI（#9 gui-panes）・永続化（#3 session-storage）・計測（ADR 0012）は `EventReceiver` で購読する共通 API に乗る
- 購読 API は将来の分散化でも不変 — gateway bridge 方式の前提は、versioning された不変 schema が存在すること
