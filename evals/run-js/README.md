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
  scripts/               prepare-clone.sh / prompt.sh / self-test.sh / gen-t2-expected.mjs（課題・検証）
                         make-overlay.sh / run-one.sh / run-matrix.sh / models.sh / metrics.mjs（実行・集計）
  test/                  実行・集計スクリプトの確認（ダミー evorch、fixture、単体確認）
```

## 実行手順の骨子

フラグ名は確定（`feat/run-js-prototype` の `evorch run` / `evorch inspect`）:

```
evorch run --project <clone> --role worker --prompt <text> \
  [--user-config <overlay>] [--run-js-enabled <true|false>] [--db <path>]
evorch inspect --db <path> run <run-id> [--full]
evorch inspect --db <path> events --run <run-id> [--limit <n>]     # 既定 limit 200、上限 10000
```

- `--db` は `inspect` では**サブコマンドの直前**に置く（`inspect --db <path> run …`）。
- `--db` を付けた run だけ stderr に `run_id: <id>` を出す。`--run-js-enabled` は `[tools.run_js] enabled` の上書き。
- `--user-config` は元の `~/.config/evorch` にフォールバックしない。overlay に必要な設定をすべて入れる。

### 1. 事前準備（1 回だけ）

```sh
# 依存を取得しておく（検証と Worker のビルドは --offline で動かすため）
cd <evorch-checkout> && cargo fetch
# run_js を含む evorch をビルドし、バイナリを指定する（feat/run-js-prototype の成果）
export EVORCH_BIN=<evorch-checkout>/target/release/evorch
```

`evals/run-js` を含むこのブランチ（`feat/run-js-eval`）を 1 か所に checkout しておく（以下 `$EVAL`）。資格情報は keyring にあり、overlay には入れない。

### 2. モデルごとの overlay（`make-overlay.sh`）

```sh
$EVAL/evals/run-js/scripts/make-overlay.sh <profile> <model-id> <effort> <outdir>
```

- `<outdir>/config.toml` — 利用者の `config.toml` のコピー（元は `$EVORCH_CONFIG_SRC`、既定 `${XDG_CONFIG_HOME:-~/.config}/evorch`）。秘密値は含まれない。
- `<outdir>/config.d/zz-worker.toml` — 候補が 1 件だけの評価専用 route `eval-pinned`（`profile` と `model` を明示）と、`[agents.worker]` / `models = [{ route = "eval-pinned", reasoning_effort = "<effort>" }]`。5 つの overlay の違いは Worker のモデルと provider だけ。
- provider の固定: 利用者の `config.toml` の route（`gpt-luna+fast` など）は `codex-work` が先頭候補で、そのままだと `*-work` に解決される。設定の深マージは配列を置き換えるので、既存 route を上書きせず別名の route を足している。候補が 1 件なので他 profile へのフェイルオーバーも起きない。`<profile>` は `codex-personal` / `claude-personal` のみ受け付け、`config.toml` に定義があることも確認する。
- 生成物はリポジトリにコミットしない（このリポジトリの作業ツリー内への出力は拒否する）。`run-one.sh` は run ごとに作って後で消すので、通常は手で作らない。

モデルキーと固定先（`scripts/models.sh`）:

| model-key | profile | モデル |
|---|---|---|
| `gpt-sol` | `codex-personal` | gpt-6.1-sol |
| `gpt-luna-fast` | `codex-personal` | gpt-6-luna+fast |
| `gpt-astra` | `codex-personal` | gpt-6-astra |
| `claude-opus` | `claude-personal` | claude-opus-5-5 |
| `claude-sonnet` | `claude-personal` | claude-sonnet-5-5 |

`reasoning_effort` の既定は `medium`（`models.sh` の `EVAL_DEFAULT_EFFORT`）。モデルごとに変えるなら `--effort` か `EVAL_EFFORT` で指定する（実行前に決める）。

### 3. 1 run の実行（`run-one.sh`）

```sh
$EVAL/evals/run-js/scripts/run-one.sh <model-key> <task-id> <rep> <outroot> \
  [--run-js true|false] [--effort <e>] [--timeout <sec>] [--keep-clone] [--force]
```

流れ: overlay 生成 → `prepare-clone.sh`（t6 は退行コミットを積む setup を含む）→ `prompt.sh` → `evorch run`（`timeout` 付き、既定 30 分 = `EVAL_TIMEOUT_S`）→ `verify/<id>.sh`（既定 15 分 = `EVAL_VERIFY_TIMEOUT_S`。タイムアウトした run も検証する）→ 結果の保存 → clone と overlay の削除。clone は `${EVAL_WORK:-${TMPDIR:-/tmp}/run-js-eval-work}` に作り、clone ごとに `target/` を持つ（`CARGO_TARGET_DIR` は共有しない）。

環境変数: `EVORCH_BIN`（必須）、`EVORCH_CONFIG_SRC`、`EVAL_WORK`、`EVAL_SOURCE_REPO`（`prepare-clone.sh` の source-repo）。

結果は `<outroot>/<model>/<task>/<rep>/`（`<outroot>` はリポジトリの外に置く）:

| ファイル | 内容 |
|---|---|
| `result.json` | `run_id`、`status`（`ok` / `error` = evorch が非 0 終了 / `timeout`）、`evorch_exit`、`duration_ms`、`verify`（`pass` / `fail` / `timeout` / `error`）、`verify_exit`、`run_js_enabled`、`profile`、`model_id`、`effort` など。**最後に書く**ので、あれば完了扱い |
| `run.db` | この run の store（`--db`）。run ごとに別ファイルなので並列でも衝突しない |
| `evorch.stdout.log` / `evorch.stderr.log` | Worker の最終出力 / `run_id:` を含む stderr |
| `verify.log` | 検証の出力（不一致一覧） |
| `final.diff` / `answers/` | 固定コミットからの追跡ファイル差分 / `answers/` のコピー |
| `inspect-run.json` / `inspect-events.json` | run 直後の `evorch inspect` の出力（`metrics.mjs` が優先して読む。無ければ `run.db` を読む） |
| `zz-worker.toml` | この run で使った Worker のモデル設定 |

終了コード: 0 = 保存した（verify の合否は問わない）/ 結果が既にある、2 = 使い方や環境の誤り、3 = 準備に失敗（結果は書かない）。`result.json` が無いディレクトリは作り直す。

### 4. マトリクスの実行（`run-matrix.sh`）

```sh
$EVAL/evals/run-js/scripts/run-matrix.sh --outroot <dir> \
  [--models "gpt-sol claude-opus"] [--tasks "t1-survey t5-single-edit"] \
  [--reps 3] [--rep-from 1] [-j 2] [--run-js true|false] [--effort <e>] [--timeout <sec>] \
  [--retry-errors] [--list] [--keep-clone]
```

- 既定は全 5 モデル × 全 6 課題 × 1 回。順序は rep → 課題 → モデル。
- `result.json` がある組はスキップするので、中断しても**同じコマンドで再開**できる。`--list` で実行せずに計画（run / skip）だけ見られる。
- `-j N` で並列度（既定 1）。並列 run は clone・`run.db` が別なので干渉しないが、プロバイダのレート制限と CPU/ビルド負荷に注意。
- `status` が `error` の結果（資格情報の誤りなど）は `--retry-errors` で消して再実行する。`timeout` は再実行しない。
- 予備実行（5 モデル × 6 課題 × 1 回 = 30 run）: `run-matrix.sh --outroot <dir>`。その後 `--reps 3` で 3 回に増やすと、既にある rep 1 はスキップされる。
- stage 2（run_js 無し対照）は別の `--outroot` に `--run-js false` で流す。

### 5. 検証

`run-one.sh` が自動で実行する。手動で行う場合は clone に対して `verify/<id>.sh <clone>` を走らせる（合格 = exit 0 / 不合格 = 1 / 使い方の誤り = 2）。LLM による採点の観点は各 `tasks/<id>.md` の「採点基準」にある。

### 6. 指標の集計（`metrics.mjs`）

bun か node（npm 依存なし）。

```sh
node $EVAL/evals/run-js/scripts/metrics.mjs --results <outroot> [--json out.json] [--md out.md] [--format md|json|both]
node $EVAL/evals/run-js/scripts/metrics.mjs --db <path> --run <run-id>[=<model>/<task>/<rep>] [--run ...]
node $EVAL/evals/run-js/scripts/metrics.mjs --fixture test/fixture-metrics.json
```

標準出力は Markdown（既定）。表は 3 つ: run ごと / モデル × 課題（平均。`run_js` 有効・無効は別行）/ shell コマンドの複雑さ分布。JSON には全指標（run ごとの入れ子の形と集計）が入る。`--db` を直接読むときは `--evorch <bin>`（既定 `$EVORCH_BIN`）で `evorch inspect` を呼ぶ。

#### 指標の定義

データ源: `evorch inspect run`（`usage_requests`、`context`）と `evorch inspect events --run … --limit 10000`（`ToolStarted` / `ToolCompleted`。イベントが上限で切れたら注意欄に出す）。

**リクエスト・トークン・費用**（`usage_requests` の行単位）

| 指標 | 定義 |
|---|---|
| `requests` | `purpose` が `agent`（または欠落）の行数。失敗した試行も含む（`requests_failed` に内数） |
| `aux_requests` | それ以外の行数（`compaction` / `title` / `routing` / `escalation_review` など）。種別は `aux_purposes` |
| `input` / `cache_read` / `cache_write` / `output` / `reasoning` | agent 行の `input_tokens` / `cache_read_tokens` / `cache_write_tokens` / `output_tokens` / `reasoning_tokens`（null は 0）の合計。台帳の値そのままで、`input` が `cache_read` を含むかはプロバイダ依存の可能性がある（未確認）。モデル間比較は費用か 3 種の和で行う |
| `cost_usd` / `aux_cost_usd` / `total_cost_usd` | agent 行 / それ以外 / 全体の `cost_usd` 合計。価格不明の行（null）は 0 扱いで、数を `unpriced_requests` と注意欄に出す |

**ツール**

| 指標 | 定義 |
|---|---|
| `tools.top_level_calls` | `parent_call_id` を持たない `ToolStarted` の数（`by_name` にツール別）。`top_level_errors` は対応する `ToolCompleted.is_error` の数 |
| `run_js.calls` | 上のうち `tool_name = run_js` |
| `run_js.child_calls` | 各 `run_js` の `detail.run_js.calls` の件数 + `calls_omitted` の合計（host に拒否された呼び出しを含む試行数） |
| `tools.child_executed` / `child_by_tool` | `parent_call_id` を持つ `ToolStarted`（実際に実行した子呼び出し）の数とツール別 |
| `run_js.output_bytes` / `return_bytes` | `detail.run_js` の `output_bytes` / `return_bytes` の合計 |
| `run_js.code_chars` | `run_js` の `input.code` の文字数の合計 |

**`run_js` のエラー**（`run_js.errors`。1 つの `run_js` 呼び出しが複数に数えられることがある）

| 指標 | 定義 |
|---|---|
| `js_exception` | 親 `ToolCompleted.is_error` で、`timed_out` でも `limit_hit` でもない（JS の例外、worker 異常終了を含む） |
| `host_rejected` | 子呼び出しのうち `detail.calls[].is_error` だが、対応する子 `ToolCompleted` の `is_error` が無いもの。host が実行せず拒否した呼び出し（利用不可ツール、権限・承認が要る、引数不正、呼び出し数上限） |
| `child_tool_error` | 子呼び出しが実行されてツール側でエラーになった数（`ToolCompleted.is_error`） |
| `timeout` | `detail.run_js.timed_out` が真 |
| `limit_hit` | `detail.run_js.limit_hit` が `timeout` 以外（`tool_calls` / `memory` / `output`。種別は `limit_hit_kinds`） |
| `pre_run_error` | `detail.run_js` が無いエラー（引数不正、`timeout_ms` の範囲外、host 不在） |

**shell**（トップレベルの `shell` 呼び出しのうち `command` を持つ開始呼び出しが対象。`poll` / `stdin` / `stop` は `control_calls`。`run_js` 内の shell は `child_commands` に件数だけ）

コマンド文字列を次のように前処理してから数える: ヒアドキュメント本体、引用符（`'…'` `"…"`）の中身、コメントを取り除く。

| 指標 | 定義 |
|---|---|
| `heredoc_cmds` | `<<` / `<<-`（`<<<` を除く）を含むコマンドの数（`here_string_cmds` は `<<<`） |
| `interp_inline_cmds` | インライン実行のインタプリタを含むコマンドの数。対象は node / bun / deno（`-e` `-p` `--eval` `--print` `-`、またはヒアドキュメント・here-string 入力）、python（`-c` `-` または heredoc）、perl（`-e` `-E` 系または heredoc）、ruby（`-e`）、awk / gawk / mawk（`-f` 以外）。コマンド位置の語だけを見る（`echo "node -e"` は数えない。`xargs` `env` `sudo` `time` と環境変数代入は読み飛ばす）。言語別は `interp_inline_by_lang` |
| `interp_file_runs` | スクリプトファイルの実行（`node x.mjs`、`python3 x.py`、`awk -f`）。インラインではない |
| `jq` | `interp_inline_by_lang.jq` に別枠で数える（`interp_inline_cmds` には含めない） |
| `oneliner_cmds` | 改行を 1 つも含まないコマンドの数 |

**ワンライナーの複雑さ**（ADR 0039 Open question「予備実行で決める」への初期案。予備実行の分布を見て閾値を見直す）

コマンドごとに次の 5 指標を測り、run ごとに分布（件数・平均・中央値・p90・最大）を出す。重み付きの総合点は作らない（分布そのものを見る）。

| 指標 | 定義 |
|---|---|
| `pipes` | パイプ演算子（`\|`、`\|&`）の数 = パイプライン段数 − パイプライン数。`\|\|` と `>\|` は含めない |
| `chain_ops` | 連結演算子 `&&` `\|\|` `;` とバックグラウンド `&` の数（`2>&1` `&>` の `&` は含めない） |
| `segments` | 上の演算子と改行で区切った非空の単純コマンドの概数（`then` `do` `fi` などの制御語も 1 つに数える）。帯 `1` / `2-3` / `4-7` / `8+` の件数も出す |
| `chars` | コマンド文字列全体の文字数（ヒアドキュメント本体を含む） |
| `lines` | 行数（ヒアドキュメント本体を含む） |

目安として、`segments ≥ 4` または `chars ≥ 300`、あるいはインライン・インタプリタ／ヒアドキュメントを含むものを「ワンライナーを超えた複雑なコマンド」と見て、`run_js` に書くべきだったかを目視で確認する。

**その他**: `verify`（`result.json` の `pass` / `fail` / `timeout` / `error`）、`status`、`duration_s`（`result.json`、無ければイベントの最初〜最後）、`terminal_phase`。モデル × 課題の表は、同じ（モデル, 課題, `run_js` 有効/無効）の run の各数値の平均と `verify pass` の件数。

## 繰り返し

予備の実行は 5 モデル × 6 課題を 1 回ずつ（30 run）。その後、ばらつきを見るため各条件を 3 回程度に増やす。run ごとに clone は作り直す（`run-one.sh` が行う）。

## スクリプトの確認（実モデルを呼ばない）

```sh
node test/shell-analysis-test.mjs        # shell 解析の単体確認
node test/metrics-test.mjs               # fixture-metrics.json に対する metrics.mjs の期待値
test/scripts-test.sh <存在しない作業ディレクトリ>   # ダミー evorch で make-overlay / run-one / run-matrix / metrics の流れを確認
```

`test/fixture-metrics.json` は `evorch inspect` の出力形式に合わせた合成データ（`test/gen-fixture.mjs` で再生成）。`test/dummy-evorch.sh` は `EVORCH_BIN` に指定できるダミー（引数を記録し、`run_id:` を出し、fixture の `inspect` 出力を返す）。

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
