# v02-otel-metrics-exporter Implementation Packet

## Goal

内部ドメイン event（event bus 配布の provider 観測・usage 計測）を OTel gen_ai semantic conventions 標準（`gen_ai.*`）＋ `evorch.*` 拡張属性へ変換する専任写像層（crates/event-bus 内の新規 module）を実装し、その上に metrics OTLP exporter を載せる。ADR 0023 の二段分割（決定 5）における slice ① であり、span / trace は slice ②（`v02-otel-span-exporter`）に委ね、本 slice では手を出さない。`otel-exporter` feature は feature-gated opt-in（既定 off）として新規導入する。

## Why

mvp-roadmap v0.2「役割の深化と観測」の入口として、OTLP exporter（metrics）が未着手のまま残っている。観測は自己改善機能の分析基盤でもあり（ADR 0023 Context / Consequences: 委譲軸は diagnostics-self-improvement のローカル downsampled 集計にも供給される）、ADR 0023（grill session `metrics-attribute-semconv` の 7 決定）で gen_ai semconv 部分採用＋ `evorch.*` 拡張、signal は metrics + span、変換の写像層集中、`evorch.*` は集計軸＋最小構造軸のみ、exporter の二段分割、semconv v1.37.0 pin＋意図的 bump、golden test 主軸の検証、が確定した。写像層＋metrics exporter を先行させれば token / cost / latency が早期に collector へ出せ、span 親子紐付けという難所は slice ② に集中させられる。event 基盤（crates/event-bus の型付き event schema と tokio broadcast bus、ADR 0017 の in-process 確定、PR #32 の provider 観測 event）は既に landed しており、本 slice はその上の標準準拠レイヤーを固定する。opentelemetry crate 依存は未導入であり、本 slice で feature-gated に新規導入する。

## Scope

- 写像層（crates/event-bus 内の新規 module）:
  - 内部ドメイン event → `gen_ai.*` 標準属性への変換表（gen_ai semconv registry 準拠、対象リリース = v1.37.0 pin を明記）
  - `evorch.*` 拡張: 集計軸 session / task / agent_run / profile ＋ 最小構造軸 `evorch.delegation.depth` / `evorch.delegation.role`（低カーディナリティ列挙値のみ）
  - producer 非汚染の構造: provider / runtime / tool 各層は opentelemetry crate を直接参照せず、OTel 属性名の知識は写像層一箇所に閉じる
- metrics OTLP exporter:
  - opentelemetry SDK / OTLP exporter 依存を `otel-exporter` feature gate（opt-in、既定 off）配下で新規導入し、写像層経由のメトリクスを OTLP として export
  - feature off（既定）ビルドでは依存を引き込まず、既存の event 配信・bus 動作に回帰がないことを担保する
- crate 分割が必要になった場合は ADR 0016（crate 粒度）の基準に従う
- 検証（ADR 0023 決定 7）:
  - mapping 層の golden/snapshot test（pin 固定 fixture）
  - CI cardinality guard（metrics attribute whitelist 強制、ID 系属性・自由文字列の metrics 混入を静的ブロック）
  - debug exporter 経由の最小 OTLP E2E 1 本（wire 疎通の押さえ）
- knowledge writeback 同梱: ADR 0023 決定 4 の `evorch.*` whitelist を最終属性で確定反映＋ mvp-roadmap.md の v0.2「役割の深化と観測」への着手・完了反映

## Out of scope

- span / trace の実装、span 属性、委譲親子 = span 親子の紐付け（slice ② `v02-otel-span-exporter` の担当。slice ② は本 slice に依存する）
- raw LLM I/O log / SSE body / message 本文レベルの export（ADR 0012 の raw 非永続ポリシーにより恒久的に対象外）
- sampling hook・span 高頻度制御の実装（slice ② 側の関心事）
- cost inspector の計算ロジック変更
- GUI / TUI surface の追加・変更（本 slice は内部写像層と exporter のみ）
- gen_ai semconv pin（v1.37.0）からの bump。release 差分精読に基づく bump は slice ② 着手時または technology re-evaluation 連動の別手続き
- `otel-exporter` feature の既定 on 化・強制有効化

## Verification

- unit / golden test:
  - 写像層 fixture が固定属性集合（`gen_ai.*` 標準 + whitelist 内 `evorch.*`）に対し golden/snapshot 一致
  - whitelist 外属性（ID 系・自由文字列）を注入した fixture が cardinality guard で静的拒否される
  - `otel-exporter` feature off（既定）ビルドで既存 event 配信・bus 動作に回帰がないこと
- E2E: 写像層 → OTel SDK exporter → debug exporter の JSON 出力 assertion による最小 OTLP E2E 1 本
- `cargo test -p event-bus` / `cargo clippy -p event-bus -- -D warnings` / `cargo fmt --check` が通ること。`git diff --check`

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: primary は `intents/evorch/features/diagnostics-self-improvement/overview.md`（観測は自己改善分析基盤）、supporting は `intents/evorch/technology/mvp-roadmap.md`（v0.2「役割の深化と観測」）と `intents/evorch/features/gui-workbench/overview.md`。新規 intent node 不要。OTel 最上位 intent は作らず、将来の telemetry 再編可能性のみ残す（ADR 0023 記載）。
- ADR candidate: 必須。`intents/evorch/decisions/0023-otel-attribute-conventions.md`「OTel attribute 規約と写像層配置の採用」を本 PR で確定版に更新（決定 4 の `evorch.*` whitelist に最終属性を反映）。
- Diagram candidate: decline。写像層集中の構造と exporter 配置は ADR 0023 とコード＋golden fixture で記録済みであり、図化は不要。
- Docs update: `intents/evorch/technology/mvp-roadmap.md` の v0.2「役割の深化と観測」に OTLP exporter（metrics）の着手・完了を反映（必須）。
- Closeout learning: 写像層実装で確定した `evorch.*` 属性の最終 whitelist と pin 対象 semconv リリースを writeback 対象とし、`write_back_required: true`。

- Guide reachability (G645): 本 slice は内部 crate の写像層と opt-in 既定 off の exporter を追加するのみで、ユーザー / オペレータ向け等の role-facing surface を追加しない。`no_role_facing_surface: true` を明示する。

`improve` (G456 / G460) is the later safety net; packet-time maintenance is the normal path.
