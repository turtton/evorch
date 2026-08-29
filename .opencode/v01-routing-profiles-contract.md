# v01-routing-profiles 実装契約（lead → worker）

- **lead**: opencode pane `w2:p1`（host repo cwd）。**すべてのリレーは `w2:p1` 固定**。他の opencode pane が見えても送り先を変えない。
- **対象 issue**: turtton/evorch **#8**（`gh issue view 8` で全文を読むこと。本契約は要点と追加制約のみを記す）
- **作業ディレクトリ**: 現在の cwd（worktree `.worktrees/v01-routing-profiles`、branch `v01-routing-profiles`、base = origin/main。v01-scaffold / event-stream / session-storage / provider-client / tool-layer / sandbox-approval 完了済み。#7 agent-roles は別 worker が並列作業中 — crates/agents・runtime には触れないこと）
- **intent-cli 絶対パス**: `/nix/store/bhn4d645q3f0nc7m7k0iw20phizrsw5i-intent-cli-0.26.0/bin/intent-cli`

## Task

issue #8 に従い `crates/config/`・`crates/model/`・`crates/routing/` に v0.1 の設定・モデル・ルーティング 3 層を実装する:

- **crates/config/**: TOML マルチソース読み込みと優先順位 merge（CLI 引数/環境変数 > project `./evorch.toml` > user `~/.config/evorch/config.toml` > builtin defaults）、`config.d/*.toml` の辞書順 deep merge（後勝ち）、version フィールド + migration 関数、schemars による JSON Schema 生成・公開、v0.1 設定領域の typed struct（provider profiles / model routing / panel layout・keybind / diagnostics / permission preset / 計測）
- **crates/model/**: ModelCatalog（builtin デフォルト + models.dev 起動時 fetch（キャッシュ + オフラインフォールバック）+ `/v1/models` 検出マージ（属性未確定フラグ付き））、更新履歴の SQLite 記録（storage crate 利用）、resolve 時の availability / cost / capability 参照
- **crates/routing/**: ProviderProfile 定義（credential は参照のみ、ADR 0008 の CredentialStore とは接続しない — v0.1 は参照表現のみ）、logical model → route → profile → 実モデル ID の解決、simple fallback（current profile → 同じ logical model の別 profile → 別 logical model）、session affinity の基礎
- 新規依存は `[workspace.dependencies]` に集約

## Hard constraints

- **`.intent-cli/` と `intents/` には一切触れない**（host state。`features/provider-routing` overview への writeback は lead が closeout 時に実施する。本 slice の ADR candidate は none — ADR 0004 / 0013 / 0014 で確定済み）
- **crates/agents/ と crates/runtime/ には一切触れない**（#7 並列作業中のスコープ）
- 代わりに **PR body と完了リレーに以下を必ず含める**（lead が writeback に使う）:
  - config 層の実装要約（マルチソース merge 順序・deep merge 実装・version/migration 設計・JSON Schema 生成方法・typed struct の主要フィールド）
  - ModelCatalog の供給源マージ設計（builtin / models.dev fetch+キャッシュ / `/v1/models` 検出・属性未確定フラグ）と SQLite 記録方式
  - routing の解決アルゴリズム（logical model → route → profile → 実モデル ID）と simple fallback の遷移順・失敗判定（429 / 5xx / timeout / quota / auth の扱い）
  - session affinity の基礎実装（prompt cache のため同一 session で profile に留まる仕組み）
  - credential 参照の表現（config に書かない構造の保証）
  - 新規依存の一覧と採用理由
- workflow ラベルの付け替えは intent-cli 経由のみ
- **CI green を確認してから完了リレー**（`gh pr checks <n>`）

## Flow

1. `gh issue view 8` で issue 全文を読む
2. 実装 → ローカル検証（`cargo build / test / clippy / fmt --check`）→ commit（適宜分割可）→ push（branch `v01-routing-profiles`）
3. `gh pr create --base main --head v01-routing-profiles`（**draft にしない**）。PR body に acceptance evidence（実行コマンドと結果）+ 上記の writeback 用情報を書く
4. CI green 確認
5. intent-cli（上記絶対パス）で `worker next-action --repo turtton/evorch --workdir . --format json` を実行し、その指示に従い `worker claim` → `worker result-summary` → `worker complete` まで進める
6. 完了リレー: `relay="[herdr-relay] v01-routing-profiles: 完了 PR#<n> <要約>"; herdr agent prompt w2:p1 "$relay"`（`$` や backtick はシェル変数経由）

## 着手時リレー（必須）

作業開始時に最初に以下を送り、配線を確認する:

```text
herdr agent prompt w2:p1 "[herdr-relay] v01-routing-profiles: 着手 (送り先 w2:p1 を確認)"
```

## sandbox で commit/push できない場合（bundle 運用）

worker sandbox が `.git` を read-only mount していて commit/push が失敗する場合**のみ**:

1. writable な copy gitdir を用意する（例: `git clone <repo-url> <copy-dir>`）し、そこで commit する
2. `git bundle create <checkout>/v01-routing-profiles.bundle v01-routing-profiles`（bundle は checkout ディレクトリ内に置く。`/tmp` は host から見えない）
3. リレーで bundle のパスと branch 名を伝える（lead が fetch → push → PR 作成を代行する）

## その他

- models.dev fetch はテストでは mock にすること（実ネットワーク非依存）
- 不明点・阻塞まった場合は `[herdr-relay]` で lead に質問する（`w2:p1` 固定）
