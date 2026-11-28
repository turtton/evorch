# ツール説明・プロンプトの評価と最適化の runbook

実際の失敗から作った sample で、モデルごとのツール説明とプロンプトを比べ、精度と context 量を改善するための手順。
既存の thread 再実行による評価（[ADR 0036](../intents/evorch/decisions/0036-thread-replay-evaluation.md)、末尾の追補）を使う。別の評価基盤は無い。

この文書が扱うのは、実装済みの次の範囲だけである。

- sample の checks（機械判定）と、記録に対する無料の採点
- 共通 overlay（`--common`）で両群のモデルを固定した baseline と candidate の比較
- `model_tool_descriptions`（モデル別のツール説明）と、role binding の appendix preset の比較
- report の指標の分離と判定保留

自動最適化、モデルの総当たり、短い sample の自動抽出、モデルごとの system 全文の差し替えは無い。

## 守ること

- **有料の実行は、毎回ユーザーの承認が必要である。** `evorch eval run`（replay、simulator、検証コマンド、judge）は provider の費用がかかる。対象、件数、budget、換算額を示して承認を得てから実行する。一度の承認を、別の実行や拡大した実行に使い回さない。
- 無料なのは、sample の保存（checks の採点）、`evorch eval report --checks-only`、`evorch eval clean` である。
- 採用は人が判断する。設定は自動では変わらない。
- 実 thread や設定、認証情報をリポジトリにコピーしない。

## 全体の流れ

1. モデルを登録する（provider、model ID、価格、context 上限）。
2. 失敗例から sample を作り、checks が元の記録で fail することを無料で確認する。
3. 調整用の sample と、未使用の回帰用 sample を分けて固定する。
4. 共通 overlay で、評価するモデルを両群に固定する。GUI の Context Inspector で、現行設定の初期 context（概算）を確認できる。candidate の値は、report の Initial context が正本である。
5. baseline だけの小さな pilot で、失敗の再発率と費用を測る（承認が必要）。
6. 説明または preset を 1 つずつ変えて、baseline と candidate を比べる（承認が必要）。
7. 品質の条件を満たした candidate だけ、未使用の回帰用 sample で確認する。
8. report を保存し、人が採用を判断する。採用した設定は新規 run から有効にする。

## 1. 新しいモデルを登録する

1. `[providers.<profile>]` に provider と model ID を登録する。設定の書き方は [providers.md](providers.md) を見る。
2. 価格と context 上限が解決できることを確認する。価格はまず models.dev から解決される。解決できなければ、設定のモデルエントリに価格を手で書く（ADR 0036 §8）。価格が不明なモデルの費用は、report に `unknown` と出る（0 円にはならない）。
3. 評価に使う model ID を、routing 上の正確な ID に決める。`+fast` は別の ID である。`model_tool_descriptions` は、ここで決めた ID と完全に一致するときだけ選ばれる（base ID への暗黙の fallback は無い）。

## 2. 失敗例から sample を作る

sample は Evaluator role との相談で作る。CLI に sample を作るコマンドは無い。

1. `eval_list_threads` で、開始状態が記録された thread を探す。再実行できない thread には理由が付く（開始状態が無い、thread goal を使った、など）。
2. `eval_inspect_thread` で、ユーザーに見えた会話、イベントの時系列、エラー、diagnostics を確認する。自己改善の候補だけに頼らず、イベントも見る。**tool のエラーだけで、モデルの誤用と決めない。**
3. 「観測された失敗」と「望ましい挙動」を、人が確定する。
4. `eval_save_sample` で保存する。引数は次のとおり。

   | 引数 | 内容 |
   |---|---|
   | `thread_id`、`problem` | 必須。`problem` は、ユーザーが同意した、観測できる挙動で書く |
   | `focus_roles` | 評価が焦点を当てる role |
   | `kind` | `failure`（既定）または `regression`（うまくいった thread） |
   | `source_refs` | 問題を観測した `run`、`event`、`improvement`。存在と thread への所属を検査する |
   | `tags` | 任意の分類。判定の意味には使われない |
   | `checks` | 次の 3 種の型付き条件。最大 32 件 |

   checks は 3 種だけで、式言語は無い。

   - `successful_calls`: tool の成功呼び出し数。`min`（1 以上）か `max`、または両方を指定する。「正しい tool を使った」（`min`）と「同じ呼び出しを繰り返した」（`max`）を表せる。
   - `tool_errors`: tool のエラー数の上限（`max`）。
   - `diagnostics`: 診断コードの発生数の上限（`max`）。

   どの check も、対象範囲 `scope`（`"tree"`: delegation 木全体、または `{"roles": ["worker"]}`）を持つ。tool の check は、呼び出し範囲 `calls` も持つ（`"direct"`: モデルが直接出した呼び出し、`"including_children"`: `run_js` の中の呼び出しも数える）。

   ```json
   [
     {"type": "tool_errors", "tool": "edit", "max": 0, "scope": {"roles": ["worker"]}, "calls": "direct"},
     {"type": "successful_calls", "tool": "read", "max": 3, "scope": "tree", "calls": "including_children"},
     {"type": "diagnostics", "code": "ToolArgumentsMalformed", "max": 0, "scope": "tree"}
   ]
   ```

   checks は直接呼び出しと子呼び出しを、`calls` ではっきり選ぶ。`run_js` を使うモデルでは、`"direct"` だと子の呼び出しが数えられない。

5. **元の記録で fail することの確認は、保存時に自動で行われる。** provider は呼ばない。
   - `failure` の sample は、元 thread の記録に当てて fail する check が 1 つも無ければ保存されない。
   - `regression` の sample は、完全な記録で全 check が pass でなければ保存されない。
   - 保存の結果に `checks_on_recording` が付く。`eval_inspect_thread` でも同じ採点を見られる。
6. **これは再発の証明ではない。** 元の記録で fail しても、thread 全体を replay したときに失敗が再発するとは限らない。再発しなければ baseline も pass になり、説明の効果は測れない。再発率は、「全体の流れ」の 5 の有料 pilot で測る。
7. checks は任意である。checks を宣言しない sample と、sample を持たない `eval run` は動くが、report に「機械判定なし」と出て、品質は検証コマンドと judge だけで見ることになる。checks で表せない失敗（意味的な誤りなど）は、この形になる。

### 既存の評価出力を無料で再採点する

```sh
evorch eval report --output <評価名> --checks-only
```

記録済みの証跡だけを読み、judge、simulator、検証コマンド、provider を使わない。`<dir>/checks-<sample hash>.json` と `.md` を書き、manifest と `report.json` は書き換えない。sample を固定していない旧い評価は、現在の sample による「事後分析」と書かれる。`--rejudge` とは併用できない。`--checks-only` は `eval report` のフラグで、`eval run` には無い。

## 3. 共通 overlay で、評価するモデルを両群に固定する

baseline も candidate も、評価するモデルで走らせる。そうしないと「モデルの変更」と「説明の変更」が混ざる。日常利用の設定を変えずに済ませるため、`--common` を使う。

```text
baseline  = user 設定 + project 設定 + common
candidate = baseline + candidate
```

- common に書けるのは、`agents`、`routing`、`role_profile`、`role_profiles` だけである。`eval`、`tools`、`comment_checker`、`model_tool_descriptions` は書けない。
- candidate は `eval`、`tools`、`comment_checker` を変えられない。`model_tool_descriptions` と、`agents` の preset などを変えられる。
- project 設定が選べるのは `role_profile` だけである。`role_profile` を使っているなら、`agents` や `routing` ではなく `role_profiles.<名前>` を書き換える。
- simulator、judge、budget は両群で共通である。

common の例（`worker` を、候補が 1 件だけの評価専用の route に固定する）:

```toml
# common.toml
[[routing.routes.eval-target]]
profile = "my-provider"
model = "exact-model-id"

[agents.worker]
models = [{ route = "eval-target", reasoning_effort = "high" }]
```

- route の候補を 1 件にすると、他の profile への fallback が起きない。fallback して別のモデルが答えた replay は、判定保留（pending）になる。
- 評価したい role すべてに、同じように固定する。thread が使う他の role は、両群で同じ（user 設定のまま）になる。
- 比較するのは、1 つの変更だけにする。説明と preset を同時に変えない。

### 比較の事前検査

provider の request を送る前に、次の比較は拒否される（runner と `eval_start`）。

- candidate が何も変えない（両群が同じになる）
- 固定 builtin でない tool の説明（`run_js` の生成説明、team、MCP、host tool、未知の名前）
- ある role のモデル設定と、tool の説明を同時に変える
- どの role にも届かない説明、そのモデルの wire に届かない説明（`apply_patch` を含む）

この検査のために、runner は preflight として各 route の利用可否を確認する。モデル一覧の問い合わせは行うが、replay は始めない。

## 4. モデル別ツール説明（`model_tool_descriptions`）

```toml
[model_tool_descriptions.my-provider."exact-model-id"]
read = "評価する説明文"
grep = "評価する説明文"

[model_tool_descriptions.my-provider."exact-model-id+fast"]
read = "fast の経路にだけ使う説明文"
```

- 最初のキーは `[providers.<profile>]` の profile 名、2 番目は routing 上の正確な model ID（`+fast` は別のキー）。`tools` の下には置かない。
- profile が未登録、または ID が profile のモデルでなければ、設定の読み込みで拒否される。
- 対象は、build が所有する固定の builtin tool（`read`、`edit`、`write`、`grep`、`shell`、`git_diff`、`web_fetch`、`web_search`、`delegate`、runtime の meta 操作など）の、トップレベルの説明文だけである。引数 schema 内の説明、tool 名、順序、権限、公開範囲は変わらない。
- 1 説明は非空で 8 KiB 以下（UTF-8）、1 モデルあたり 64 tool 以下。秘密情報は拒否される。
- `run_js` の生成説明、team tool、MCP tool、外部 host tool は指定できない。`apply_patch` の説明は、評価では確認できないため拒否される。
- `shell` と `edit` の hint、`delegate` の family の追記などは、説明の差し替えの後に付く。override では消せない。

### run 開始時に固定される

- 説明は、run が始まるときに routing が選んだモデルで 1 回選ばれ、その run の間は変わらない。モデル切替や fallback、compaction、restore で選び直されない。
- 子 run は、自分の開始時のモデルで選ぶ。親の説明を引き継がない。
- 保存先は、既存の `run_contexts.config_json` である。設定ファイルを後から編集しても、動いている run と restore される run には効かない。**新規 run から有効になる。**
- したがって、説明の変更は新しい cache 境界を作らない。AGENTS.md の Cache correctness gate は変わらない。
- 開始時の family も固定される。binding を編集してから restore しても、`delegate` の追記は変わらない。

## 5. プロンプトのモデル別の差分

評価で差し替えられるのは、**role binding の appendix preset だけ**である。

- appendix preset は、`<user-config-dir>/presets/<名前>.md` に置き、binding の `preset = "<名前>"` で参照する（`[agents.worker] preset = "..."`、category にも書ける。`role_profile` を使うなら `role_profiles.<名前>.agents...`）。
- baseline 用と candidate 用に、**別の名前の preset** を用意する。既存の preset を書き換えない（baseline にも変更が混ざる）。評価の開始時に preset の内容の hash を固定し、承認の後に変わっていたら実行しない。
- baseline の preset は common に、candidate の preset は candidate に書く。common の `[agents.worker]` は 1 つの table にまとめる（同じ table を 2 回書くと TOML として無効になる）。§3 の例に `preset` を足した完成形は次のとおり。

  ```toml
  # common.toml
  [[routing.routes.eval-target]]
  profile = "my-provider"
  model = "exact-model-id"

  [agents.worker]
  models = [{ route = "eval-target", reasoning_effort = "high" }]
  preset = "worker-appendix-v1"
  ```

  ```toml
  # candidate.toml
  [agents.worker]
  preset = "worker-appendix-v2"
  ```

- `family-*`（`family-claude`、`family-gpt5` など）と、`role-<role>-<family>` は、family 単位で、共有の presets ディレクトリから解決される。overlay では差し替えられない。モデルごとの system 全文の差し替えは対象外である。
- system のプロンプトは run の開始時に決まり、新しい cache 境界は作らない。
- binding の `preset` は、model ではなく role と category に付く。モデルごとに別の appendix を使うには、そのモデルを割り当てた構成（common や role_profile）の中で、その role の preset を指定する。

## 6. 費用を見積もり、budget を絞る

### 見積もり

```text
1 replay の換算上限 = max_tokens × (使うモデルの最高の token 単価) / 1,000,000
全体 = replay 数 × 1 replay の agent 費用 + simulator + judge + 再試行
```

- 既定の budget は、1 replay の delegation 木全体で、5,000,000 token、500 tool 呼び出し、20 user turn、3,600 秒である。最高単価が $15/M なら、1 replay の換算上限は $75 になる。実測の参考値より大きい。
- `eval_start` は、承認の前に次を示す: replay と judge の組の数、agent のモデル、budget、budget を最高単価で換算した額（replay ごと、全体）、simulator と judge の別枠、停止条件。価格が不明なら、換算も不明と示す。
- `eval run` を CLI から直接使うときは、この提示は出ない。同じ項目を自分で計算して示し、承認を得てから実行する。
- **この換算額は、厳密なドルの上限ではない。** budget は request の完了後に判定するため、進行中の request と並行する子 run は超えうる。simulator と judge は含まれない。
- 元 thread の費用は、`eval_list_threads` で見られる（評価時点の価格表で付け直した値）。
- 実測の参考値: `evals/run-js` の r4 は、72 run で $28.407、**1 run あたり約 $0.39** だった。これは固定課題のハーネスの実績で、thread replay の予測ではない。thread 全体、delegation、simulator、judge により増える。同じハーネスでは、暴走した 1 run が 487 request、約 $12.6 を使った。

### 停止策

- 止められるのは、`eval.budget` の token、tool 呼び出し、user turn、経過時間である。超えた replay は未完了として記録され、停止を要求した後は次の replay が始まらない。
- CLI の `eval run` は、interrupt や接続の切断で、起動した job を止める。
- `evals/run-js` の watchdog や `--cost-limit` は、thread replay には無い。ドル建ての強い上限は無い。

### pilot の budget を絞る

`eval` は overlay では変えられない。budget は、ユーザー設定の `[eval.budget]` から読まれる。絞る方法は 2 つある。

- **設定を直接絞る。** `config.toml` に書き、終わったら戻す。

  ```toml
  [eval.budget]
  max_tokens = 100000
  max_tool_calls = 50
  max_user_turns = 5
  max_elapsed_secs = 300
  ```

  最高単価が $15/M なら、1 replay の換算上限は $1.50 になる。長い thread には足りず、未完了になりうる。自動では広げない。未完了の結果を見て、再度承認を得る。
- **`--user-config` で指す設定の複製で絞る。** 日常の設定を触らずに済む。`--user-config <dir>` は、core と、runner が読む場所を切り替える。コードを読んだ限りでは、`<dir>` に次が要る。
  - `config.toml`（と `config.d/`）、`presets/`
  - `evorch-events.db`（thread と sample を読む DB）、`credentials/`
  - `sidebar.json`。runner は、評価する project の trust をここから読む。**ファイルが無い、または読めないと、trust は `approved` として扱われる**ので、元は未承認の project が、複製先では承認済みになる。複製に含め、元と複製の両方で、その project の `trust` が同じであることを確認する（`repo_root` が project の root に当たる項目の `trust`。`approved` 以外は未承認として扱われる）。確認できないときは、この複製の方法を使わず、設定を直接絞る。
  - 出力は `<dir>/eval` に書かれる。

  `<dir>` ごとに別の core が起動する。この複製の運用は、ここでは実際には試していない。DB は一貫した形で複製し、認証情報を含む複製はリポジトリの外に置く。
  `--user-config` は、この `eval run` だけでなく、同じ評価を後から読む `eval report`（`--checks-only` を含む）と `eval clean` にも毎回同じ `<dir>` を付ける。付けないと、元の設定ディレクトリの `eval/` を見に行く。

## 7. 実行する

### pilot（baseline だけ）

baseline だけの実行は、CLI の `eval run` で行う。`eval_start` は candidate が必須である。candidate を渡さないと、baseline だけを走らせる。

```sh
evorch eval run --thread <thread-id> --common common.toml \
  --repeat 1 --no-report --output pilot-1 --problem "<観測した失敗>" \
  [--user-config <dir>]
```

1. まず 1 モデル、1 sample、baseline 1 回、`--no-report` で、費用と所要時間を較正する。
2. 結果は無料で採点できる: `evorch eval report --output pilot-1 --checks-only [--user-config <dir>]`（複製を使った場合は、同じ `<dir>` を付ける）
3. 承認された範囲で、2〜3 sample、baseline 各 3 回程度へ進み、失敗の再発率と揺れを見る。
4. 想定外のモデルが答えた、usage が分からない、進まない反復がある、停止できない、のどれかなら中止する。
5. baseline が pass なら、その sample では改善の効果を測れない。

`--no-report` を付けないと、終了後に report が作られる。report は judge を呼びうるので、pilot では付ける。

### baseline と candidate の比較

説明だけを変える candidate:

```toml
# candidate.toml
[model_tool_descriptions.my-provider."exact-model-id"]
read = "評価する説明文"
```

```sh
evorch eval run --thread <thread-id> --common common.toml --candidate candidate.toml \
  --repeat 2 --output desc-1 --problem "<観測した失敗>" [--verify "<検証コマンド>"]
```

- 通常は、Evaluator の `eval_start` から起動する（承認の提示と digest の照合がある）。`common` と `candidate` は TOML の文字列で渡し、`regression_thread_ids` に回帰用の sample を指定する。
- CLI の主なフラグ: `--thread`（必須）、`--candidate`、`--common`、`--repeat`（既定は `eval.repeat_target`。`eval_start` は回帰 sample に `eval.repeat_regression` を使う）、`--output`（`<user-config-dir>/eval` 配下の名前。空か存在しないこと）、`--problem`、`--verify` と `--verify-timeout`、`--report` / `--no-report`、`--keep-workspace`、`--user-config`。`--approved-digest` は `eval_start` が使うので、手で付けない。
- 承認の後に、sample、overlay、設定、preset が変わると、実行されない。承認をやり直す。
- 説明または preset を 1 つずつ変えて比べる。何が効いたか分からない比較を避ける。
- 説明の差が小さいとき、1 回の勝敗はノイズに埋もれやすい。`--repeat` を増やすか、複数の sample で見る。

## 8. report を読む

`<output>/report.md` と `report.json`。複数の `--output` を渡した `eval report` は、並べた表も出す。

| 節 | 意味 |
|---|---|
| Machine checks | sample の checks の結果。checks が無ければ「機械判定なし」。check の結果と replay の有効性は別 |
| Initial context（estimate） | 各 replay の runtime が組み立てた、root run の初期の system と tools の量。**bytes / 4 の概算**で、実測の token ではない |
| First request（measured） | 最初の request の provider の usage。task の文や履歴も含むので、ツール説明だけの token ではない |
| Cumulative usage（measured） | 全 request の合計。初期 context が毎回送られる。初期 context とは比べられない |
| Quality | status、checks、replay の記録、検証コマンド |
| Effective changes | 変更した説明が、どの run で使われたか |
| Cost by payer | agent、simulator、judge の別と合計。**欠けた値は `unknown`** で、0 円にならない（priced part が別に出る） |
| Validity | 判定保留の理由 |
| Pairwise verdicts | 観点ごとの勝ち、負け、tie、順序の食い違い。両方 tie と、食い違いによる tie は別 |

「初期 context が減った」ことと「累積の費用が減った」ことは別である。初期 context の減少だけで、費用の削減を主張しない。

### 判定保留（pending）の理由

pending の間は、成功にも退行にも数えない。

| 理由 | 意味 |
|---|---|
| `not_applied` | 変更した説明が、replay のどの run でも使われなかった |
| `model_fallback` | 変更の対象 role の request が、別のモデルに行った。または、その role のモデルが 1 つに定まらず、fallback と区別できない |
| `evidence_missing` | replay が、何を使ったかの記録を残さなかった |
| `replay_incomplete` | replay が未完了（budget 超過など） |
| `replay_missing` | 行うはずの replay が、始まらなかった、または終わらなかった |
| `preflight_failed` | preflight が拒否し、何も比べていない |
| `checks_not_conclusive` | checks が判定不能、または記録が不完全な replay で pass になった |
| `judge_order_disagreement` | judge の判定が、提示順で食い違った |
| `judging_failed` | judge が失敗した |

### 「適用済み」の条件

説明の変更は、次の 4 つが成り立つ run だけで「使われた」と数える。

1. run の snapshot が、変更の（profile、route の model ID）を選び、その tool を含む。
2. その tool が、run 自身の記録の `tool_names` にある。
3. その role の request がすべて、その role の run が固定した（profile、base model）に答えられた（usage の台帳で確認）。
4. 説明がそのモデルの wire に届く。

既知の制限（詳細は ADR 0036 の追補）:

- tool calling に対応しないモデルは、送信時に tools が空になる。確認はしない。
- `apply_patch` の説明は、確認できないため数えず、評価では拒否する。
- 3 は role 単位の保守的な照合である。同じ role の run のモデルが一意に決まらなければ、実際に使われていても保留になる。

### 採用の基準

- checks と検証コマンドに、新たな退行が無い。
- 未完了、判定不能、対象外のモデルの replay を、合格に混ぜない。
- judge に明らかな品質悪化が無く、順序の食い違いは保留にする。
- 失敗の改善を主張する sample では、baseline で失敗が再発している。baseline も candidate も pass なら、改善ではなく正常の維持である。
- 品質の悪化を、平均点や費用の削減で相殺しない。

## 9. 採用する

- 採用する説明は、ユーザーの `config.toml` の `model_tool_descriptions` に書く。**`eval_apply_config` は対応しない**（agents と routing だけを保存する）。
- 新規 run から有効になる。動いている run は、開始時に固定した説明のままである。
- model、sample の hash、説明と preset の hash、build、価格、report は、`<user-config-dir>/eval/<評価名>/`（manifest の `pins` と `changes`）に残る。採用の判断と一緒に保存する。
- 後始末: `evorch eval clean`（`--dry-run` で確認できる。複製を使った場合は同じ `--user-config <dir>` を付ける）。workspace だけを消し、証跡は消さない。

## 注意

- 既知の別件: B は Responses 系モデルへの `apply_patch` override を受理するが wire には届かない。preview には override 後の文が出る。
- `evorch eval` の usage 文には、「説明を変える candidate はまだ受理されない」という古い記述が残っている。#856 以降は、説明だけの candidate を受理する。
- `eval run` と `eval report` は core の job として動く。core が無ければ起動される。
- 実 provider での再発率と費用は、この文書の時点では実測していない。
- この評価の結果は、1 thread の勝敗であり、モデルの一般的な順位ではない。
