# v02-intent-gate-rules Implementation Packet

## Goal

Intent Gate の分類ルールを `crates/runtime/src/prompt/intent_gate.rs:15-48` の固定文字列から型付きポリシーモジュールへ抽出し、`render_intent_gate` をそのモジュールからのレンダリングに変更する。コードと prompt が参照する単一の正本を作り、後続の entry pre-routing（Layer A）が同じルールをプログラム的に扱えるようにする。

## Why

v02-prompt-assembly（PR #50）で prompt 組立が決定論的に固定されたが、分類ルール自体はまだ文字列リテラルの中にしかない。Layer A の routing prompt が同じルールを必要とする段階で二重管理になるのを防ぐため、先に型として単一ソース化する。

## Scope

- `crates/runtime` へのポリシーモジュール新設（`ExecutionShape`、8 分類軸、タスク種別判定表、mutation 非持越ルール）。
- `render_intent_gate` のポリシーモジュール由来レンダリング化。
- routing prompt（Layer A・将来）と Orchestrator prompt の両方を生成できる API の公開。
- Orchestrator 向け gate 本文の再分類指示を削除し、選択結果の検証を説明する記述への置換。

## Out of scope

- Layer A routing prompt 本体と pre-routing 実行経路。
- `crates/routing` への型配置・routing ロジック。
- GUI / CLI entry の変更。
- prompt assembly の順序変更。

## Implementation Steps

1. ポリシーモジュールを `crates/runtime/src/prompt/` 配下に新設する（例: `intent_gate_policy.rs`）。以下を型として定義する。
   - `ExecutionShape`（Direct / Coordinated）の enum。
   - 8 分類軸（タスク種別・必要ケイパビリティ・変異可否・スコープ・不確実性・期待する出力・完了条件・委譲要否）を表す型。
   - タスク種別の判定表（explain / implement / look into / broken / refactor）を表す型。
   - mutation 非持越ルール（直前ターンの変異許可は持ち越さず、メッセージごとに独立判定する）を表す型。
   - すべての型は決定論的にレンダリングできる順序（定義順の静的配列など）で保持し、HashMap の反復など順序が揺れる構造を使わない。
2. `crates/runtime/src/prompt.rs` でポリシーモジュールを公開し、コンシューマが型に到達できるようにする。
3. `render_intent_gate`（`crates/runtime/src/prompt/intent_gate.rs`）を、`GATE_BODY:15-48` の固定文字列連結から、ポリシーモジュールの型定義を走査して prompt 文字列を生成する形へ変更する。keyTriggers 埋め込み（BEGIN/END マーカー）の構造は維持する。
4. ポリシーモジュールに 2 consumer 向けのレンダリング API を公開する。routing prompt 用（Layer A・将来の consumer）と Orchestrator prompt 用（既存の `render_intent_gate` 経路）で同じ型定義から生成できること。
5. Orchestrator 向け gate 本文から「応答の前に現在のメッセージを 8 項目で分類すること」に相当する再分類指示を削除し、entry pre-routing が選択した結果を Orchestrator が検証する説明に置き換える。検証のフレーミングは残す。
6. `crates/runtime/src/prompt/assembly.rs:34-55` の組立順と挿入条件（`Role::Orchestrator` のみ Intent Gate を挿入）は変更しない。
7. PR #50 由来の byte-identical golden test（同一入力でバイト一致する決定論性テスト）が引き続き pass することを確認する。レンダリングの決定論性を壊す変更は禁止。gate 本文の文言変更に伴い #49 由来の golden 文字列の更新が必要な場合は、差分の理由を PR 本文に明記する。

## Verification

- `cargo test`（v02-prompt-assembly 由来の byte-identical golden test を含む）が pass すること。
- `cargo clippy -- -D warnings` が pass すること。
- `cargo fmt --check` が pass すること。
- `git diff --check` が pass すること。

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: `intents/evorch/features/orchestration/overview.md`（primary）。v0.2 確定節の pre-routing / escalation の基盤となる型抽出であり、新規 intent node は不要。`intents/evorch/decisions/0001-no-fixed-workflow.md` と `intents/evorch/interviews/new-session.json` を supporting とする。
- ADR candidate: none。
- Diagram candidate: none。
- Docs update: none。
- Closeout learning: Intent Gate のコード化（型付きポリシーモジュール抽出、単一ソース化、Orchestrator prompt からの再分類指示削除）が orchestration overview の v0.2 確定節に実装完了として反映されること。`write_back_required: true`、write-back target は `intents/evorch/features/orchestration/overview.md`。

- Guide reachability (G645): role-facing surface は system prompt assembly（Orchestrator）。guide surface は orchestration overview の v0.2 Intent Gate / pre-routing surface、role は Orchestrator、target surface は system prompt assembly。

`improve` (G456 / G460) is the later safety net; packet-time maintenance is the normal path.
