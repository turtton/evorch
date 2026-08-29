# v01-sandbox-approval 実装契約（lead → worker）

- **lead**: opencode pane `w2:p1`（host repo cwd）。**すべてのリレーは `w2:p1` 固定**。他の opencode pane が見えても送り先を変えない。
- **対象 issue**: turtton/evorch **#6**（`gh issue view 6` で全文を読むこと。本契約は要点と追加制約のみを記す）
- **作業ディレクトリ**: 現在の cwd（worktree `.worktrees/v01-sandbox-approval`、branch `v01-sandbox-approval`、base = origin/main。v01-scaffold / event-stream / session-storage / provider-client / tool-layer 完了済み）
- **intent-cli 絶対パス**: `/nix/store/bhn4d645q3f0nc7m7k0iw20phizrsw5i-intent-cli-0.26.0/bin/intent-cli`

## Task

issue #6 に従い `crates/sandbox/` に v0.1 security 層を実装する（ADR 0008 v0.1 必須）:

- **approval 層**: tool 実行を policy で分類（auto-allow / ask / deny）。ask は GUI/CLI の承認応答を待つ
- **sandbox 層**: Linux で dangerous 操作を sandbox 実行。承認しても sandbox 外では実行不可（二層分離）
- **Linux sandbox 第一実装の選択**（packet 推奨: bwrap。user namespaces + network namespace で egress deny 可能。Landlock は filesystem のみ。選択の最終判断と根拠は PR body + リレーで報告。bwrap 実行不可環境では fail-closed）
- **credential 隔離**: keychain 優先、0600 平文 JSON fallback。agent / 子プロセス / env へ渡さない
- **network egress 既定 deny**: provider endpoint のみ allowlist
- OS 抽象層を前提とした crate 構成（ADR 0009: v0.1 Linux 先行。macOS v0.2 / Windows v0.3 以降）
- 新規依存は `[workspace.dependencies]` に集約

## Hard constraints

- **`.intent-cli/` と `intents/` には一切触れない**（host state。**ADR 0021「Linux v0.1 sandbox 第一実装」**と tools-sandbox overview の Open question 解消は lead が closeout 時に実施する）
- 代わりに **PR body と完了リレーに以下を必ず含める**（lead が ADR 0021 writeback に使う）:
  - **Linux sandbox 第一実装の最終選択と根拠**（bwrap vs Landlock vs その他の比較・選定理由・fail-closed 方針）
  - approval policy の分類設計（auto-allow / ask / deny の判定方法・policy 表現）
  - 二層分離の実装方法（approval を通過しても sandbox 外で実行不可にする仕組み）
  - credential 隔離の実装方法（keychain 優先の実現・0600 fallback・非注入の保証）
  - network egress deny の実装方法（allowlist の表現・bwrap の network namespace 利用の有無）
  - OS 抽象層の crate 構成（将来 macOS/Windows をどう乗せるか）
- workflow ラベルの付け替えは intent-cli 経由のみ
- **CI green を確認してから完了リレー**（`gh pr checks <n>`）

## Flow

1. `gh issue view 6` で issue 全文を読む
2. 実装 → ローカル検証（`cargo build / test / clippy / fmt --check`）→ commit（適宜分割可）→ push（branch `v01-sandbox-approval`）
3. `gh pr create --base main --head v01-sandbox-approval`（**draft にしない**）。PR body に acceptance evidence（実行コマンドと結果）+ 上記の writeback 用情報を書く
4. CI green 確認
5. intent-cli（上記絶対パス）で `worker next-action --repo turtton/evorch --workdir . --format json` を実行し、その指示に従い `worker claim` → `worker result-summary` → `worker complete` まで進める
6. 完了リレー: `relay="[herdr-relay] v01-sandbox-approval: 完了 PR#<n> <要約>"; herdr agent prompt w2:p1 "$relay"`（`$` や backtick はシェル変数経由）

## 着手時リレー（必須）

作業開始時に最初に以下を送り、配線を確認する:

```text
herdr agent prompt w2:p1 "[herdr-relay] v01-sandbox-approval: 着手 (送り先 w2:p1 を確認)"
```

## sandbox で commit/push できない場合（bundle 運用）

worker sandbox が `.git` を read-only mount していて commit/push が失敗する場合**のみ**:

1. writable な copy gitdir を用意する（例: `git clone <repo-url> <copy-dir>`）し、そこで commit する
2. `git bundle create <checkout>/v01-sandbox-approval.bundle v01-sandbox-approval`（bundle は checkout ディレクトリ内に置く。`/tmp` は host から見えない）
3. リレーで bundle のパスと branch 名を伝える（lead が fetch → push → PR 作成を代行する）

## その他

- bwrap 等の外部バイナリへの依存が sandbox 内テストで制限される場合は、設計レベルで抽象化し実行系テストは可能な範囲に留めて PR body に明記する（lead が host で検証する）
- 不明点・阻塞まった場合は `[herdr-relay]` で lead に質問する（`w2:p1` 固定）
