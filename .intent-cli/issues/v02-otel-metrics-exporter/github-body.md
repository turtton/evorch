## Goal

内部ドメイン event を、OTel gen_ai semantic conventions 標準（`gen_ai.*`）＋ `evorch.*` 拡張属性へ一義的に変換する専任写像層（crates/event-bus 内の新規 module）と、その写像層経由の metrics OTLP exporter を実装する。ADR 0023 の二段分割（決定 5）における slice ①。`otel-exporter` feature は feature-gated opt-in（既定 off）として新規導入する。

## Why This Slice Exists Now

mvp-roadmap v0.2「役割の深化と観測」の入口として、OTLP exporter（metrics）が未着手のまま残っている。ADR 0023 で export 方針（gen_ai 部分採用＋ `evorch.*` 拡張、metrics + span、写像層集中、`evorch.*` 最小軸、exporter 二段分割、semconv v1.37.0 pin＋意図的 bump、golden test 主軸検証）が確定したため、まず写像層と metrics exporter を固定し、token / cost / latency を早期に collector へ出せる状態にする。観測は自己改善機能の分析基盤でもあり（ADR 0023 Context / Consequences）、span 親子紐付けは slice ②（`v02-otel-span-exporter`）に切り分けて集中させる。

## Current Observed State

- `crates/event-bus/` に型付き event schema と tokio broadcast ベースの bus が landed 済み。provider 観測 event（request 開始 / response 完了 / usage 計測の観測点）は PR #32 で schema に追加済み。
- event bus の transport は in-process tokio broadcast に確定済み（ADR 0017）。ADR 0012 で計測アーキテクチャ（OTel Metrics API 語彙、raw 非永続、downsampled 保存、任意 OTLP export）は決定済みだが、opentelemetry / OTLP exporter 依存は未導入。
- OTel 標準属性への変換写像層は未存在で、OTLP exporter 本体は未実装（mvp-roadmap v0.2「役割の深化と観測」の未着手項目）。
- gen_ai semconv への採用方針は ADR 0023 で確定済みだが、コード上の属性規約（写像表・whitelist）は未固定。

## Accepted Baseline You May Assume

- 型付き event bus（tokio broadcast、in-process、ADR 0017）が `crates/event-bus/` に存在し、複数 subscriber へ event を配信できる。内部 event schema はドメイン語彙を保持する（ADR 0023 決定 3）。
- ADR 0012 の計測アーキテクチャ（OTel Metrics API 語彙・raw 非永続・downsampled 保存・任意 OTLP export）を前提とする。opentelemetry / opentelemetry-otlp 依存は本 slice で `otel-exporter` feature 配下に新規導入する。
- gen_ai semconv は v1.37.0 に pin。自動追従しない（ADR 0023 決定 6）。
- ADR 0023 の 7 決定（採用ポリシー / signal 範囲 / 写像層集中 / `evorch.*` 骨格 / slice 二段分割 / semconv 追従 / 検証）は実装判断の Source of Truth として確定済み。

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/event-bus/`

Target part: 写像層（crates/event-bus 内の新規 module。domain event → OTel 属性変換）と metrics OTLP exporter（feature-gated opt-in、既定 off として新規導入）。crate 分割が必要になった場合は ADR 0016 の crate 粒度基準に従う。

## In Scope

- 内部ドメイン event → `gen_ai.*` 標準属性への変換写像層（crates/event-bus 内の新規 module）。
- `evorch.*` 拡張属性: 集計軸 session / task / agent_run / profile ＋ 最小構造軸 `evorch.delegation.depth` / `evorch.delegation.role`（低カーディナリティ列挙値のみ）。
- producer 非汚染の構造（provider / runtime / tool 層が opentelemetry crate を直接参照しない）。
- metrics OTLP exporter: opentelemetry SDK / OTLP exporter 依存を `otel-exporter` feature gate（opt-in、既定 off）配下で新規導入し、写像層経由メトリクスを export。
- 検証: mapping 層 golden/snapshot test（pin 固定 fixture）、CI cardinality guard（metrics attribute whitelist 強制）、debug exporter 経由の最小 OTLP E2E 1 本。
- 同梱 writeback: ADR 0023 決定 4 の `evorch.*` whitelist 確定反映、mvp-roadmap.md の v0.2「役割の深化と観測」への着手・完了反映。

## Out Of Scope

- span / trace 実装、span 属性、委譲親子 = span 親子の紐付け、sampling hook 制御（slice ② `v02-otel-span-exporter`）。
- raw LLM I/O log / SSE body / message 本文の export（ADR 0012 の raw 非永続ポリシーに基づく恒久対象外）。
- cost inspector の計算ロジック変更。
- GUI / TUI surface の追加・変更（本 slice は内部写像層と exporter のみ）。
- gen_ai semconv pin（v1.37.0）からの bump。
- `otel-exporter` feature の既定 on 化・強制有効化。

## Standalone Child Issue Contract

`turtton/evorch` の `crates/event-bus/` に、内部ドメイン event を OTel gen_ai semconv（v1.37.0 pin）標準属性＋ `evorch.*` 拡張属性（集計軸 session / task / agent_run / profile、構造軸は `evorch.delegation.depth` / `evorch.delegation.role` の低カーディナリティ限定）へ一義的に変換する写像層 module を実装し、provider / runtime / tool 各 producer が opentelemetry crate を直接参照しない構造を担保した上で、その写像層経由の metrics OTLP exporter を `otel-exporter` feature（feature-gated opt-in、既定 off）として新規導入する。検証として、pin 固定 fixture による mapping 層 golden/snapshot test、metrics attribute whitelist を静的に強制する CI cardinality guard、debug exporter 経由の最小 OTLP E2E 1 本を入れる。さらに、写像層で確定した `evorch.*` 最終 whitelist を ADR 0023 決定 4 に確定反映して ADR を確定版にし、mvp-roadmap.md の v0.2「役割の深化と観測」に着手・完了を反映する変更を同梱する。

## Acceptance Criteria

- 写像層が内部ドメイン event を `gen_ai.*` 標準 ＋ `evorch.*` 拡張属性へ一義的に変換し、mapping 表が pin 対象 semconv リリース（v1.37.0）を明記する。
- producer 層が opentelemetry crate を直接参照していない。
- metrics attribute が whitelist（集計軸＋最小構造軸）にのみ収まり、ID 系属性・自由文字列が metric label に含まれない。
- golden/snapshot test ＋ CI cardinality guard ＋ debug exporter 経由の最小 OTLP E2E 1 本が入る。
- `otel-exporter` feature が feature-gated opt-in（既定 off）として新規導入され、feature off（既定）ビルドで既存の event 配信・bus 動作が回帰しない。
- ADR 0023 確定版（決定 4 の whitelist 更新）と mvp-roadmap.md の v0.2 反映更新が同梱される。

## Verification

- `cargo test -p event-bus`: 写像層 golden/snapshot 一致、whitelist 外属性の静的拒否、feature off 回帰なし。
- CI cardinality guard による metrics attribute whitelist 強制。
- 写像層 → OTel SDK exporter → debug exporter の JSON 出力 assertion による最小 OTLP E2E 1 本。
- `cargo clippy -p event-bus -- -D warnings` / `cargo fmt --check` / `git diff --check`。

## Related Links

- [diagnostics-self-improvement/overview.md](../../../intents/evorch/features/diagnostics-self-improvement/overview.md) — primary intent（観測は自己改善分析基盤）
- [gui-workbench/overview.md](../../../intents/evorch/features/gui-workbench/overview.md) — supporting（観測面の workbench）
- [mvp-roadmap.md](../../../intents/evorch/technology/mvp-roadmap.md) — v0.2「役割の深化と観測」の入口としての位置づけ
- [architecture.md](../../../intents/evorch/technology/architecture.md) — Agent Kernel 構成上の event bus / 計測の位置づけ
- [0012-metrics-architecture.md](../../../intents/evorch/decisions/0012-metrics-architecture.md) — OTel Metrics API 語彙 / raw 非永続 / 任意 OTLP export
- [0014-config-architecture.md](../../../intents/evorch/decisions/0014-config-architecture.md) — 設定構成（feature 有効化の経路）
- [0015-verification-two-layer.md](../../../intents/evorch/decisions/0015-verification-two-layer.md) — verification two-layer
- [0017-event-bus-transport.md](../../../intents/evorch/decisions/0017-event-bus-transport.md) — event bus transport（in-process broadcast）
- [0023-otel-attribute-conventions.md](../../../intents/evorch/decisions/0023-otel-attribute-conventions.md) — 採用ポリシーと 7 決定の Source of Truth

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs
writeback is expected for this slice. Answer or explicitly decline:

- Intent placement: primary = diagnostics-self-improvement/overview.md、supporting = mvp-roadmap.md / gui-workbench/overview.md。新規 intent node 不要、OTel 最上位 intent は作らない（再編可能性のみ残す、ADR 0023 記載）。
- ADR candidate: `intents/evorch/decisions/0023-otel-attribute-conventions.md` を確定版へ更新（決定 4 の `evorch.*` whitelist 最終反映）（必須）
- Diagram candidate: なし。
- Docs update: `intents/evorch/technology/mvp-roadmap.md` の v0.2「役割の深化と観測」への着手・完了反映（必須）
- Closeout writeback expected: yes（ADR 0023 確定版＋ mvp-roadmap.md 更新）。

## Guide Reachability (G645)

本 slice は内部 crate の写像層と opt-in 既定 off の exporter を追加するのみで、role-facing surface は追加しない（`no_role_facing_surface: true`）。

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
