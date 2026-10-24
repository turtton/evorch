# ADR 0040: Advisor を行き詰まり時の相談役として起用条件を定める

## Status

Accepted（2026-10-23）。利用者との相談で、ADR 0035 の Advisor を残し、起用条件を 3 つの場面に具体化することに合意した。ADR 0035 の役割境界（read-only、delegate 不可、出力契約、Reviewer との別 context）は変えない。

2026-10-23 追記: 当初は起用条件を Orchestrator のプリセット・key triggers・delegate の説明の 3 か所に書いたが、どれも同じ Orchestrator のプロンプトに入るため重複していた。「役割の記述と責務」の節のように、情報ごとに 1 か所へ整理した。

## Context

ADR 0035 で Oracle を相談専用の Advisor に改称したが、改称後に Advisor が起用された run は 1 件もない。

### 計測（2026-10-23、GUI の store から）

GUI の store（`run_contexts` / `usage_requests` / `user_questions`）を読み取り専用で集計した。対象は 2026-09-14〜10-23 の run である。分類はキーワードによる近似を含む。

- Advisor の run は 0 件。旧名の Oracle は 10 件（10/03 と 10/09 のみ）。うち 7 件は provider の失敗で Error になり、完了した 3 件のうち 2 件は「独立レビュー」「最終受入レビュー」の依頼で、ADR 0035 で Reviewer に移した仕事だった。設計案の比較を頼んだのは 1 件だけだった。
- 10/19 以降の子 run は Worker 280、Reviewer 63、Explorer 11、Planner 3、WebResearcher 1、Advisor 0。
- Advisor を委譲できるのは Orchestrator だけで（ADR 0002）、Worker が困ったときの経路は親への質問か escalate である。親は質問に自分で答えるか、ユーザーに聞くか、Planner に引き継ぐ。Orchestrator のプリセットに Advisor の起用条件はなく、用途の案内は「必要なときのみ」だけだった。

ADR 0035 の Advisor の担当が、実際には誰に担われていたか:

- 設計案の比較: Planner（未決事項に推奨案を付ける）と deep カテゴリの Worker（候補を比較して選択理由を示す）、最終判断はユーザー。Orchestrator がユーザーに出した質問 159 件のうち約 30 件は、選択肢と推奨案を付けた設計判断だった。
- 原因仮説の比較: high-reasoning や deep の Worker に「調査のみ」として委譲されていた（10/19 以降 high-reasoning の Worker は 56 件）。仮説を確かめるには shell・ログ・CI の参照が要り、read / grep / git_diff だけの Advisor にはできない。Explorer は推測を報告しない。
- 計画への独立した意見: Reviewer の plan-review。

どの役割も担っていない場面:

- レビューと修正が何周も続く場面。「再レビュー」「2巡目」「3回目のレビュー」を含む修正依頼は 18 件（9 つの Orchestrator）あり、3 周したものが 3 件あった。親は修正を再委譲するだけで、設計や前提がずれているのか、指摘のほうが誤っているのかを見直す役がいない。
- 子の質問のうち、仕様では決まらない技術判断（例: 再起動後のハンドル再利用、cursor の基準、fallback 条件の矛盾）。子からの質問 305 件の多くは権限（約 1/3）、ツールや環境の制約（約 1 割）、仕様や意図の確認で、技術判断は十数件だった。こうした質問には Orchestrator が独立した検討なしに自分で答えることがある。
- 調査で集めた証拠から原因を絞りきれない場面。証拠を集める Worker はいるが、仮説を見分ける次の確認を設計する役がいない。

## Decision

Advisor を残し、次の 3 つの場面で起用する相談役と定める。どれも Orchestrator の判断で、固定工程ではない。

1. 同じ must 指摘が修正後の再レビューでも残る、または Worker と Reviewer の見解が食い違う。次の修正を再委譲する前に、指摘と修正の経緯を渡して、設計や前提のずれか指摘の誤りかを比べさせる。
2. 子の質問が仕様では決まらない技術判断で、Orchestrator がユーザーに聞かずに決める根拠が足りない。
3. 集めた証拠で原因の仮説を絞りきれない。Advisor は証拠と各仮説の整合を比べ、仮説を見分ける次の確認（コマンド・見るログや値と、その結果ごとの解釈）を「判断を変える追加情報」として返す。確認の実行は Worker に任せる。

権限やユーザーの意図の確認には Advisor を使わず、ユーザーに聞く。Advisor が判断の要をユーザーの意図と見たときは、親がユーザーに確認すべき質問として返す。

Advisor の権限と出力契約の項目は変えない。

### 役割の記述と責務

Orchestrator には、プリセット・key triggers（Intent Gate 内）・delegate の説明がすべて同じプロンプトで届く。同じ情報を複数の箇所に書かず、次のように分ける。

- key triggers（ロールの用途・起用条件）: 3 つの場面、権限や意図の確認には使わないこと、他ロールとの選び分け。
- delegate の advisor target の説明（委譲先の仕様）: 渡すもの、読み取り専用で実行しないこと、返す内容、最終 Reviewer との別 context。
- Orchestrator のプリセット: Advisor の起用条件は書かない。
- Advisor のプリセット: 受け手が違うので、自分が呼ばれる場面と応答の契約を持つ。

## Alternatives

- Advisor を廃止し、Planner / Reviewer / Worker に吸収する。役割は減るが、上の隙間は埋まらず、ADR 0035 の互換処理をまた変えることになる。
- Worker に Advisor 相談用のツールを持たせる。困った本人が直接相談できるが、委譲を Orchestrator に限る ADR 0002 の権限設計を変える。
- 今のまま残す。起用されない状態が続く。

## Validation and limits

自動テストで次を確かめる。合成した Orchestrator のプロンプトで、各場面が 1 回だけ現れる。Advisor のプロンプトに場面と「検証は実行しない」が入っている。delegate の説明は仕様だけを持ち、起用条件を繰り返さない。

これはプロンプト契約の変更で、モデルが条件どおり Advisor を起用することや、判断やループの収束が良くなることは保証しない。採用後は store で Advisor の起用回数、起用した場面、相談後に再レビューのループが収束したかを追い、効果がなければ見直す。
