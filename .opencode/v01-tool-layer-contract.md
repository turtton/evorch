# v01-tool-layer 実装契約（lead → worker）

- **lead**: opencode pane `w2:p1`（host repo cwd）。**すべてのリレーは `w2:p1` 固定**。他の opencode pane が見えても送り先を変えない。
- **対象 issue**: turtton/evorch **#5**（`gh issue view 5` で全文を読むこと。本契約は要点と追加制約のみを記す）
- **作業ディレクトリ**: 現在の cwd（worktree `.worktrees/v01-tool-layer`、branch `v01-tool-layer`、base = origin/main。v01-scaffold / event-stream / session-storage / provider-client 完了済み）
- **intent-cli 絶対パス**: `/nix/store/bhn4d645q3f0nc7m7k0iw20phizrsw5i-intent-cli-0.26.0/bin/intent-cli`

## Task

issue #5 に従い `crates/tools/` に tool 基盤と v0.1 標準 5 ツールを実装する:

- 統一 `Tool` trait（`name` / JSON Schema `schema` / `execute` / 結果正規化 / permissions 表明）
- `read`（存在しない path は typed error）、`edit`（一時ファイル + rename の atomic write + 制御マーカー エスケープ）、`grep`（正規表現、typed error）、`shell`（非 interactive = `tokio::process::Command`、interactive = portable-pty）、`git_diff`（working tree diff）
- tool 実行結果の event stream への emit（`event_bus` crate の `ToolStarted` / `ToolCompleted` 等を利用）
- JSON Schema による tool call 引数検証
- 新規依存は `[workspace.dependencies]` に集約（regex / portable-pty 等。必要に応じて）
- Out of scope（MCP / sandbox / approval / ContentOrigin 等）は実装しない

## Hard constraints

- **`.intent-cli/` と `intents/` には一切触れない**（host state。`features/tools-sandbox` overview への v0.1 標準5ツール確定記録は lead が closeout 時に実施する。本 slice の ADR candidate は none）
- 代わりに **PR body と完了リレーに以下を必ず含める**（lead が writeback に使う）:
  - `Tool` trait の表面（シグネチャと主要型）
  - 5 ツールの JSON Schema 要約（主要パラメータ）
  - 制御マーカー エスケープの実装方法（対象マーカー一覧・エスケープ手法・edit 適用位置）
  - edit の atomic write 実装（一時ファイル命名・rename 手順）
  - shell の PTY / 非 PTY 分離と出力ストリーム設計
  - tool 実行結果の event emit 経路
  - 新規依存の一覧と採用理由
- workflow ラベルの付け替えは intent-cli 経由のみ
- **CI green を確認してから完了リレー**（`gh pr checks <n>`）

## Flow

1. `gh issue view 5` で issue 全文を読む
2. 実装 → ローカル検証（`cargo build / test / clippy / fmt --check`）→ commit（適宜分割可）→ push（branch `v01-tool-layer`）
3. `gh pr create --base main --head v01-tool-layer`（**draft にしない**）。PR body に acceptance evidence（実行コマンドと結果）+ 上記の writeback 用情報を書く
4. CI green 確認
5. intent-cli（上記絶対パス）で `worker next-action --repo turtton/evorch --workdir . --format json` を実行し、その指示に従い `worker claim` → `worker result-summary` → `worker complete` まで進める
6. 完了リレー: `relay="[herdr-relay] v01-tool-layer: 完了 PR#<n> <要約>"; herdr agent prompt w2:p1 "$relay"`（`$` や backtick はシェル変数経由）

## 着手時リレー（必須）

作業開始時に最初に以下を送り、配線を確認する:

```text
herdr agent prompt w2:p1 "[herdr-relay] v01-tool-layer: 着手 (送り先 w2:p1 を確認)"
```

## sandbox で commit/push できない場合（bundle 運用）

worker sandbox が `.git` を read-only mount していて commit/push が失敗する場合**のみ**:

1. writable な copy gitdir を用意する（例: `git clone <repo-url> <copy-dir>`）し、そこで commit する
2. `git bundle create <checkout>/v01-tool-layer.bundle v01-tool-layer`（bundle は checkout ディレクトリ内に置く。`/tmp` は host から見えない）
3. リレーで bundle のパスと branch 名を伝える（lead が fetch → push → PR 作成を代行する）

## その他

- portable-pty は C 依存を伴う可能性がある（crates.io fetch / C compiler / `nix develop` が sandbox 内で失敗する場合は無理をせず PR body に「host 側検証必要」と明記。lead が host で確認する）
- 不明点・阻塞まった場合は `[herdr-relay]` で lead に質問する（`w2:p1` 固定）
