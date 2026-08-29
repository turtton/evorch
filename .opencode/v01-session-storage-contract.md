# v01-session-storage 実装契約（lead → worker）

- **lead**: opencode pane `w2:p1`（host repo cwd）。**すべてのリレーは `w2:p1` 固定**。他の opencode pane が見えても送り先を変えない。
- **対象 issue**: turtton/evorch **#3**（`gh issue view 3` で全文を読むこと。本契約は要点と追加制約のみを記す）
- **作業ディレクトリ**: 現在の cwd（worktree `.worktrees/v01-session-storage`、branch `v01-session-storage`、base = origin/main。v01-scaffold / v01-event-stream 完了済みで workspace はビルド可能）
- **intent-cli 絶対パス**: `/nix/store/bhn4d645q3f0nc7m7k0iw20phizrsw5i-intent-cli-0.26.0/bin/intent-cli`

## Task

issue #3 に従い `crates/storage/` に SQLite 永続化層を実装する（**rusqlite 採用は packet 確定済み**）:

- rusqlite ベースのアクセス層。新規依存は `[workspace.dependencies]` に集約（rusqlite は bundled feature 等の必要 feature を適宜指定）
- version 管理された migration（起動時適用）で `sessions` / `tasks` / `messages` / `agent_runs` / `events` / `downsampled_metrics` を作成
- session / task / message / agent_run / event の CRUD
- events からの session 復元（state projection、再起動後 resume）
- ADR 0012 運用: PRAGMA（WAL / synchronous=NORMAL / wal_autocheckpoint）/ 定期 PASSIVE checkpoint / ハード上限（event / session / WAL / DB サイズ）/ 起動時サイズ安全検査 / 自己参照防止
- metrics 書込経路: `event_bus::UsageSink` trait を実装し、`UsageBucket`（1分バケット per provider/model）を single-writer がバッチ flush。raw 高頻度イベントは直接書かない
- credential 非永続化（ADR 0008、型とテストで保証）
- テスト: migration / CRUD / resume 復元 / downsampled flush 経路 / ハード上限拒否 / credential 書込不可

## Hard constraints

- **`.intent-cli/` と `intents/` には一切触れない**（host state。ADR 0018「sqlite-storage-schema」と `intents/evorch/features/storage-memory/overview.md` の Open question 更新は lead が closeout 時に実施する）
- 代わりに **PR body と完了リレーに以下を必ず含める**（lead が writeback に使う）:
  - 最終テーブル schema（各テーブルの主要カラムと型）
  - WAL 運用の決定値（hard limit の具体値・定期 PASSIVE checkpoint の間隔・起動時検査の閾値と動作）
  - credential 非永続化の実装方法（型レベルでどう保証したか）
  - projection（event log → session 復元）のアプローチ要約
  - rusqlite 採用に伴う build 要件（bundled / feature / C compiler 依存の有無）
- workflow ラベルの付け替えは intent-cli 経由のみ
- **CI green を確認してから完了リレー**（`gh pr checks <n>`）

## Flow

1. `gh issue view 3` で issue 全文を読む
2. 実装 → ローカル検証（`cargo build / test / clippy / fmt --check`）→ commit（適宜分割可）→ push（branch `v01-session-storage`）
3. `gh pr create --base main --head v01-session-storage`（**draft にしない**）。PR body に acceptance evidence（実行コマンドと結果）+ 上記の writeback 用情報を書く
4. CI green 確認
5. intent-cli（上記絶対パス）で `worker next-action --repo turtton/evorch --workdir . --format json` を実行し、その指示に従い `worker claim` → `worker result-summary` → `worker complete` まで進める
6. 完了リレー: `relay="[herdr-relay] v01-session-storage: 完了 PR#<n> <要約>"; herdr agent prompt w2:p1 "$relay"`（`$` や backtick はシェル変数経由）

## 着手時リレー（必須）

作業開始時に最初に以下を送り、配線を確認する:

```text
herdr agent prompt w2:p1 "[herdr-relay] v01-session-storage: 着手 (送り先 w2:p1 を確認)"
```

## sandbox で commit/push できない場合（bundle 運用）

worker sandbox が `.git` を read-only mount していて commit/push が失敗する場合**のみ**:

1. writable な copy gitdir を用意する（例: `git clone <repo-url> <copy-dir>`）し、そこで commit する
2. `git bundle create <checkout>/v01-session-storage.bundle v01-session-storage`（bundle は checkout ディレクトリ内に置く。`/tmp` は host から見えない）
3. リレーで bundle のパスと branch 名を伝える（lead が fetch → push → PR 作成を代行する）

## その他

- rusqlite（bundled は C ソースコンパイルを伴う）の導入で crates.io fetch / C compiler / `nix develop` が sandbox 内で失敗する場合は、無理をせず PR body に「host 側検証必要」と明記する（lead が host で確認する）
- 不明点・阻塞まった場合は `[herdr-relay]` で lead に質問する（`w2:p1` 固定）
