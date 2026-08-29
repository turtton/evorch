# v01-gui-panes 実装契約（lead → worker）

- **lead**: opencode pane `w2:p1`（host repo cwd）。**すべてのリレーは `w2:p1` 固定**。他の opencode pane が見えても送り先を変えない。
- **対象 issue**: turtton/evorch **#9**（`gh issue view 9` で全文を読むこと。本契約は要点と追加制約のみを記す）
- **作業ディレクトリ**: 現在の cwd（worktree `.worktrees/v01-gui-panes`、branch `v01-gui-panes`、base = origin/main。**#1–#7 完了済み** — event-bus / storage / providers / tools / sandbox / agents / runtime の実装が main に存在する。**#8 v01-routing-profiles は別 worker が並列作業中**）
- **intent-cli 絶対パス**: `/nix/store/bhn4d645q3f0nc7m7k0iw20phizrsw5i-intent-cli-0.26.0/bin/intent-cli`

## Task

issue #9 に従い `crates/workspace-ui/` と `crates/gui/` に v0.1 GUI を実装する:

- **crates/workspace-ui/**: Workspace Model（Split / Tabs / Panel / Floating / Window の framework 非依存データ）、layout 検証・保存、panel 定義（agent / terminal / tasks）
- **crates/gui/**: egui + egui_dock（anhosh/egui_dock 0.21.x、ADR 0007）アプリ、UI Event Bus 購読、Workspace Model → GUI Renderer 変換
  - agent pane: `event_bus` crate の subscribe API から transcript（message / reasoning / tool 実行）を描画
  - terminal pane: portable-pty で PTY を扱う
  - tasks pane: `runtime` crate の AgentRun 一覧（name / role / status / model）を表示
- offscreen レンダリング抽象（ADR 0009、フレーム capture 可能な土台）
- 初期3 pane は egui_dock の binary split 制約のため nested split で構成
- 層構造を守る: Agent Kernel → UI Event Bus → Workspace Model → GUI Renderer
- 新規依存は `[workspace.dependencies]` に集約（egui / egui_dock / portable-pty は既存 workspace dep あり）

## Hard constraints

- **`.intent-cli/` と `intents/` には一切触れない**（host state。`features/gui-workbench` overview への writeback は lead が closeout 時に実施する。本 slice の ADR candidate は none）
- **並列作業との境界（最重要）**: **`crates/config/`・`crates/model/`・`crates/routing/` には一切触れない**。これらは #8 worker が並列で実装中。#9 の panel layout / keybind の config 公開は workspace-ui crate 内に v0.1 最小の設定型を定義して対応し、crates/config への統合は #8 merge 後の後続 slice に委譲することを PR body に明記する
- 同様に **crates/agents/・runtime/・event-bus/ 等の既存 crate には原則読み取り専用**（必要最小限の pub 追加がどうしても必要な場合のみ、後方互換の範囲で。PR body に明記）
- GUI は購読・表示のみ — ネットワーク・provider 呼び出しは kernel 側に委譲
- 代わりに **PR body と完了リレーに以下を必ず含める**（lead が writeback に使う）:
  - Workspace Model の型設計（Split / Tabs / Panel / Floating / Window の定義と層構造の守り方）
  - 3 pane の描画内容要約（agent transcript / terminal PTY / tasks AgentRun 一覧のデータ経路）
  - offscreen レンダリング抽象の設計
  - v0.1 config 公開の実装方法（workspace-ui 内の最小設定型・#8 統合委譲の旨）
  - nested split 構成の確定有無（確定なら overview open questions 更新の素材）
  - 新規依存の一覧と採用理由
- workflow ラベルの付け替えは intent-cli 経由のみ
- **CI green を確認してから完了リレー**（`gh pr checks <n>`）。**注意: egui/egui_dock はネイティブ deps（X11/GL 等）を引く可能性がある。CI Linux runner での build が落ちる場合、必要な system deps を CI workflow に追加してよい（runner: ubuntu-latest）**

## Flow

1. `gh issue view 9` で issue 全文を読む
2. 実装 → ローカル検証（`cargo build / test / clippy / fmt --check`。GUI の表示確認が難しい場合は offscreen capture テストで代替）→ commit（適宜分割可）→ push（branch `v01-gui-panes`）
3. `gh pr create --base main --head v01-gui-panes`（**draft にしない**）。PR body に acceptance evidence（実行コマンドと結果）+ 上記の writeback 用情報を書く
4. CI green 確認
5. intent-cli（上記絶対パス）で `worker next-action --repo turtton/evorch --workdir . --format json` を実行し、その指示に従い `worker claim` → `worker result-summary` → `worker complete` まで進める
6. 完了リレー: `relay="[herdr-relay] v01-gui-panes: 完了 PR#<n> <要約>"; herdr agent prompt w2:p1 "$relay"`（`$` や backtick はシェル変数経由）

## 着手時リレー（必須）

作業開始時に最初に以下を送り、配線を確認する:

```text
herdr agent prompt w2:p1 "[herdr-relay] v01-gui-panes: 着手 (送り先 w2:p1 を確認)"
```

## sandbox で commit/push できない場合（bundle 運用）

worker sandbox が `.git` を read-only mount していて commit/push が失敗する場合**のみ**:

1. writable な copy gitdir を用意する（例: `git clone <repo-url> <copy-dir>`）し、そこで commit する
2. `git bundle create <checkout>/v01-gui-panes.bundle v01-gui-panes`（bundle は checkout ディレクトリ内に置く。`/tmp` は host から見えない）
3. リレーで bundle のパスと branch 名を伝える（lead が fetch → push → PR 作成を代行する）

## その他

- 不明点・阻塞まった場合は `[herdr-relay]` で lead に質問する（`w2:p1` 固定）
