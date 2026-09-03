# v02-entry-pre-routing Implementation Packet

## Goal

GUI entryのユーザーメッセージ到着時にpre-routingを実行し、ローカルキーワードルール（explicit directキーワード検出）でDirectと高確度判定できた場合のみWorker runを直接起動し、それ以外はOrchestrator runを起動する2段階のentry routingを実装する。ローカルで解決しない入力はOrchestratorと同じモデルによる再分類にフォールバックし、それでも不確かな場合はCoordinatedへfail-safeする。判定結果はRoutingDecision eventとしてEvent Busへ発行する。

## Why

現行の`crates/gui/src/bin/evorch-gui.rs:246-255`は`Role::Orchestrator`固定で`AgentRuntime::production`を生成しており、entry時点でのshape判定が存在しない。new-session.json interviewで、デフォルトOrchestrator起動、明示directキーワード時のみWorker起動、fail-safeはOrchestrator側、分類モデルはOrchestratorと同じ、ローカル高確度なら直接Worker起動する2段階方式が確定した。分類ルールの単一ソース化はv02-intent-gate-rulesで型付きポリシーモジュールとして提供される前提で、本sliceはその消費者かつLayer A（entry側）を実装する。実行中の昇格はLayer Bのv02-direct-escalation-handoffで扱う。

## Scope

- 分類APIを`crates/runtime`に実装する。GUI専用ではなく、将来のCLI entryからも再利用可能な配置にする。gui-local実装にしない
- ローカルキーワード検出器を実装する。omoのulwキーワード実装（正規表現`/\b(ultrawork|ulw)\b/i`）をモデルに、explicit directキーワード（例: direct / just）をword boundary・case insensitiveのregexで検出する
- 検出除外を実装する。コードブロック（フェンス内）、スラッシュコマンド（先頭`/`行）、内部メッセージ（システム注入等）内のキーワードはカウントしない
- 分類ルールの参照先はv02-intent-gate-rulesが提供する型付きポリシーモジュールとする。分類分岐をprompt記述に新設しない。単一ソースをコードとpromptの両方から消費する構造を壊さない
- 2段階判定: ローカルルールで高確度なDirectを確定した場合のみWorker runを`delegate_background(Role::Worker, ...)`で起動する。キーワード非検出はCoordinatedとしてOrchestrator runを起動する
- 判定不能・低確度・矛盾（例: 同一メッセージ内でdirect系キーワードと明示的なOrchestrator要求が共存）の場合は、起動予定と同じOrchestratorモデルによる再分類へフォールバックする。再分類でも確度が足りなければCoordinatedへ倒す
- GUI entry（`evorch-gui.rs`の`run()`、現行246-255行）を改修し、固定`Role::Orchestrator`生成を分類APIの結果で選択する形にする。入力メッセージを分類APIへ渡し、DirectならWorker、CoordinatedならOrchestratorでruntimeを生成・起動する
- RoutingDecision eventをEvent Busへ発行する。payloadはshape（Direct/Coordinated）、判定理由、使用したルールorモデル識別子を含む。観測経路は既存event bus（ToolStarted/Completedと同系）に乗せる
- GUI headlessモードで、Directキーワード有りでWorker run、無しでOrchestrator runが起動することを検証するテストを追加する

## Out of scope

- mid-run escalation、EscalationRequested event、Worker→Orchestrator昇格handoff。v02-direct-escalation-handoffの責務
- 新規evorch CLI binary/commandの作成。`crates/evorch/src/main.rs`は空のまま維持する
- 型付きポリシーモジュール本体の実装。v02-intent-gate-rulesの責務
- 手動shape選択UIやそのGUIコンポーネント
- 分類結果に応じたモデル・権限ポリシーの変更。本sliceはRole選択のみ
- Orchestrator内Intent Gateのprompt調整（再分類指示の削除等）。別slice

## Verification

- キーワード検出unit test: direct/just検出、フェンスコードブロック除外、スラッシュコマンド除外、内部メッセージ除外、大文字小文字、word boundary（ディレクトリ等の部分一致非検出）
- 2段階分類table-driven test: ローカル高確度確定、フォールバック発火、矛盾入力の扱い、再分類後も不確か→Coordinated fail-safe
- RoutingDecision event test: shape、理由、ルールorモデル識別子がpayloadへ含まれる
- GUI headless: キーワード有りでWorker run起動、キーワード無しでOrchestrator run起動
- promptへprompt-only分岐を残していないことの確認
- workspace全体の`cargo test` / `cargo clippy -- -D warnings` / `cargo fmt --check` / `git diff --check`

## Knowledge Maintenance (G461, optional)

Captured while the design context is fresh. Answer or explicitly decline:

- Intent placement: `features/orchestration/overview.md`をprimary intentとし、v0.2確定節へentry pre-routingの実装完了を反映する。新規intent nodeは不要
- ADR candidate: none。2段階pre-routing方式はADR 0001の動的topologyの内側の運用判断で、interviewで確定済み
- Diagram candidate: none
- Docs update: none
- Closeout learning: pre-routing判定が型付きポリシー＋ローカルキーワード＋同モデル再分類で実装され、GUIがCoordinatedのみOrchestratorを起動することの検証結果をwritebackする。`write_back_required: true`、targetは`features/orchestration/overview.md`

- Guide reachability (G645): GUI goal投入surfaceにpre-routing判定を追加する。route: GUI workbench goal投入（pre-routing判定でWorker/Orchestratorを選択）→ role Orchestrator → goal submission UI / run startup。`no_role_facing_surface: false`

`improve` (G456 / G460) is the later safety net; packet-time maintenance is the normal path.
