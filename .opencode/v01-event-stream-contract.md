# v01-event-stream 実装契約（lead → worker）

- **lead**: opencode pane `w2:p1`（host repo cwd）。**すべてのリレーは `w2:p1` 固定**。他の opencode pane が見えても送り先を変えない。
- **対象 issue**: turtton/evorch **#2**（`gh issue view 2` で全文を読むこと。本契約は要点と追加制約のみを記す）
- **作業ディレクトリ**: 現在の cwd（worktree `.worktrees/v01-event-stream`、branch `v01-event-stream`、base = origin/main。v01-scaffold 完了済みで workspace はビルド可能）
- **intent-cli 絶対パス**: `/nix/store/bhn4d645q3f0nc7m7k0iw20phizrsw5i-intent-cli-0.26.0/bin/intent-cli`

## Task

issue #2 に従い `crates/event-bus/` に型付き Event Bus を実装する:

- `Event` enum（serde 直列化可能）: lifecycle / message / tool / usage / provider request-response / fault の各カテゴリ。agent-runtime-kernel/overview.md の runtime event 一覧（Started / MessageDelta / ReasoningDelta / ToolStarted / ToolCompleted / Delegated / BackgroundTaskStarted / BackgroundTaskCompleted / Usage / CacheStats / ProviderFallback / Completed / Failed）を出発点にする
- timestamp は monotonic + wall-clock 両方（token usage / TTFT 計測用）
- schema version フィールド（将来の event 拡張用）
- `EventBus`（tokio broadcast ベース）: emit / 複数 subscriber / capacity 上限
- subscriber lag 時: drop ポリシー定義 + slow-consumer 検知（`Lagged(n)` → `tracing::warn` + fault event emit）
- `RingBuffer<T>`（ADR 0012 の bounded バッファ、最古 drop）
- usage event から 1 分バケット per provider/model の in-memory 集計モジュール（最小）+ storage single-writer への受け渡しインターフェース土台
- 依存（tokio / serde / tracing 等）は `[workspace.dependencies]` に集約してから各 crate から参照する
- 全 event 型の serde round-trip テスト、複数 subscriber 受信テスト、lag 検知テスト

## Hard constraints

- **`.intent-cli/` と `intents/` には一切触れない**（host state。ADR 0017「event-bus-transport」と `intents/evorch/technology/architecture.md` の transport Open question 更新は lead が closeout 時に実施する）
- 代わりに **PR body と完了リレーに以下を必ず含める**（lead が ADR 0017 writeback に使う）:
  - transport 決定の根拠（in-process tokio broadcast 固定の理由・分散 transport を将来どう接続するかの方針・schema versioning との関係）
  - 最終 `Event` enum のカテゴリ一覧と主要 variant
  - lag ポリシーの要点（capacity 値・drop 方針・slow-consumer 検知の実装方法）
- workflow ラベルの付け替えは intent-cli 経由のみ
- **CI green を確認してから完了リレー**（`gh pr checks <n>`）

## Flow

1. `gh issue view 2` で issue 全文を読む
2. 実装 → ローカル検証（`cargo build / test / clippy / fmt --check`）→ commit（適宜分割可）→ push（branch `v01-event-stream`）
3. `gh pr create --base main --head v01-event-stream`（**draft にしない**）。PR body に acceptance evidence（実行コマンドと結果）+ 上記の writeback 用情報を書く
4. CI green 確認
5. intent-cli（上記絶対パス）で `worker next-action --repo turtton/evorch --workdir . --format json` を実行し、その指示に従い `worker claim` → `worker result-summary` → `worker complete` まで進める
6. 完了リレー: `relay="[herdr-relay] v01-event-stream: 完了 PR#<n> <要約>"; herdr agent prompt w2:p1 "$relay"`（`$` や backtick はシェル変数経由）

## 着手時リレー（必須）

作業開始時に最初に以下を送り、配線を確認する:

```text
herdr agent prompt w2:p1 "[herdr-relay] v01-event-stream: 着手 (送り先 w2:p1 を確認)"
```

## sandbox で commit/push できない場合（bundle 運用）

worker sandbox が `.git` を read-only mount していて commit/push が失敗する場合**のみ**:

1. writable な copy gitdir を用意する（例: `git clone <repo-url> <copy-dir>`）し、そこで commit する
2. `git bundle create <checkout>/v01-event-stream.bundle v01-event-stream`（bundle は checkout ディレクトリ内に置く。`/tmp` は host から見えない）
3. リレーで bundle のパスと branch 名を伝える（lead が fetch → push → PR 作成を代行する）

## その他

- crates.io からの依存 fetch や `nix develop` が sandbox 内で失敗する場合は、無理をせず PR body に「host 側検証必要」と明記する（lead が host で確認する）
- 不明点・阻塞まった場合は `[herdr-relay]` で lead に質問する（`w2:p1` 固定）
