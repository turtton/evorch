# v01-scaffold 実装契約（lead → worker）

- **lead**: opencode pane `w2:p1`（host repo cwd）。**すべてのリレーは `w2:p1` 固定**。他の opencode pane が見えても送り先を変えない。
- **対象 issue**: turtton/evorch **#1**（`gh issue view 1` で全文を読むこと。本契約は要点と追加制約のみを記す）
- **作業ディレクトリ**: 現在の cwd（worktree `.worktrees/v01-scaffold`、branch `v01-scaffold`、base = origin/main）
- **intent-cli 絶対パス**: `/nix/store/bhn4d645q3f0nc7m7k0iw20phizrsw5i-intent-cli-0.26.0/bin/intent-cli`

## Task

issue #1 に従い v0.1 の土台を実装する:

- ルート `Cargo.toml`（virtual workspace、`members = ["crates/*"]`、`workspace.dependencies` で共通依存管理）
- 11 crate 骨格: `runtime` / `event-bus` / `storage` / `providers` / `tools` / `sandbox` / `routing` / `model` / `config` / `gui` + バイナリ `evorch`（各 `src/lib.rs`、バイナリは `src/main.rs`）
- `rust-toolchain.toml`（channel + components 固定）
- `flake.nix` devShell への Rust toolchain 追加（既存 flake の構造は壊さない）
- `.github/workflows/ci.yml`: `cargo fmt --check` → `cargo clippy --workspace -- -D warnings` → `cargo test --workspace`（**実 API 呼び出しは含めない**。ADR 0015 の2層検証）
- crate 構成の最終決定は本 slice で行い、判断根拠を PR body に記録する

## Hard constraints

- **`.intent-cli/` と `intents/` には一切触れない**（host state。ADR 0016 生成と `intents/evorch/technology/architecture.md` 更新は lead が closeout 時に実施する）
- 代わりに **PR body と完了リレーに「最終 crate 一覧（各 crate の一言役割付き）+ rust channel バージョン」を必ず含める**（lead が ADR 0016 writeback に使う）
- workflow ラベルの付け替えは intent-cli 経由のみ（`gh label edit` / GitHub UI での手動変更禁止）
- **CI green を確認してから完了リレー**（`gh pr checks <n>`）

## Flow

1. `gh issue view 1` で issue 全文を読む
2. 実装 → commit（適宜分割可）→ push（branch `v01-scaffold`）
3. `gh pr create --base main --head v01-scaffold`（**draft にしない**）。PR body に acceptance evidence（実行したコマンドと結果）を書く
4. CI green 確認
5. intent-cli（上記絶対パス）で `worker next-action --repo turtton/evorch --workdir . --format json` を実行し、その指示に従い `worker claim` → `worker result-summary` → `worker complete` まで進める
6. 完了リレー: `relay="[herdr-relay] v01-scaffold: 完了 PR#<n> <要約>"; herdr agent prompt w2:p1 "$relay"`（`$` や backtick が混入し得るテキストはシェル変数経由で渡す）

## 着手時リレー（必須）

作業開始時に最初に以下を送り、配線を確認する:

```text
herdr agent prompt w2:p1 "[herdr-relay] v01-scaffold: 着手 (送り先 w2:p1 を確認)"
```

## sandbox で commit/push できない場合（bundle 運用）

worker sandbox が `.git` を read-only mount していて commit/push が失敗する場合**のみ**:

1. writable な copy gitdir を用意する（例: `git clone <repo-url> <copy-dir>`）し、そこで commit する
2. `git bundle create <checkout>/v01-scaffold.bundle v01-scaffold`（bundle は checkout ディレクトリ内に置く。`/tmp` は host から見えない）
3. リレーで bundle のパスと branch 名を伝える（lead が fetch → push → PR 作成を代行する）

## その他

- `nix develop` やネットワークが必要な検証が sandbox 内で失敗する場合は、無理をせず PR body に「host 側検証必要」と明記する（lead が host で確認する）
- 不明点・阻塞まった場合は `[herdr-relay]` で lead に質問する（`w2:p1` 固定）
