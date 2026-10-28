# run_js 第 1 段階の観察用 課題セット

ADR 0039（`intents/evorch/decisions/0039-js-code-mode-tool.md`）の「評価 / 第 1 段階」で使う課題セット。題材は evorch 自身の固定コミット `4391a7e5`（`run_js` 未実装の時点）。Worker に `run_js` を有効にして課題を解かせ、ターン数・トークン・`run_js` の使い方・shell の使い方・品質を観察する。

各課題のプロンプトは `run_js` の使用を指示しない（自然に使うかを見る）。課題ごとに「正解で exit 0、未着手で exit 1」になる検証コマンドを付けてある。

## 課題一覧

| id | 種類 | 内容 | 検証が見るもの | 想定ターン数 |
|---|---|---|---|---|
| [`t1-survey`](tasks/t1-survey.md) | (1) 調査と要約 | `crates/tools/src/tools/` の 9 ツールの name / 実行モード / 権限 / Worker での許可を表にする | `answers/t1.json` の 65 項目 | 5〜10 |
| [`t2-aggregate`](tasks/t2-aggregate.md) | (2) 集計 | 22 crate × (ファイル数・行数・テスト属性数) と最大 5 ファイル | `answers/t2.json` の 78 項目 | 4〜10 |
| [`t3-rename`](tasks/t3-rename.md) | (3) 複数か所の編集 | `apply-patch` 内で構造体とフィールドを改名（51 行・5 ファイル） | grep の件数 + `cargo test -p apply-patch` | 5〜12 |
| [`t4-build-test`](tasks/t4-build-test.md) | (4) ビルド・テストを含む手順 | `agents` に `Role::ALL` を追加し、fmt / clippy / test を通して報告する | fmt + clippy + test + 隠しテスト + 報告 JSON | 8〜16 |
| [`t5-single-edit`](tasks/t5-single-edit.md) | (5) 1 回で済む対照 | `grep.rs` の定数と doc コメントの数字を 1 回の編集で変える | 差分がちょうど +2/-2 行 | 2〜4 |
| [`t6-debug-fix`](tasks/t6-debug-fix.md) | (6) 調査してから直す | `apply-patch` に仕込んだ 2 つの退行（別ファイル）をテスト失敗から探して直す | `cargo test -p apply-patch` + 報告 | 6〜14 |

ビルド・テストを走らせる課題（t3 / t4 / t6）は `apply-patch` と `agents` の 2 crate だけを使い、ワークスペース全体のビルドは要らない（依存の解決だけは必要）。

## ディレクトリ構成

```
evals/run-js/
  README.md
  tasks/<id>.md          課題の定義（プロンプト全文、想定解、採点基準、検証、ターン数、run_js が効く理由）
  verify/<id>.sh         検証コマンド。`verify/<id>.sh <clone>`、合格 = exit 0 / 不合格 = exit 1 / 使い方の誤り = exit 2
  verify/expected/       t1 / t2 の期待値（JSON）
  verify/hidden/         t4 の隠しテスト（検証時だけ clone にコピーする）
  verify/lib/            検証の共通部
  setup/                 課題固有の初期化（t6 の退行コミット）
  solutions/<id>.sh      参照解（検証スクリプトの自己確認用。モデルには見せない）
  scripts/               prepare-clone.sh / prompt.sh / self-test.sh / gen-t2-expected.mjs
```

## 実行手順の骨子

以下の `<…>` は環境に合わせて置き換える。`--db` / `--run-js-enabled` は別担当が headless に実装中のため、フラグ名は仮置き。

### 1. 事前準備（1 回だけ）

```sh
# 依存を取得しておく（検証と Worker のビルドは --offline で動かすため）
cd <evorch-checkout> && cargo fetch
```

`evals/run-js` を含むこのブランチ（`feat/run-js-eval`）を 1 か所に checkout しておく（以下 `$EVAL`）。

### 2. run ごとの clone を作る

利用者の作業ツリーでは実行しない。run ごとに使い捨ての clone を作る。`prepare-clone.sh` は固定コミットの履歴だけを取り込み（他の ref は入れない。`run_js` の実装を含みうる branch が見えないようにするため）、detached HEAD にし、`answers/` を作り（git の追跡対象外）、課題固有の初期化（t6 のみ）を適用する。

```sh
RUN=<scratch>/<model>-<task>-<n>
$EVAL/evals/run-js/scripts/prepare-clone.sh t3-rename "$RUN" [<source-repo>]
```

`<source-repo>` の既定は、このブランチの worktree の共通 `.git`。初回のビルドは clone ごとに走る（依存は sccache で大半が再利用される。`agents` と `apply-patch` は数十秒）。clone ごとに `target/` を持たせ、`CARGO_TARGET_DIR` は共有しない（AGENTS.md）。

### 3. モデルごとの overlay 設定

モデルごとの user-config ディレクトリ（overlay）を用意する。provider は `codex-personal`（`gpt-6.1-sol`、`gpt-6-luna+fast`、`gpt-6-astra`）と `claude-personal`（`claude-opus-5-5`、`claude-sonnet-5-5`）。モデル名は利用者の設定に登録された名前を使う。

```
<overlays>/gpt-6.1-sol/        # Worker のモデルを gpt-6.1-sol にする overlay（仮置き）
<overlays>/gpt-6-luna-fast/
<overlays>/gpt-6-astra/
<overlays>/claude-opus-5-5/
<overlays>/claude-sonnet-5-5/
```

overlay の中身（設定ファイル名と書式）はこの README では決めない。5 つの overlay の違いは Worker のモデルだけにし、他の設定（`run_js` の有効化、上限など）は揃える。

### 4. headless で Worker を走らせる

```sh
evorch run \
  --project "$RUN" \
  --role worker \
  --prompt "$($EVAL/evals/run-js/scripts/prompt.sh t3-rename)" \
  --user-config <overlays>/<model> \
  --db <results>/<model>-t3-rename-<n>.db \   # 仮置き: run ごとの store の出力先（別担当が実装中）
  --run-js-enabled true                        # 仮置き: run_js の有効化フラグ
```

`scripts/prompt.sh <id>` は `tasks/<id>.md` の ` ```prompt ` ブロックをそのまま出す（モデルに渡す全文。補足や指示を足さない）。

### 5. 検証

run が終わったら、同じ clone に対して検証コマンドを走らせる。

```sh
$EVAL/evals/run-js/verify/t3-rename.sh "$RUN"; echo "exit=$?"
```

検証は clone の `cargo` を使うため、clone の `target/` が温まっていれば短い。結果（exit code と標準エラーの不一致一覧）を run の記録に残す。ターン数・トークン・`run_js` の使い方などの指標は、store（`--db`）から集計する（集計手段は ADR 0039 の Open questions）。LLM による採点の観点は各 `tasks/<id>.md` の「採点基準」に書いてある。

### 6. 繰り返し

予備の実行は 5 モデル × 6 課題を 1 回ずつ（30 run）。その後、ばらつきを見るため各条件を 3 回程度に増やす。run ごとに手順 2 の clone を作り直す（clone は使い回さない）。

## 検証の自己確認

検証スクリプトとその期待値が壊れていないことを、各課題で「未着手の clone は不合格、参照解を適用した clone は合格」の両方で確かめる。

```sh
scripts/self-test.sh <存在しない作業ディレクトリ> [task-id ...]
```

固定コミットが変わった場合や課題を変えた場合に再実行する。t2 の期待値は `scripts/gen-t2-expected.mjs <固定コミットの checkout>` で再計算できる（`verify/expected/t2-aggregate.json` と一致することを確認する）。

## 注意

- 課題の事実（t1 の表、t2 の数値、t3 の出現数、t4 のテスト件数 28、t6 の退行）は固定コミット `4391a7e5` に基づく。別のコミットでは使えない。
- `answers/` は clone の `.git/info/exclude` に入れてあり、`git status` や `git diff` には出ない。検証は `git diff 4391a7e5`（追跡ファイルの差分）で変更範囲を見る。
- t5 は `run_js` が不要な対照。t4 は `run_js` の中で cargo を走らせず通常の shell ジョブで流すのが望ましい（ADR 0039）。使い方の良し悪しは検証の合否とは別に観察する。
- t6 は `git show HEAD` で退行コミットが見える。調査手段の観察対象であり、減点しない。
- 検証が cargo を走らせるため、並行して複数の検証や run を同じ clone で動かさない。
