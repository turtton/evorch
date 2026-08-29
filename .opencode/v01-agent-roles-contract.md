# v01-agent-roles 実装契約（lead → worker）

- **lead**: opencode pane `w2:p1`（host repo cwd）。**すべてのリレーは `w2:p1` 固定**。他の opencode pane が見えても送り先を変えない。
- **対象 issue**: turtton/evorch **#7**（`gh issue view 7` で全文を読むこと。本契約は要点と追加制約のみを記す）
- **作業ディレクトリ**: 現在の cwd（worktree `.worktrees/v01-agent-roles`、branch `v01-agent-roles`、base = origin/main。v01-scaffold / event-stream / session-storage / provider-client / tool-layer / sandbox-approval 完了済み）
- **intent-cli 絶対パス**: `/nix/store/bhn4d645q3f0nc7m7k0iw20phizrsw5i-intent-cli-0.26.0/bin/intent-cli`

## Task

issue #7 に従い `crates/agents/` と `crates/runtime/` に role 実行 runtime を実装する:

- **crates/agents/**: Role 定義（Orchestrator / Explorer / Worker / Reviewer）と role ごとの許可 tool セット・network 扱いの capability 定義（ADR 0002 capability boundary を runtime レベルで強制。Orchestrator は mutation tool を持たない、Explorer は write/edit/delegate を持たない等）。Librarian / Oracle を role 定義追加だけで載せられる拡張構造
- **crates/runtime/**: AgentRun の実行管理（Tokio task）、event-sourced 状態遷移（pending / running / waiting / done / error）、independent agent contexts、background agent（delegate_background / send_message / wait / cancel）、role → execution policy の適用
- AgentRun の状態遷移を `event_bus` crate の EventBus へ emit（v01-event-stream 完了済みの型を利用）
- background agent の開始・完了・キャンセルの event 観測
- 複数 AgentRun の同時並行動作（各 run が独立 context）
- 新規依存は `[workspace.dependencies]` に集約

## Hard constraints

- **`.intent-cli/` と `intents/` には一切触れない**（host state。`features/agent-runtime-kernel`（primary）＋ `features/orchestration` への writeback は lead が closeout 時に実施する。本 slice の ADR candidate は none — ADR 0002 で確定済み）
- 代わりに **PR body と完了リレーに以下を必ず含める**（lead が writeback に使う）:
  - Role / capability 定義の表面（Role 型・Capability 構造・許可 tool セットの表現）
  - 4 role の capability boundary の表（どの role がどの tool / network を許可・拒否）
  - AgentRun の状態遷移モデル（event-sourced 遷移の emit 設計）
  - background agent API の表面（delegate_background / send_message / wait / cancel のシグネチャ）
  - independent context の実現方法（各 run の context 独立性の保証）
  - role → model routing を v01-routing-profiles に委譲するための境界（provider 呼び出しを避ける抽象）
  - 新規依存の一覧と採用理由
- workflow ラベルの付け替えは intent-cli 経由のみ
- **CI green を確認してから完了リレー**（`gh pr checks <n>`）

## Flow

1. `gh issue view 7` で issue 全文を読む
2. 実装 → ローカル検証（`cargo build / test / clippy / fmt --check`）→ commit（適宜分割可）→ push（branch `v01-agent-roles`）
3. `gh pr create --base main --head v01-agent-roles`（**draft にしない**）。PR body に acceptance evidence（実行コマンドと結果）+ 上記の writeback 用情報を書く
4. CI green 確認
5. intent-cli（上記絶対パス）で `worker next-action --repo turtton/evorch --workdir . --format json` を実行し、その指示に従い `worker claim` → `worker result-summary` → `worker complete` まで進める
6. 完了リレー: `relay="[herdr-relay] v01-agent-roles: 完了 PR#<n> <要約>"; herdr agent prompt w2:p1 "$relay"`（`$` や backtick はシェル変数経由）

## 着手時リレー（必須）

作業開始時に最初に以下を送り、配線を確認する:

```text
herdr agent prompt w2:p1 "[herdr-relay] v01-agent-roles: 着手 (送り先 w2:p1 を確認)"
```

## sandbox で commit/push できない場合（bundle 運用）

worker sandbox が `.git` を read-only mount していて commit/push が失敗する場合**のみ**:

1. writable な copy gitdir を用意する（例: `git clone <repo-url> <copy-dir>`）し、そこで commit する
2. `git bundle create <checkout>/v01-agent-roles.bundle v01-agent-roles`（bundle は checkout ディレクトリ内に置く。`/tmp` は host から見えない）
3. リレーで bundle のパスと branch 名を伝える（lead が fetch → push → PR 作成を代行する）

## その他

- 不明点・阻塞まった場合は `[herdr-relay]` で lead に質問する（`w2:p1` 固定）
