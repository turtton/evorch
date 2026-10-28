# run_js 第 1 段階の観察用 課題セット

ADR 0039（`intents/evorch/decisions/0039-js-code-mode-tool.md`）の「評価 / 第 1 段階」で使う課題セット。題材は evorch 自身の固定コミット `4391a7e5`（`run_js` 未実装の時点）。Worker に `run_js` を有効にして課題を解かせ、ターン数・トークン・`run_js` の使い方・shell の使い方・品質を観察する。

各課題のプロンプトは `run_js` の使用を指示しない（自然に使うかを見る）。課題ごとに「正解で exit 0、未着手で exit 1」になる検証コマンドを付けてある。

## 課題一覧

予備実行（`results/prelim-20261028/`）は、どの run も 3〜13 リクエスト・2 分以内で終わった。v2 に重くした 2 回目の観察（`results/r2-20261028/`）でも 5〜22 リクエストで、`run_js` の効果（ターン数の削減）を測れる規模にならなかった。モデルは集計・改名を 1 本のスクリプト（python / awk / sed）で片づけるので、「ファイルや箇所の数」を増やしただけでは往復が増えない。そこで **v3** は、素直な解き方（`run_js` なし）で 1 run あたり 20〜35 リクエストになるように、往復が**増えざるをえない**形にした:

- 入力の種類を増やす（t2: Rust・`Cargo.toml`・`git log`・ADR の markdown・全ファイルの拡張子表を別々に集計して合成する。パースの書き方を一から決めて試す必要がある）。
- 読まないと分からない項目を増やす（t1: 17 個の `impl` ブロックを読まないと `overrides` と実行モードの既定値が分からない）。
- コンパイラ・テストの出力を見て直す往復を作る（t3: フィールド名の改名は単語置換では通らず、`cargo check` のエラー位置に従って 4〜5 周直す。t4: 変更で既存テストが落ち、clippy が unused import を出す。t6: 失敗が crate・テストバイナリに分かれて 1 つずつ現れる）。
- 3 crate にまたがらせる（t3 / t4 / t6）。

プロンプトはどれも `run_js` の使用を指示しない（自然に使うかを見る）。全課題（v3）のプロンプトは、確認の質問で止まらないように「確認の質問はしないでください。曖昧な点は妥当な判断で進め、判断と仮定を `answers/notes.md` に書いてください」で終わり、変更範囲の制限には `answers/` への書き込みの例外を明記する（`scripts/lint-prompts.sh` で点検する）。課題ごとに「正解で exit 0、未着手で exit 1、部分誤答で exit 1」になる検証コマンドと参照解（`solutions/`）を付けてある。

| id | 種類 | 内容 | 検証が見るもの | 想定ターン数 |
|---|---|---|---|---|
| [`t1-survey-v3`](tasks/t1-survey-v3.md) | (1) 調査と要約 | `tools` 9 + `eval` 7 + `lsp_diagnostics` の 17 ツールの name・source・実行モード（トレイトの既定は Exclusive）・権限・`overrides`（`impl` ブロックのメソッド名）・`allowed_roles`、6 ディレクトリでの名前の参照ファイル数（102 回の走査）、`by_role`、`ref_ranking` | `answers/t1.json` の 251 項目 | 9〜15（`run_js` あり。実測 9〜13）／ なし 15〜30 |
| [`t2-aggregate-v3`](tasks/t2-aggregate-v3.md) | (2) 集計 | 6 入力の集計の合成: Rust の crate 別 6 指標、`Cargo.toml` の内部依存グラフ（`dependents`・`transitive_deps`・`layer`）、`git log -200` の crate 別コミット数・変更行数・最終日、ADR 44 件の状態と被参照上位、最上位ディレクトリ × 拡張子の表、`hotspots`（4 つの結果から作る順位） | `answers/t2.json` の 399 項目 | 7〜15（`run_js` あり。実測 7〜15）／ なし 12〜25 |
| [`t3-rename-v3`](tasks/t3-rename-v3.md) | (3) 複数か所の編集 | (A) `event-bus` の 3 識別子（コード 233 箇所・約 30 ファイル、コメント行は不変）。(B) `apply-patch` の 4 つの型のフィールド名（`AppliedChange.path` など。同名の別のフィールドがあり、tools にも使用箇所 92 行）をコンパイラのエラーに従って直す | event-bus のファイル別の件数、4 定義、コメント行の不変、`cargo test` event-bus / apply-patch と `cargo check -p tools --tests` | 20〜50（実測 21〜49） |
| [`t4-build-test-v3`](tasks/t4-build-test-v3.md) | (4) ビルド・テストを含む手順 | 3 crate で、テスト数を控える → agents（権限変更・既存テスト 2 件の修正・`Role::ALL`・`roles_allowing`）→ apply-patch（`ChangeKind::label`・`ChangeCounts`）→ event-bus（`error_type` と `map_failure` の置き換え、clippy の unused import）→ fmt / clippy / test → 報告 JSON | 変更範囲 + 既存テスト数の下限 + doc 表 + `map_failure` の削除 + fmt + clippy + test（3 crate）+ 隠しテスト（3 crate）+ 報告 JSON | 20〜35 |
| [`t5-single-edit-v3`](tasks/t5-single-edit-v3.md) | (5) 1 回で済む対照 | `grep.rs` の定数と doc コメントの数字を 1 回の編集で変える（v1・v2 と同内容。対照なので重くしない） | 差分がちょうど +2/-2 行 | 2〜4 |
| [`t6-debug-fix-v3`](tasks/t6-debug-fix-v3.md) | (6) 調査してから直す | 4 コミットで仕込んだ 8 つの退行（apply-patch 4・event-bus 3・agents 1）。コミット c は新機能 `ChangeKind::label` も含み、コミットごとの revert では消える | 変更範囲 + 7 ファイルの `#[cfg(test)]` 以降が無改変 + 3 crate の `cargo test` + 隠しテスト（新機能が残る）+ 報告に 7 ファイル | 20〜35 |

**想定ターン数の考え方**: リクエスト数は「走査・集計を何回の呼び出しにまとめられるか」で大きく変わる。第 3 回の実測では、t1 / t2 は `run_js` で読み取り・走査・集計が 1〜3 回にまとまり、v3 でも 7〜15 リクエスト（`run_js` の子呼び出しは 0〜772 件）だった。一方 t3 はコンパイラ駆動の改名の周回数で 21〜49 リクエストになる。そこで各 `tasks/*.md` は、リクエスト数ではなく**処理量**（読むファイル数・走査回数・入力の種類・子呼び出し数・コンパイルの周回数）を主な指標として書き、リクエスト数は「`run_js` あり（実測）」と「なし（処理量からの見積り）」を分けて示す。

**費用**: 第 3 回の実測（暴走なし）で、**1 rep（全課題 × 4 モデル）は約 $11〜12**。第 2 回の約 $3.9 の約 3 倍で、目安（2〜3 倍）の上端に収まる。第 3 回の run 別の費用は、t1 が $0.02〜0.70、t2 が $0.01〜1.06、t3 が $0.06〜1.26（opus・astra・luna。モデルごとの 1 リクエストあたりは opus 約 $0.025、sonnet 約 $0.014、astra 約 $0.04、luna 約 $0.003）。ただし第 3 回には**暴走した run**（claude-sonnet × t3、完了後の同じ通知に 487 リクエスト応答し続けて $12.6、30 分 timeout まで）があり、費用の見積りはこの種の暴走を止められることが前提になる。止める仕組みは次の 2 つ:
- run ごと（`run-one.sh`、既定で有効）: リクエスト数 150 超、費用 $3 超、直近 4 件の同一応答のどれかで打ち切り（`status=aborted`、下の「watchdog」）。
- マトリクス全体（`run-matrix.sh --cost-limit <usd>`、指定したとき）: 同じ (rep, 課題) の区切りごとに累計費用を見て、上限を超えたら次の区切りを始めずに止まる（第 3 回は外部のスクリプトで行っていたものを正式なオプションにした）。

ビルド・テストを走らせる課題（t3 / t4 / t6）は agents・apply-patch・event-bus・tools の `cargo check` だけで、libclang が要る crate（routing・storage・recorder などは providers 経由で bindgen を使う）は避けてある。ワークスペース全体のビルドは要らない（依存の解決だけは必要）。

旧課題:

| 世代 | id | 備考 |
|---|---|---|
| v2（`EVAL_TASKS_V2`） | `t1-survey-v2` … `t6-debug-fix-v2` | 予備実行の次。プロンプトに確認質問を止める文がなく、t1 / t4 に矛盾があった（`ecd7467b` で直した） |
| v1（`EVAL_TASKS_V1`） | `t1-survey` … `t6-debug-fix` | 最初の課題セット |

結果ディレクトリは課題 id で分かれるので、世代が混ざることはない。

## ディレクトリ構成

```
evals/run-js/
  README.md
  tasks/<id>.md          課題の定義（プロンプト全文、想定解、採点基準、検証、ターン数、run_js が効く理由）
  verify/<id>.sh         検証コマンド。`verify/<id>.sh <clone>`、合格 = exit 0 / 不合格 = exit 1 / 使い方の誤り = exit 2
  verify/expected/       t1 / t2 の期待値（JSON。v2 は scripts/gen-v2-expected.mjs、v3 は scripts/gen-v3-expected.py で再計算できる）
  verify/hidden/         t4 の隠しテスト（検証時だけ clone にコピーする）
  verify/lib/            検証の共通部
  setup/                 課題固有の初期化（t6 の退行コミット。v2 は 3 コミット、v3 は 4 コミット）
  solutions/<id>.sh      参照解（検証スクリプトの自己確認用。モデルには見せない）
  solutions/<id>.wrong-*.sh  部分誤答（検証が fail になることの確認用）
  scripts/               prepare-clone.sh / prompt.sh / self-test.sh / lint-prompts.sh / gen-t2-expected.mjs / gen-v2-expected.mjs / gen-v3-expected.py（課題・検証）
                         make-overlay.sh / preflight.sh / run-one.sh（watchdog 込み）/ last-events.mjs / run-usage.mjs / run-matrix.sh / outroot-cost.mjs / models.sh（実行）
                         metrics.mjs / classify-shell.mjs（集計）
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
  [--run-js true|false] [--effort <e>] [--timeout <sec>] [--idle-timeout <sec>] [--max-requests <n>] [--max-cost <usd>] [--repeat-limit <n>] [--keep-clone] [--force]
```

流れ: overlay 生成 → `prepare-clone.sh`（t6 は退行コミットを積む setup を含む）→ `prompt.sh` → `evorch run`（`timeout` 付き、既定 30 分 = `EVAL_TIMEOUT_S`）→ `verify/<id>.sh`（既定 15 分 = `EVAL_VERIFY_TIMEOUT_S`。タイムアウトした run も検証する）→ 結果の保存 → clone と overlay の削除。clone は `${EVAL_WORK:-${TMPDIR:-/tmp}/run-js-eval-work}` に作り、clone ごとに `target/` を持つ（`CARGO_TARGET_DIR` は共有しない）。

環境変数: `EVORCH_BIN`（必須）、`EVORCH_CONFIG_SRC`、`EVAL_WORK`、`EVAL_SOURCE_REPO`（`prepare-clone.sh` の source-repo）、`EVAL_IDLE_S` / `EVAL_WATCHDOG_POLL_S` / `EVAL_MAX_REQUESTS` / `EVAL_MAX_COST_USD` / `EVAL_REPEAT_LIMIT` / `EVAL_USAGE_POLL_S`（watchdog、下記）。

**watchdog（入力待ち・暴走した run を打ち切る）**: evorch は、モデルが確認の質問などでターンを終えると、入力待ち（`AgentRunStateChanged` Running → Waiting、続いて `TurnCompleted`）のまま終了しない。第 2 回の観察では、そのまま 30 分の `timeout` まで待った run が複数あった（t1 の「8 個か 9 個か」、t4 の「answers への書き込みは範囲外か」）。評価の Worker は対話しないので、`run-one.sh` は evorch を動かしている間 `--idle-timeout`（既定 `$EVAL_IDLE_S` = 60 秒、0 で無効）の watchdog を回す。
- 方式: `EVAL_WATCHDOG_POLL_S`（既定 5 秒）ごとに、run の DB（`--db`）を `evorch inspect events --run <id> --limit 12` で読み、`scripts/last-events.mjs` で末尾の状態（最後のイベントの種類・`RunProgress` の activity・状態・最後の `MessageCompleted` の本文）を取り出す。**最後のイベントが `TurnCompleted` のまま、イベント ID が `idle-timeout` 秒だけ動かなければ、evorch を `kill`（TERM、10 秒後に KILL）する**。通常の終了は `Running → Done` で `TurnCompleted` が最後にならず、すぐ evorch が終わるので誤検出しない（成功した run と停止した run の実データで確かめた）。DB が読めない間は何もしない。
- 結果: `status` は `stalled`、`error_reason` に `stalled: TurnCompleted のあと 60s 動かず（activity=User state=Waiting）。最後の発言: …`（モデルの質問の冒頭）が入る。`stalled` の後も検証は走るので、成果物が揃っていれば `verify=pass`（metrics の `outcome` は `stalled+pass`）。`--retry-errors` の対象は `error` だけで、`stalled` と `aborted` は再実行しない（モデルの振る舞いの観察結果であるため。再実行するなら `--force`）。
- **暴走（`status=aborted`）**: 第 3 回に、claude-sonnet × t3-v3 が作業を終えたあとも、同じ nudge（実行中のジョブの通知）に 487 リクエストにわたって同じ返事をし続け、$12.6 を使って 30 分の timeout まで走った（検証は pass）。次の 3 つのどれかで `kill` して `status=aborted`、`abort_kind` に種類、`error_reason` に理由（件数・費用・繰り返された応答）を残す。打ち切った後も検証は走る（`outcome` は `aborted+pass` など）。
  - `requests`: `usage_requests` の agent 行が `--max-requests`（既定 150。`EVAL_MAX_REQUESTS`）を超えた。`evorch inspect run <id>` を `EVAL_USAGE_POLL_S`（既定 10 秒）ごとに読み、`scripts/run-usage.mjs` で件数を数える（metrics の `requests` と同じ定義）。
  - `cost`: `usage_requests` の `cost_usd` の合計が `--max-cost`（既定 $3。`EVAL_MAX_COST_USD`）を超えた。同じ `inspect run` から求める。
  - `repeat`: 直近の（空でない）アシスタント応答（`MessageCompleted` の本文）が `--repeat-limit`（既定 4。`EVAL_REPEAT_LIMIT`）件続けて同一。`inspect events --limit 150` の末尾から `last-events.mjs` が数える。第 2・3 回の全 run（正常終了のもの）で、同一の応答が連続した run は暴走の 1 件だけだった（最大連続 469）ので、4 件なら誤検出しない。
  - いずれも 0 で無効。上限に達する前に止めるわけではなく、確認の間隔の分だけ超過しうる（リクエスト数 10 秒分、費用は 1 リクエスト分程度）。metrics は `aborted` の run の費用・リクエスト数を「打ち切るまでの値」と注記し、モデル × 課題の表に `aborted` の件数を出す。
- evorch 側の修正（入力待ちにならない `run`、nudge ループの修正）が入っても、ハーネス側の防御として残す。

結果は `<outroot>/<model>/<task>/<rep>/`（`<outroot>` はリポジトリの外に置く）:

| ファイル | 内容 |
|---|---|
| `result.json` | `run_id`、`status`（`ok` / `error` = evorch が非 0 終了 / `timeout` / `stalled` = 入力待ちを watchdog が打ち切った / `aborted` = 暴走を watchdog が打ち切った。`abort_kind` = `requests` / `cost` / `repeat`）、`evorch_exit`、`idle_timeout_s`、`max_requests`、`max_cost_usd`、`repeat_limit`、`error_reason`（`status` が `ok` でないときだけ。stderr の `reason=` 以降、なければ最後の非空行。`stalled` は最後の発言つきの上の文。300 文字まで）、`duration_ms`、`verify`（`pass` / `fail` / `timeout` / `error`）、`verify_exit`、`run_js_enabled`、`profile`、`model_id`、`effort` など。**最後に書く**ので、あれば完了扱い |
| `run.db` | この run の store（`--db`）。run ごとに別ファイルなので並列でも衝突しない |
| `evorch.stdout.log` / `evorch.stderr.log` | Worker の最終出力 / `run_id:` を含む stderr |
| `verify.log` | 検証の出力（不一致一覧） |
| `final.diff` / `answers/` | 固定コミットからの追跡ファイル差分 / `answers/` のコピー |
| `inspect-run.json` / `inspect-events.json` | run 直後の `evorch inspect` の出力（`metrics.mjs` が優先して読む。無ければ `run.db` を読む） |
| `zz-worker.toml` | この run で使った Worker のモデル設定 |

終了コード: 0 = 保存した（verify の合否は問わない）/ 結果が既にある、2 = 使い方や環境の誤り、3 = 準備に失敗（結果は書かない）。`result.json` が無いディレクトリは作り直す。

**`--force` と退避**: `--force` で結果がある組を実行し直すときは、既存の結果ディレクトリを消さず `<rep>.attempt<N>`（N は未使用の最小の番号）へ移す。`<rep>` に `.attempt<N>` で終わる名前は使えない。

**`status=error` だが `verify=pass` の run**: provider の SSE エラー（`invalid SSE stream: …` など）で evorch が非 0 終了しても、止まる前に成果物が正しく出来ていることがある。このとき `status` は `error`、`verify` は `pass` で、`error_reason` に理由が入る。`run-one.sh` の出力行には `(error だが verify=pass)` が付き、`metrics.mjs` は `outcome` を `error+pass` と表示して注意欄に理由を出す。ターン数や費用の比較では `ok` の run と分けて見る（途中で止まっているため）。

### 4. マトリクスの実行（`run-matrix.sh`）

```sh
$EVAL/evals/run-js/scripts/run-matrix.sh --outroot <dir> \
  [--models "gpt-sol claude-opus"] [--tasks "t1-survey-v3 t5-single-edit-v3"] \
  [--reps 3] [--rep-from 1] [-j 2] [--run-js true|false] [--effort <e>] [--timeout <sec>] [--idle-timeout <sec>] \
  [--max-requests <n>] [--max-cost <usd>] [--repeat-limit <n>] [--cost-limit <usd>] \
  [--retry-errors] [--no-preflight | --preflight-only] [--list] [--keep-clone]
```

- 既定は全 5 モデル × v3 の全 6 課題（`EVAL_TASKS`）× 1 回。順序は rep → 課題 → モデル。
- `result.json` がある組はスキップするので、中断しても**同じコマンドで再開**できる。`--list` で実行せずに計画（run / skip）だけ見られる。
- `-j N` で並列度（既定 1）。並列 run は clone・`run.db` が別なので干渉しないが、プロバイダのレート制限と CPU/ビルド負荷に注意。
- `status` が `error` の結果（資格情報の誤りなど）は `--retry-errors` で再実行する。最初の試行は消さず、`<model>/<task>/<rep>.attempt<N>` へ退避してから流す（`metrics.mjs` は既定で読まない。`--include-attempts` で読む）。`timeout` は再実行しない。`error` でも `verify=pass` だった結果も、`status` が `error` なので対象になる。
- **`--cost-limit <usd>`（費用の停止線）**: 同じ (rep, 課題) の組（モデルは `-j` で並列）を 1 つの区切りとして順に流し、区切りの前後に結果ディレクトリの累計費用（`scripts/outroot-cost.mjs`。全 run の `inspect-run.json` の `cost_usd` の合計で、退避された `.attempt<N>` と打ち切られた run を含む）を見る。上限を超えていたら次の区切りを始めず、「費用の停止線で止まった」と出して**終了コード 4**。超過は最大で 1 区切り分（run ごとの `--max-cost` と組み合わせる）。上限を上げて同じコマンドで再開できる（既にある結果はスキップ）。`--cost-limit` なしは従来どおり全体を 1 つの並列キューで流す。
- **preflight**: 実行前に、計画に残っている各モデルを `preflight.sh` で確かめる。Worker をそのモデルに固定した overlay で、極小のプロンプト（ツール不使用で `OK` と答えさせる、`--run-js-enabled false`）を 1 回流し、evorch が 0 で終われば使える。provider が advertise しないモデル（`model is not advertised by provider`）や、config にないモデル（`the model chain exists but has no available candidate`）は、モデル呼び出しの前に失敗するので費用はかからない（予備実行の gpt-6.1-sol は 4 秒で失敗した）。使えるモデルでは極小のリクエストが 1 回出る。使えないモデルの組は計画から外し、結果ディレクトリも作らず、使えるモデルだけを流して最後に exit 1 で報告する（直して同じコマンドで再開できる）。成功は `<outroot>/.preflight/<model>.ok` に記録し、再実行では確認を飛ばす（`preflight.sh <model> --outroot <dir> --force` で確かめ直す）。`--no-preflight` で省略、`--preflight-only` で確認だけ。`--list` は preflight をしない。
- 予備実行（5 モデル × 6 課題 × 1 回 = 30 run）: `run-matrix.sh --outroot <dir>`（v3 を流す）。その後 `--reps 3` で 3 回に増やすと、既にある rep 1 はスキップされる。
- stage 2（run_js 無し対照）は別の `--outroot` に `--run-js false` で流す。

### 5. 検証

`run-one.sh` が自動で実行する。手動で行う場合は clone に対して `verify/<id>.sh <clone>` を走らせる（合格 = exit 0 / 不合格 = 1 / 使い方の誤り = 2）。LLM による採点の観点は各 `tasks/<id>.md` の「採点基準」にある。

### 6. 指標の集計（`metrics.mjs`）

bun か node（npm 依存なし）。

```sh
node $EVAL/evals/run-js/scripts/metrics.mjs --results <outroot> [--json out.json] [--md out.md] [--format md|json|both] [--include-attempts]
node $EVAL/evals/run-js/scripts/metrics.mjs --db <path> --run <run-id>[=<model>/<task>/<rep>] [--run ...]
node $EVAL/evals/run-js/scripts/metrics.mjs --fixture test/fixture-metrics.json
```

標準出力は Markdown（既定）。表は 3 つ: run ごと / モデル × 課題（平均。`run_js` 有効・無効は別行）/ shell コマンドのプログラム的構造（run ごと）。JSON には全指標（run ごとの入れ子の形と集計）が入る。`--db` を直接読むときは `--evorch <bin>`（既定 `$EVORCH_BIN`）で `evorch inspect` を呼ぶ。

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
| `heredoc_cmds` | `<<` / `<<-`（`<<<` を除く）を含むコマンドの数（`here_string_cmds` は `<<<`）。受け手で分けた件数が `heredoc_by_kind`（下記） |
| `interp_inline_cmds` | インライン実行のインタプリタを含むコマンドの数。対象は node / bun / deno（`-e` `-p` `--eval` `--print` `-`、またはヒアドキュメント・here-string 入力）、python（`-c` `-` または heredoc）、perl（`-e` `-E` 系または heredoc）、ruby（`-e`）、awk / gawk / mawk（`-f` 以外）。コマンド位置の語だけを見る（`echo "node -e"` は数えない。`xargs` `env` `sudo` `time` と環境変数代入は読み飛ばす）。言語別は `interp_inline_by_lang` |
| `interp_file_runs` | スクリプトファイルの実行（`node x.mjs`、`python3 x.py`、`awk -f`）。インラインではない |
| `jq` | `interp_inline_by_lang.jq` に別枠で数える（`interp_inline_cmds` には含めない） |
| `oneliner_cmds` | 改行を 1 つも含まないコマンドの数 |

**ワンライナーの複雑さ**（ADR 0039 Open question「予備実行で決める」への案。予備実行の 84 コマンドで検討し、v2 の予備実行の分布を見て閾値を見直す）

`chars` の大小や `segments` だけでは、無害な長い 1 行（`cd` や `ls` を並べた確認、`cargo fmt && cargo clippy && cargo test` の連結）を「複雑」と誤判定する。そこで、**主指標は「プログラム的構造」の有無**とし、`chars` と `segments` は補助にする。重み付きの総合点は作らない（分布と件数を見る）。コマンド文字列は前処理（ヒアドキュメント本体・引用符の中身・コメントを除く）した上で、`&&` `||` `;` `&` `|` と改行で単純コマンドに分けて判定する。

主指標（コマンド単位の真偽。`programmatic_cmds` はどれかに当てはまるコマンドの数、種類別は `programmatic_by_kind`）:

| 種類 | 定義 |
|---|---|
| ① `script` | インライン実行のインタプリタ本体を含む（python / node / bun / deno / perl / ruby の `-e` `-c` `-p` `-`・ヒアドキュメント・here-string 入力、`bash` / `sh` / `zsh` / `dash` へのヒアドキュメント、awk / gawk / mawk のプログラム。`awk -f` や `python3 x.py` などのファイル実行と `jq` は含めない）、**または**ファイルへ書いたヒアドキュメント（`cat > x.py <<EOF`）を、同じ run の同じコマンドか後のコマンドが実行した（下記の `write_run`）。ファイルへ書くだけのヒアドキュメントは含めない |
| ② `loop` | `for` / `while` / `until` のループ、または `xargs` によるファイル反復 |
| ③ `textproc` | 1 つのパイプラインに text 処理の段（grep / egrep / fgrep / rg / sed / awk / gawk / mawk / sort / uniq / cut / tr / wc）が 3 つ以上。`head` / `tail`（と `cat`、`git ls-files` などその他の段）は段に数えないので、末尾が `head` / `tail` だけの延長は当たらない。`;` や `&&` をまたいで数えない |
| ④ `sed_multi` | `sed -i`（`-i.bak`、`-Ei`、`--in-place`）で複数ファイルを書き換える。ファイル引数が 2 つ以上（glob は 2 と数える）、またはコマンドに `xargs` / ループがある。複数の `sed -i` が合計 2 ファイル以上になる場合も含む |

**ヒアドキュメントの受け手**（`heredoc_by_kind`。run ごとの件数。`heredoc_write_only_cmds` は、ヒアドキュメントがすべて `write_only` のコマンドの数）:

| 種類 | 定義 |
|---|---|
| `interp` | 受け手がインタプリタ（上の ① のインライン実行）。`python3 - <<'PY'`、`node <<EOF`、`bash <<EOF` など。script に数える |
| `write_run` | 受け手が `cat` / `tee` でファイルへ書く（`cat > x.py <<EOF`、`cat >> f <<EOF`、`cat <<EOF > f`、`tee f <<EOF`）うえ、**同じ run の同じコマンドか後のコマンドがそのファイルを実行した**（`python3 /tmp/x.py`、`node ./x.mjs`、`bash x.sh`、`./x.sh`、`source x.sh`、`awk -f x.awk`）。ファイル名（basename）で照合する。script に数える（書いたコマンドを数える。実行したコマンドは `interp_file_runs`） |
| `write_only` | ファイルへ書くだけで、同じ run で実行されない（ノート・設定・JSON の書き出し、`cat <<EOF` の表示など）。**プログラム的構造から除き**、別カテゴリとして数える。書き込み先が変数（`"$f"`）などで特定できないものもここに入る |
| `other` | 受け手が上のどれでもない（`git commit -F - <<EOF`、`patch <<EOF` など）。プログラム的構造に数えない |

照合は run ごとの top-level の shell コマンドの順で行う（`run_js` の中の shell は見えない）。ヒアドキュメントの本体の中の文字列（`python3 x.py` と書いたノートなど）は実行とみなさない。実行が書き込みより前のコマンドは照合しない。

補助指標:

| 指標 | 定義 |
|---|---|
| `segments` | 単純コマンドの数。ただし **`cd` / `pwd` / `ls` / `echo` / `which` / `git status`**、**cargo の build 系**（`cargo fmt` / `clippy` / `test` / `check` / `build` / `nextest`。`+toolchain` やフラグは読み飛ばす）、制御語だけの区切り（`then` `do` `else` `fi` `done` など）は数えない。数えたものを `segments`、全部の非空区切りを `segments_raw`、除いた内訳を `segments_excluded`（`trivial` / `build` / `control`）に出す。帯 `1` / `2-3` / `4-7` / `8+` の件数も出す（0 は帯 `1`） |
| `build_steps` / `build_chain_cmds` | 1 コマンドに含まれる cargo の build 系の数（`build_steps`、run ごとの分布と合計 `build_steps_total`）と、2 つ以上を連結したコマンドの数（**build 連結数** `build_chain_cmds`）。ADR 0039 の「ビルドと確認を 1 つのワンライナーに連結する」誤用の観察用で、`segments` や主指標とは別に数える |
| `pipes` | パイプ演算子（`\|`、`\|&`）の数 = パイプライン段数 − パイプライン数。`\|\|` と `>\|` は含めない |
| `chain_ops` | 連結演算子 `&&` `\|\|` `;` とバックグラウンド `&` の数（`2>&1` `&>` の `&` は含めない） |
| `chars` / `lines` | コマンド文字列全体の文字数 / 行数（ヒアドキュメント本体を含む。補助にとどめる） |

`scripts/classify-shell.mjs --results <outroot> [--list]` は、同じ定義でモデル別の件数表（ヒアドキュメントの受け手の表を含む）を出し、`--list` で該当コマンド（`write_only` も）の本文を列挙する（`run_js` に書くべきだったかの目視確認用）。以前の臨時の分類スクリプトを取り込んだもので、`metrics.mjs` と同じ `analyzeShell` を使う。

**その他**: `verify`（`result.json` の `pass` / `fail` / `timeout` / `error`）、`status`、`outcome`（`ok` / `error` / `timeout` / `stalled` / `aborted`、および `status` が `ok` でないのに `verify=pass` の **`error+pass`** / `timeout+pass` / `stalled+pass` / `aborted+pass`。成果物は正しいが途中で止まった run を区別する）、`error_reason`、`duration_s`（`result.json`、無ければイベントの最初〜最後）、`terminal_phase`。モデル × 課題の表は、同じ（モデル, 課題, `run_js` 有効/無効）の run の各数値の平均と `verify pass` の件数、`err+pass` の件数。

## 繰り返し

予備の実行は 5 モデル × 6 課題を 1 回ずつ（30 run）。その後、ばらつきを見るため各条件を 3 回程度に増やす。run ごとに clone は作り直す（`run-one.sh` が行う）。

## スクリプトの確認（実モデルを呼ばない）

```sh
node test/shell-analysis-test.mjs        # shell 解析（プログラム的構造・segments・build 連結など）の単体確認
node test/metrics-test.mjs               # fixture-metrics.json に対する metrics.mjs の期待値、結果ディレクトリの読み分け、outcome
test/scripts-test.sh <存在しない作業ディレクトリ>   # ダミー evorch で make-overlay / preflight / run-one / run-matrix / metrics の流れを確認
```

`test/fixture-metrics.json` は `evorch inspect` の出力形式に合わせた合成データ（`test/gen-fixture.mjs` で再生成）。`test/dummy-evorch.sh` は `EVORCH_BIN` に指定できるダミー（引数を記録し、`run_id:` を出し、fixture の `inspect` 出力を返す。`DUMMY_UNAVAILABLE_MODEL` で provider の `model is not advertised by provider` を、`DUMMY_ERROR_REASON` で provider エラーの stderr を再現する）。preflight の実装は、ダミーでの確認に加えて、存在しないモデル名を固定した overlay で実バイナリを 1 回起動し、モデル呼び出しの前に失敗して exit 1 になること（費用ゼロ）を確かめた。

`metrics.mjs --results <dir>` は、`<dir>/<model>/<task>/<rep>/result.json` を持つ組だけを読む。`result.json` の `model` / `task` / `rep` がパスと一致しないもの（別の outroot を直下に置いた、手で退避したディレクトリなど）、`.`始まりのディレクトリ（`.preflight/`）、`timeline/` のような `result.json` を持たないディレクトリは読まない（読まなかった理由は stderr に `skip …` で出す）。`<rep>.attempt<N>` は `--include-attempts` のときだけ読む。結果が 1 つも見つからなければ、`--outroot` を指していない可能性を示して exit 1 にする。

## 検証の自己確認

検証スクリプトとその期待値が壊れていないことを、各課題で次の 3 通りで確かめる。

| 状態 | 作り方 | 期待 |
|---|---|---|
| `untouched` | 未着手の clone | fail（exit 1） |
| `solved` | 参照解 `solutions/<id>.sh` を適用 | pass（exit 0） |
| `wrong-*` | 部分誤答 `solutions/<id>.wrong-*.sh` を適用 | fail（exit 1） |

```sh
scripts/self-test.sh <存在しない作業ディレクトリ> [task-id ...]   # 省略時は v2 の 6 課題
```

部分誤答の例（v3）: t1 は実行モードの既定値の取り違え（Shared と決めつけ）と `overrides` の欠落、t2 は `tokio::test` の取りこぼし（観察された誤り）と `git log -200 -- <path>`（パスで絞ってから 200 件）、t3 はコメント行まで置換 / (B) をしない / `AppliedChange.raw` まで巻き込む、t4 は壊れたテストを消す / 名前順に返す / `map_failure` を残す / `Moved` のラベルの誤り、t5 は doc コメントの直し忘れ、t6 は新機能ごと revert / `ring.rs` の直し忘れ / テストを緩める。v3 は `scripts/lint-prompts.sh`（プロンプトの静的な点検: `run_js` に触れない、確認質問をしない指示、`answers/notes.md`、変更制限に `answers/` の例外）も走らせる。

固定コミットが変わった場合や課題を変えた場合に再実行する。期待値は `scripts/gen-t2-expected.mjs <固定コミットの checkout>`（v1 の t2）、`scripts/gen-v2-expected.mjs <固定コミットの checkout> verify/expected`（v2 の t1 / t2）、`scripts/gen-v3-expected.py <固定コミットの checkout> verify/expected`（v3 の t1 / t2。Python 3.11+）で再計算できる。v3 の t1 は 9 ツールの値を v1 の期待値（ソースを読んで確定済み）と突き合わせ、t2 は `git log` / `wc` / `awk` / `grep` の別経路で agents・event-bus・config・runtime・storage・tools・ADR 2 の値を確認した。t3-v3 の (B) の参照差分 `solutions/t3-rename-v3-b.patch` は、コンパイルエラー駆動の置換（定義を直して `cargo check --tests -p apply-patch -p tools` を繰り返す）で作った。

## 観察された落とし穴（第 2 回までの実データ）

課題の解説（`tasks/*.md` の「落とし穴」）に加えて、実際の run で観察された失敗を書き残す。

- **t2 の `tokio::test` の正規表現（gpt-6-luna+fast、v2）**: `test_attrs` の正規表現を `#[(?:test\]|tokio\.test)`（`::` を `.` と書き間違え）にして、`#[tokio::test…]` を数えなかった。tools（509 → 140）・providers（607 → 264）・runtime（1,973 → 782）など tokio のテストが多い crate が大きく少なくなり、密度の順位も崩れて 23 / 165 項目が不一致になった。`scripts/self-test.sh` の `t2-aggregate-v3.wrong-tokio-regex` はこの誤りの再現（同じ値になる）。`#[tokio::test(flavor = …)]` のように引数付きも数える。
- **t2 の `git log -200 -- <path>`（v3 で追加）**: パスで絞ってから 200 件を取るので、「全体の最新 200 コミットのうち、その crate に触れたもの」より多くなる（agents は 13 でなく 28）。`wrong-path-limited-log` が再現。
- **t1 の「9 個か 8 個か」（gpt-6-astra、v2）**: プロンプトが「`tools/` 直下」と書いていたが `web_fetch` は `web_fetch/mod.rs`（サブディレクトリ）にあり、モデルが確認の質問をして入力待ちになった（30 分 timeout）。v3 は「以下（サブディレクトリを含む）」に直し、確認質問を止める文を足し、watchdog で 60 秒で打ち切る。
- **t4 の「変更範囲」と `answers/` の矛盾（gpt-6-astra、v2）**: 「`src/` と `tests/` 以外は変更しない」に `answers/t4.json` の書き込みが反したため、質問して止まった。v3 は全課題で `answers/` への書き込みの例外を明記し、`lint-prompts.sh` で点検する。
- **t3 の `run_js` の子呼び出し（gpt-6-astra、v2）**: 1 回の `run_js` が 200 件を超える子呼び出し（`read` 206 + `shell` 56 + `write` 21）をして、`tool_call_threshold`（200）の escalation と `max_file_rereads=20` の予算超過で `BudgetExhausted`（`status=error`、成果物は正しく `verify=pass`）になった。`outcome` は `error+pass`。多数ファイルの改名では、`run_js` 内の子呼び出しの数を見る（`run_js.child_calls` の指標）。
- **verify の sccache（ハーネス）**: `RUSTC_WRAPPER`（sccache）が長い cwd で動かず（`path must be shorter than SUN_LEN`）、第 2 回で t3 / t4 / t6 の検証がすべて cargo ごと落ちた（`ecd7467b` で、wrapper が動かないときは検証を wrapper なしで流すように直した）。Worker の shell は環境を絞られるので wrapper を持たない。
- **t4 の clippy の unused import（v3 で設計）**: `map_failure` を消すと `otel/mod.rs` の `use crate::event::{…, ProviderFailureKind, …}` がテスト以外で使われなくなり、`-D warnings` の clippy が落ちる。`cargo test` だけでは気づかない。

## 注意

- 課題の事実（t1 の表、t2 の数値、t3 の出現数、t4 のテスト件数 28 / 94 / 175、t6 の退行）は固定コミット `4391a7e5` に基づく。別のコミットでは使えない。
- `answers/` は clone の `.git/info/exclude` に入れてあり、`git status` や `git diff` には出ない。検証は `git diff 4391a7e5`（追跡ファイルの差分）で変更範囲を見る。
- t5 は `run_js` が不要な対照。t4 は `run_js` の中で cargo を走らせず通常の shell ジョブで流すのが望ましい（ADR 0039）。使い方の良し悪しは検証の合否とは別に観察する。
- t6 は `git log` / `git show` で退行コミットが見える。調査手段の観察対象であり、減点しない。
- t3-v2 / t3-v3 / t4-v3 / t6-v3 の検証は event-bus を `--features otel-exporter` 付きで流す（`span_exporter.rs` は feature 無しではコンパイルされない）。初回は opentelemetry の依存のビルドが入るので、依存の取得（`cargo fetch`）を済ませておく。
- 検証が cargo を走らせるため、並行して複数の検証や run を同じ clone で動かさない。
- v3 の t3 は `cargo check -p tools --tests` までで、tools の `cargo test` は流さない（この環境で 1 件落ちるテストが固定コミットの時点からある）。t3 / t4 / t6 の対象 crate は libclang が要らない。
