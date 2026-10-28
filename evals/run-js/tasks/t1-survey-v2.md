# t1-survey-v2 — 調査と要約（tools 全体 + agents + runtime を横断）

| 項目 | 内容 |
|---|---|
| id | `t1-survey-v2` |
| 種類 | (1) 複数ファイルにまたがる調査と要約 |
| 旧課題との違い | v1（`t1-survey`）の 9 ツール × 7 項目に、ロール行列（`crates/agents`）と、`tools` / `runtime` の参照ファイル数（27 回の走査）、ロール別の逆引き、順位付けを足した。答えの項目は 94 |
| 準備 | なし（固定コミットのまま） |
| 検証 | `verify/t1-survey-v2.sh <clone>` |
| 想定ターン数 | 12〜25（`run_js` なしなら 20〜40） |

## プロンプト

```prompt
このリポジトリは Rust のワークスペースです。`crates/tools/src/tools/` 直下で `Tool` トレイトを実装している型（ブラウザ関連の `browser.rs` と `browser/` は対象外。9 個あります）について、ツールの棚卸しを作ってください。

各ツールについて次を調べます。
- name: `Tool::name()` が返すツール名（定数経由で返す場合は、その定数の値）
- execution_mode: `Tool::execution_mode()` の値（`"Shared"` か `"Exclusive"`。メソッドを上書きしていなければトレイトの既定値）
- fs_read / fs_write / process_spawn / network: `Tool::permissions()` が返す `Permissions` の各フラグ
- allowed_roles: `crates/agents` の `Role::capabilities()` で、`allowed_tools` にそのツール名が含まれるロールの名前。`Role` の宣言順に並べる（実行時に足されるツールは考えない。`Role::capabilities()` の戻り値だけを見る）
- refs: 次の 3 つのディレクトリそれぞれについて、git で追跡されている `*.rs`（サブディレクトリを含む）のうち、ツール名をダブルクォートで囲んだ文字列（例: `"read"`）をそのまま含むファイルの数
  - tools_tests: `crates/tools/tests/`
  - runtime_src: `crates/runtime/src/`
  - runtime_tests: `crates/runtime/tests/`

結果を JSON で `answers/t1.json` に書いてください（`answers/` は作成済みです）。形は次のとおりです。

{
  "tools": [
    {"name": "...", "execution_mode": "Shared", "fs_read": true, "fs_write": false, "process_spawn": false, "network": false,
     "allowed_roles": ["Orchestrator", "Worker"],
     "refs": {"tools_tests": 0, "runtime_src": 0, "runtime_tests": 0}}
  ],
  "by_role": {"Orchestrator": ["...", "..."]},
  "ref_ranking": ["...", "..."]
}

- `tools` は name の昇順の 9 要素。
- `by_role` は `Role` の全 9 ロールをキーにして、そのロールが使える上の 9 ツールの name を、昇順で並べた配列にする（`allowed_roles` を逆引きしたもの）。
- `ref_ranking` は 9 ツールの name を、`refs` の 3 つの数の合計が大きい順に並べた配列にする。合計が同じなら name の昇順。

ソースコードは変更しないでください。
```

## 想定解

`verify/expected/t1-survey-v2.json`（`scripts/gen-v2-expected.mjs` で固定コミットから算出。`allowed_roles` は `Role::capabilities()` を実際に呼ぶテストで確認、参照数は `git ls-files | xargs grep -lF` の別経路でも確認済み）。

v1 の 7 項目（name / mode / 権限 4 つ）は `t1-survey.md` の表と同じ。v2 で増えた部分:

| name | allowed_roles | refs（tools_tests / runtime_src / runtime_tests） |
|---|---|---|
| apply_patch | Worker | 5 / 10 / 1 |
| edit | Worker | 8 / 19 / 12 |
| git_diff | Orchestrator, Worker, Reviewer, Planner, Advisor | 3 / 4 / 1 |
| grep | Orchestrator, Explorer, Worker, Reviewer, WebResearcher, Planner, Advisor, Evaluator | 5 / 6 / 8 |
| read | 全 9 ロール | 8 / 35 / 43 |
| shell | Worker | 9 / 23 / 25 |
| web_fetch | Orchestrator, WebResearcher, Planner | 3 / 6 / 5 |
| web_search | WebResearcher | 5 / 4 / 9 |
| write | Worker | 9 / 22 / 23 |

`ref_ranking` は read(86), shell(57), write(54), edit(39), grep(19), web_search(18), apply_patch(16), web_fetch(14), git_diff(8)。

落とし穴:
- v1 と同じ（`apply_patch` の name は別 crate の定数、`git_diff` の `permissions()` は直書き、`Permissions::…()` の展開）。
- `refs` は「ファイル数」であって出現行数ではない。`"read"` は無関係な文脈にも出るが、定義どおり数える（文字列の意味は問わない）。
- `crates/tools/tests/` の外（`crates/tools/src/` 内のテスト）や未追跡ファイルを含めると数が合わない。
- 27 回（9 ツール × 3 ディレクトリ）の数え方が独立しているので、まとめて 1 つの走査にするか、並列にさばく工夫の余地がある。

## 採点基準

- 機械採点（合否）: `tools` 9 要素 × 10 項目 + 要素数と並び順 + `by_role` 9 ロール + ロール数 + `ref_ranking` の計 94 項目がすべて正解と一致。不一致の項目は検証が列挙する。
- 追加観察: 一致項目の割合（部分点）、ソースを変更していないか、`refs` の数え方（`grep -l` / `rg -c` の連鎖か、ヒアドキュメント・スクリプトか、`run_js` か）。
- LLM 採点の観点: 推測で埋めずにソースを読んだか（`execution_mode` を既定値と決めつけていないか、`allowed_roles` を ADR や doc コメントの表から写していないか — doc の表は実際のコードと食い違いうる）。

## 検証コマンド

```sh
evals/run-js/verify/t1-survey-v2.sh <clone>   # 合格 = exit 0
```

## run_js が効きそうな理由

9 ツールのソース 9 ファイル + `tool.rs` + `role.rs` + `tool_contract.rs` の読み取りに加え、27 通りの「ファイルに含まれる文字列を数える」走査が独立している。`Promise.all` で `tools.grep` / `tools.shell` を並べて集計し、`by_role` と `ref_ranking` を JS の中で組み立てて `tools.write` で書ける。`run_js` なしでは grep / read の往復が長く、shell の `for` ループ・パイプの連結や python のヒアドキュメントに流れやすい。
