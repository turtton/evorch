# v01-provider-client 実装契約（lead → worker）

- **lead**: opencode pane `w2:p1`（host repo cwd）。**すべてのリレーは `w2:p1` 固定**。他の opencode pane が見えても送り先を変えない。
- **対象 issue**: turtton/evorch **#4**（`gh issue view 4` で全文を読むこと。本契約は要点と追加制約のみを記す）
- **作業ディレクトリ**: 現在の cwd（worktree `.worktrees/v01-provider-client`、branch `v01-provider-client`、base = origin/main。v01-scaffold / event-stream / session-storage 完了済み）
- **intent-cli 絶対パス**: `/nix/store/bhn4d645q3f0nc7m7k0iw20phizrsw5i-intent-cli-0.26.0/bin/intent-cli`

## Task

issue #4 に従い `crates/providers/` に provider 抽象と 3 実装を実装する:

- 統一 `ProviderClient` trait（非同期 `send` / `stream`）と `ProviderCapabilities` を返す
- canonical Message（provider 非依存）の正規化と、OpenAI chat.completions / Anthropic Messages API / OpenAI-compatible（chat.completions wire 共用）との相互変換
- reqwest による SSE ストリーミング（delta 結合、finish reason、error event 処理）
- tool call を provider 別 wire 形式から canonical 形式へ変換
- Usage（input/output/cache_read/cache_write）parse → **event stream へ usage イベント emit**（`event_bus` crate（`../event-bus`）を利用。バスへの接続方法は設計として明確にすること）
- typed error: HTTP 4xx / 5xx / 429（Retry-After 付与）/ timeout / 不正 SSE
- wiremock による mock 契約テスト（recorded response fixture。**CI・ローカルともに実 API へアクセスしない**。ADR 0015 第1層）
- canonical message round-trip テスト
- 新規依存は `[workspace.dependencies]` に集約（reqwest / wiremock(dev) / 必要な tokio feature 等）

## Hard constraints

- **`.intent-cli/` と `intents/` には一切触れない**（host state。ADR 0020「canonical message normalization」と provider-routing overview / mvp-roadmap の writeback は lead が closeout 時に実施する）
- 代わりに **PR body と完了リレーに以下を必ず含める**（lead が writeback に使う）:
  - 最終 canonical Message 型の shape 要約（主要フィールドと役割）
  - `ProviderClient` trait の表面（`send` / `stream` のシグネチャと返却型）
  - OpenAI ↔ Anthropic 変換の主要な非対称点と処理方針（system メッセージ/tool 表現等）
  - usage イベントの emit 経路（ProviderClient と EventBus の接続方法）
  - error taxonomy の一覧
  - v0.1 の provider 3 種確定に関する事実（何を「確定」と言えるか）
- workflow ラベルの付け替えは intent-cli 経由のみ
- **CI green を確認してから完了リレー**（`gh pr checks <n>`）

## Flow

1. `gh issue view 4` で issue 全文を読む
2. 実装 → ローカル検証（`cargo build / test / clippy / fmt --check`）→ commit（適宜分割可）→ push（branch `v01-provider-client`）
3. `gh pr create --base main --head v01-provider-client`（**draft にしない**）。PR body に acceptance evidence（実行コマンドと結果）+ 上記の writeback 用情報を書く
4. CI green 確認
5. intent-cli（上記絶対パス）で `worker next-action --repo turtton/evorch --workdir . --format json` を実行し、その指示に従い `worker claim` → `worker result-summary` → `worker complete` まで進める
6. 完了リレー: `relay="[herdr-relay] v01-provider-client: 完了 PR#<n> <要約>"; herdr agent prompt w2:p1 "$relay"`（`$` や backtick はシェル変数経由）

## 着手時リレー（必須）

作業開始時に最初に以下を送り、配線を確認する:

```text
herdr agent prompt w2:p1 "[herdr-relay] v01-provider-client: 着手 (送り先 w2:p1 を確認)"
```

## sandbox で commit/push できない場合（bundle 運用）

worker sandbox が `.git` を read-only mount していて commit/push が失敗する場合**のみ**:

1. writable な copy gitdir を用意する（例: `git clone <repo-url> <copy-dir>`）し、そこで commit する
2. `git bundle create <checkout>/v01-provider-client.bundle v01-provider-client`（bundle は checkout ディレクトリ内に置く。`/tmp` は host から見えない）
3. リレーで bundle のパスと branch 名を伝える（lead が fetch → push → PR 作成を代行する）

## その他

- crates.io fetch や `nix develop` が sandbox 内で失敗する場合は、無理をせず PR body に「host 側検証必要」と明記する（lead が host で確認する）
- **実 provider API へのネットワークアクセスは一切しない**（検証は wiremock のみ。これは sandbox 制約でもあり契約でもある）
- 不明点・阻塞まった場合は `[herdr-relay]` で lead に質問する（`w2:p1` 固定）
