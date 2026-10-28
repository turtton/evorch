# t1-survey-v3 — 調査と要約（tools + eval + agents + runtime/gui を横断、17 ツール）

| 項目 | 内容 |
|---|---|
| id | `t1-survey-v3` |
| 種類 | (1) 複数ファイルにまたがる調査と要約 |
| 旧課題との違い | v2 は 9 ツール。v3 は `eval` クレートの 7 ツールと `lsp_diagnostics` を加えた 17 ツールにし、トレイトの既定値が効く項目（実行モード）、`impl` ブロックを読まないと分からない項目（`overrides`）、`source`、参照数の走査を 6 ディレクトリ（102 回）に増やした。答えの項目は 251 |
| 準備 | なし（固定コミットのまま） |
| 検証 | `verify/t1-survey-v3.sh <clone>` |
| 想定ターン数 | 9〜15（`run_js` あり。第 3 回の実測 9〜13。`run_js` なしは未測定で、処理量から 15〜30 と見る）。リクエスト数は走査・読み取りを 1 回にまとめられるかで大きく変わるため、**処理量（下）を主な指標にする** |

## プロンプト

```prompt
このリポジトリは Rust のワークスペースです。`Tool` トレイト（`crates/tools/src/tool.rs`）を実装している型の棚卸しを作ってください。

対象は、`crates/tools/src/` と `crates/eval/src/` 以下（サブディレクトリを含む）にある `impl Tool for …` のうち、テスト用でないもの（`#[cfg(test)]` のモジュールの中は除く）です。ただし MCP（`mcp/`）とブラウザ（`browser.rs` と `browser/`）のツールは対象外にします。該当は 17 個あります。

各ツールについて次を調べます。
- name: `Tool::name()` が返すツール名（定数経由で返す場合は、その定数の値）
- source: その `impl Tool for …` があるファイルの、リポジトリルートからの相対パス
- execution_mode: `Tool::execution_mode()` の値（`"Shared"` か `"Exclusive"`）。`impl` が `execution_mode` を上書きしていなければ、トレイトの既定値
- fs_read / fs_write / process_spawn / network: `Tool::permissions()` が返す `Permissions` の各フラグ
- overrides: その `impl Tool for …` ブロックの中で定義されているメソッド名のうち、`name` `schema` `permissions` `execute` を除いたものを、アルファベット順に並べた配列
- allowed_roles: `crates/agents` の `Role::capabilities()` で、`allowed_tools` にそのツール名が含まれるロールの名前を `Role` の宣言順に並べた配列（実行時に足されるツールは考えず、`Role::capabilities()` の戻り値だけを見る。どのロールにも無ければ空配列）
- refs: 次の 6 つのディレクトリそれぞれについて、git で追跡されている `*.rs`（サブディレクトリを含む）のうち、ツール名をダブルクォートで囲んだ文字列（例: `"read"`）をそのまま含むファイルの数
  - tools_tests: `crates/tools/tests/`
  - runtime_src: `crates/runtime/src/`
  - runtime_tests: `crates/runtime/tests/`
  - eval_src: `crates/eval/src/`
  - gui_src: `crates/gui/src/`
  - evorch_tests: `crates/evorch/tests/`

結果を JSON で `answers/t1.json` に書いてください（`answers/` は作成済みです）。形は次のとおりです。

{
  "tools": [
    {"name": "...", "source": "crates/...", "execution_mode": "Shared", "fs_read": true, "fs_write": false, "process_spawn": false, "network": false,
     "overrides": ["description", "execution_mode"],
     "allowed_roles": ["Orchestrator", "Worker"],
     "refs": {"tools_tests": 0, "runtime_src": 0, "runtime_tests": 0, "eval_src": 0, "gui_src": 0, "evorch_tests": 0}}
  ],
  "by_role": {"Orchestrator": ["...", "..."]},
  "ref_ranking": ["...", "..."]
}

- `tools` は name の昇順の 17 要素。
- `by_role` は `Role` の全 9 ロールをキーにして、そのロールが使える上の 17 ツールの name を、昇順で並べた配列にする（`allowed_roles` を逆引きしたもの。1 つも無ければ空配列）。
- `ref_ranking` は 17 ツールの name を、`refs` の 6 つの数の合計が大きい順に並べた配列にする。合計が同じなら name の昇順。

ソースコードは変更しないでください（`answers/` への書き込みは除きます）。確認の質問はしないでください。曖昧な点は妥当な判断で進め、判断したことと置いた仮定を `answers/notes.md` に書いてください。
```

## 想定解

`verify/expected/t1-survey-v3.json`（`scripts/gen-v3-expected.py` で固定コミットから算出。9 ツールの name / mode / 権限は v1 の期待値（ソースを読んで確定済み）と全件一致を確認、`allowed_roles` は `Role::capabilities()` を実際に呼ぶテストで確認）。

| name | source | mode | 権限 | overrides |
|---|---|---|---|---|
| apply_patch | tools/src/tools/apply_patch.rs | Exclusive | r w | builtin_identity, description, execution_mode, set_default_cwd |
| edit / write | tools/src/tools/edit.rs / write.rs | Exclusive | r w | description, execution_mode |
| git_diff | tools/src/tools/git_diff.rs | Shared | r + process | description, execution_mode |
| grep / read | tools/src/tools/grep.rs / read.rs | Shared | r | description, execution_mode |
| shell | tools/src/tools/shell.rs | Exclusive | r w process | 15 個（`cancel_shell_jobs` … `take_shell_job_notifications`） |
| web_fetch / web_search | tools/src/tools/web_fetch/mod.rs / web_search.rs | Shared | network | description, execution_mode（web_search は `execute_with_search_provider` も） |
| lsp_diagnostics | tools/src/lsp/mod.rs | **Exclusive（既定）** | r process | execute_with_context |
| eval_list_threads / eval_inspect_thread / eval_report | eval/src/evaluator.rs | Shared | r | description, execution_mode |
| eval_save_sample / eval_remove_sample | eval/src/evaluator.rs | **Exclusive（既定）** | r w | description |
| eval_start | eval/src/evaluator/start.rs | **Exclusive（既定）** | r w process | description, execute_with_context |
| eval_apply_config | eval/src/evaluator/apply.rs | **Exclusive（既定）** | r w | description, execute_with_context |

`allowed_roles` は v2 の 9 ツールは同じ。eval の 7 ツールは Evaluator だけ、`lsp_diagnostics` はどのロールにも無い（空）。`ref_ranking` の先頭は read(97)、shell(74)、write(65)、edit(41)。

落とし穴:
- **トレイトの既定の実行モードは `Exclusive`**（`tool.rs` の `execution_mode` の既定実装）。「何も上書きしていない＝ Shared」と決めつけると、`lsp_diagnostics` と eval の 5 ツールで間違える。
- `apply_patch` の name は `apply_patch::tool_contract::NAME`（別 crate の定数）。`git_diff` と `lsp_diagnostics` の `permissions()` は `Permissions::…()` ではなくフィールドの直書き。
- `overrides` は `impl` ブロックの中だけを見る。ファイル内の他の `fn`（`impl Shell { … }` の固有メソッドなど）を混ぜると間違える。`shell.rs` は固有メソッドが多い。
- `refs` は「ファイル数」であって出現行数ではない。`crates/eval/tests/` は存在しない（`eval_tests` は項目にない）。
- 17 個のうち 9 個は `tools/src/tools/` にあるが、`web_fetch` は `web_fetch/mod.rs`（サブディレクトリ）、`lsp_diagnostics` は `lsp/mod.rs`、7 個は別 crate の `eval`。`tools/src/tools/` だけを見ると 8 個になる。

## 想定ターン数の根拠（処理量中心）

リクエスト数は「何回にまとめて走査できるか」で決まるので、この課題の処理量を先に書く。

| 処理 | 量 |
|---|---|
| 読むソース | 17 個の `impl Tool for …` を持つ 13 ファイル（`shell.rs` は 526 行）+ `tool.rs`（既定値）+ `role.rs`（ロール行列）+ `tool_contract.rs`（apply_patch の name）≒ 16 ファイル。`impl` ブロックは `grep` で一括に抜けない（終端が分からない） |
| 名前の参照数の走査 | 17 ツール × 6 ディレクトリ = 102 回の独立した「ファイルに文字列を含むか」の走査（対象は 6 ディレクトリの `.rs` 約 770 ファイル） |
| 組み立て | `overrides`（17 ブロックのメソッド名）、`allowed_roles` の逆引き `by_role`、`ref_ranking` の合計と順位 |

素直な解き方（`run_js` なし）では、(1) 対象の洗い出し、(2) `impl` の読み取り（3〜5 回に分けて並べる）、(3) `Role::capabilities()` と `tool.rs` の確認、(4) 参照数の走査（シェルのループか複数回の grep）、(5) JSON の組み立て、(6) 検証と手直し、で 15〜30 リクエストと見る。`run_js` ありでは、読み取り 16 件と走査 102 回が `Promise.all` で 1〜3 回の呼び出しにまとまるので 9〜15 リクエストに収まる（第 3 回の実測は 9〜13 リクエストで、`run_js` の子呼び出しは 26〜772 件）。つまり **この課題は、リクエスト数ではなく子呼び出し数と読んだファイル数・走査回数で `run_js` の効果を見る**。

## 採点基準

- 機械採点（合否）: `tools` 17 要素 × 14 項目 + 要素数と並び順 + `by_role` 9 ロール + ロール数 + `ref_ranking` の計 251 項目がすべて正解と一致。不一致は検証が列挙する。
- 追加観察: 一致項目の割合（部分点）、ソースを変更していないか、`overrides` と `refs` の数え方（シェルのループ・パイプの連結か、ヒアドキュメントのスクリプトか、`run_js` か）、`execution_mode` の既定値を確かめたか。
- LLM 採点の観点: 推測で埋めずにソースを読んだか、`answers/notes.md` に置いた仮定が書かれているか。

## 検証コマンド

```sh
evals/run-js/verify/t1-survey-v3.sh <clone>   # 合格 = exit 0
```

## run_js が効きそうな理由

13 ファイルの `impl` ブロックの抽出と、102 通りの「ファイルに含まれる文字列を数える」走査が独立している。`Promise.all` で `tools.read` / `tools.grep` を並べて、`impl` ブロックの切り出し、`by_role` の逆引き、順位付けまでを JS の中で行い、最終 JSON だけを `tools.write` で書ける。
