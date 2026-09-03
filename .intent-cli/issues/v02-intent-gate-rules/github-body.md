## Goal

Intent Gate の分類ルール（8 分類軸・Direct/Coordinated 判定・mutation 非持越ルール）を、`crates/runtime/src/prompt/intent_gate.rs:15-48` の固定プロンプト文字列から型付きポリシーモジュールへ抽出し、`render_intent_gate` をそのモジュールからレンダリングする形へ変更する。コードと prompt の両方が参照する単一の正本を作る基盤 slice であり、prompt 組立の振る舞いは変えない。

## Why This Slice Exists Now

v02-prompt-assembly（PR #50）でシステムプロンプトの組立順が決定論的に固定された。次の段階である entry pre-routing（Layer A）では、分類結果を routing 側がプログラム的に扱う必要があるが、現状の分類ルールは prompt 文字列リテラルの中にしか存在せず、コードから参照できない。固定ワークフローを敷かないという方針（decisions/0001）のもとでルールを型として単一ソース化しておくことが、後続 slice の前提になる。

## Current Observed State

- Intent Gate は `crates/runtime/src/prompt/intent_gate.rs:15-48` の `GATE_BODY` 固定文字列としてのみ存在する。`ExecutionShape`（Direct / Coordinated）や 8 分類軸を表すコード型はない。
- `render_intent_gate(triggers: &[TriggerSource]) -> String` が唯一の public API で、`Role::Orchestrator` の場合のみ assembly に追加される（`crates/runtime/src/prompt/assembly.rs:34-55`）。
- gate 本文は Orchestrator 自身に「応答の前に現在のメッセージを 8 項目で分類すること」と再分類を指示する記述になっており、entry pre-routing 導入後は役割が重複する。
- `crates/routing` には `ExecutionShape` 型が存在しない。
- GUI entry は `crates/gui/src/bin/evorch-gui.rs:246-255` で `Role::Orchestrator` を固定指定して起動し、CLI は `crates/evorch/src/main.rs` が空実装。

## Accepted Baseline You May Assume

- Intent Gate は `crates/runtime/src/prompt/intent_gate.rs:15-48` の固定プロンプト文字列としてのみ存在し、ExecutionShape や分類軸を表すコード型はない。
- `render_intent_gate(triggers: &[TriggerSource]) -> String` が唯一の public API で、`Role::Orchestrator` の場合のみ assembly に追加される（`crates/runtime/src/prompt/assembly.rs:34-55`）。
- prompt assembly 順序は role baseline → family → category overlay → Intent Gate → appendix で deterministic に固定されている（v02-prompt-assembly 完了済み、PR #50）。
- routing crate（`crates/routing`）に ExecutionShape 型は存在しない。
- GUI entry は `crates/gui/src/bin/evorch-gui.rs:246-255` で `Role::Orchestrator` を固定指定して起動し、CLI は `crates/evorch/src/main.rs` が空実装。

## Target Repo / Path / Part

Repository: `turtton/evorch`

- Target paths: `crates/runtime/`

Target part: Intent Gate の 8 分類軸・Direct/Coordinated 判定・mutation 非持越ルールを prompt 内固定文字列から型付きポリシーモジュールへ抽出し、prompt 生成をそのモジュールからレンダリングする基盤 slice。出力を変えない golden test で振る舞いを固定する。

## In Scope

- `crates/runtime` へのポリシーモジュール新設（`ExecutionShape`、8 分類軸、タスク種別判定表、mutation 非持越ルールの型定義）。
- `render_intent_gate` をポリシーモジュールから prompt 文字列をレンダリングする形へ変更。
- routing prompt（Layer A・将来の consumer）と Orchestrator prompt（既存 consumer）の両方を生成できる API の公開。
- Orchestrator 向け gate 本文から再分類指示を削除し、entry pre-routing の選択結果を検証する説明への置換（検証のフレーミングは維持）。
- keyTriggers 埋め込みと assembly との接続（`Role::Orchestrator` のみ挿入）の維持。

## Out Of Scope

- Layer A routing prompt の実本体と pre-routing 実行経路の実装（後続 slice）。
- `crates/routing` 側への型配置・routing ロジック本体。
- GUI / CLI entry の変更。
- prompt assembly の順序変更（role baseline → family → category overlay → Intent Gate → appendix は固定）。

## Standalone Child Issue Contract

`crates/runtime` に、Intent Gate の分類ルール（Direct / Coordinated の `ExecutionShape`、8 分類軸、mutation 非持越ルール）を表す型付きポリシーモジュールを新設し、コードと prompt の両方から参照される単一の正本にする。`render_intent_gate` はこのモジュールから prompt 文字列をレンダリングする形に変更し、既存の keyTriggers 埋め込みと Orchestrator 限定挿入の振る舞いは維持する。ポリシーモジュールは routing prompt（将来の Layer A）と Orchestrator prompt の両方を生成できる API を公開する。Orchestrator 向け gate 本文の「応答前に自分で 8 項目を分類する」再分類指示は削除し、pre-routing の選択結果を検証する説明に置き換える。PR #50 由来の byte-identical な決定論性は壊さず、`cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check` がすべて pass すること。

## Acceptance Criteria

- ExecutionShape（Direct / Coordinated）、8 分類軸、mutation 非持越ルールを表す型付きポリシーモジュールが `crates/runtime` に存在し、単一の正本としてコードと prompt の両方から参照される。
- `render_intent_gate()` がポリシーモジュールから prompt 文字列をレンダリングする形に変更され、既存の byte-identical golden test（v02-prompt-assembly 由来）が pass する。
- ポリシーモジュールは routing prompt（Layer A 用）と Orchestrator prompt（既存用途）の両方を生成できる API を公開する。
- Orchestrator prompt 内の再分類指示が削除され、選択結果を検証する説明に置き換わる（entry pre-routing との役割分担のため）。
- `cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check` が pass する。

## Verification

- `cargo test`（v02-prompt-assembly 由来の byte-identical golden test を含む）が pass すること。
- `cargo clippy -- -D warnings`、`cargo fmt --check`、`git diff --check` が pass すること。

## Related Links

- intents/evorch/features/orchestration/overview.md
- intents/evorch/decisions/0001-no-fixed-workflow.md
- intents/evorch/interviews/new-session.json
- crates/runtime/src/prompt/intent_gate.rs
- crates/runtime/src/prompt/assembly.rs

## Knowledge Maintenance

Optional (G461). Tells the implementer/reviewer whether intent / ADR / diagram / docs
writeback is expected for this slice. Answer or explicitly decline:

- Intent placement: `intents/evorch/features/orchestration/overview.md`（primary）。v0.2 確定節の pre-routing / escalation に接続済みで、本 slice はその基盤となる型抽出。新規 intent node は不要。
- ADR candidate: none
- Diagram candidate: none
- Docs update: none
- Closeout writeback expected: yes（orchestration overview の v0.2 確定節に、型付きポリシーモジュール化・golden test 固定・単一ソース化が実装完了として反映されること。`write_back_required: true`）

## Guide Reachability (G645)

While the author still knows the answer, name the guide surface and role that route to every
role-facing surface this slice adds, or explicitly say that no role-facing surface is added. A
blank answer is not treated as no-surface. The closeout record is a debt check, not a merge gate.

- guide surface: orchestration overview の v0.2 Intent Gate / pre-routing surface
- role: Orchestrator
- target surface: system prompt assembly

## Base Branch Policy

Policy: `direct-main`
Expected PR base branch: `main`

Open all child PRs against `main` directly.
