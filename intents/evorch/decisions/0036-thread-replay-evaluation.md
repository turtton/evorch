# ADR 0036: thread の再実行と LLM 採点で agent 構成を評価する

Status: Proposed

## Context

agent 構成（role ごとの model chain・prompt preset・生成設定）の良し悪しを、実際の作業で比べたい。
既存の 2 つの仕組みはどちらもこの目的に合わない。

- arena（`crates/arena`）は、spec で定義した合成タスクを設定の組合せで総当たりし、優位な設定を昇格させる。
  本番経路から評価 trace が書かれることはなく、GUI の Arena パネルも開く経路がない（benchmark 導入時に既定タブから外した）。
- `evorch benchmark` は、手で用意した fixture で 1 回通して実行し、leaf delegation を 1 つ録画して、別 model で再生する。
  - fixture を手で作る必要があり、実際の thread を題材にできない。
  - 差し替えられるのは model だけで、prompt preset などは変えられない。
  - 録画時点の作業ツリーの snapshot を前提にしている。
  - docs/benchmark.md 自身が書くとおり、後続の review や手直しへの影響は測れない。
  - 録画・再生のために、runtime の agent loop・compose・model・tool 境界に専用の分岐が散らばっている。
- ADR 0031 で、tool 呼び出しごとの workspace snapshot とロックを廃止した。shell がロックを待ち続けるためである。
  評価のために同等のロックを戻すことはしない。

人が結果を見比べる運用は取らない。評価は、相対比較と過程の分析を LLM に任せる。

評価は日常の作業の中で定期的に回すものではない。
いつもの開発の中で「この thread の agent の動きが微妙だった」と気づき、それを相談するところから始まる。
頻度は高くないので、題材として保持する thread は数件から数十件で足りる。
一方で、題材にしたい thread は相談の時点ですでに終わっている。開始時点の状態は後から取れないため、事前に記録しておく必要がある。

## Decision

過去の thread を題材にし、開始時点の状態から thread 全体を再実行する。
baseline と candidate の 2 つの構成で同じ条件のもとで走らせ、LLM が一対比較と過程の分析を行う。
評価は evorch の中で、Evaluator role との相談から始める。
arena と `evorch benchmark` はこの仕組みで置き換え、削除する。

### 1. 評価は Evaluator role との相談から始める

- 新しい role として Evaluator を追加する。
  - Orchestrator は、agent の振る舞いや構成についての相談を Evaluator に委譲する。
  - Evaluator は ADR 0002 の方針に従い、評価用の tool と読み取りだけを持つ。
    write / edit / apply_patch / shell / delegate / web 系 tool は持たせない。
  - ユーザーとのやり取りは、全 role 共通の user question 操作で行う。
  - 設定は `[agents.roles.evaluator]` で他の role と同じように binding する。
- Evaluator の tool は次のとおり。
  - **thread の一覧**: 開始状態を記録した thread を、最初の依頼・project・日時・費用・再実行の可否とともに返す。
  - **thread の調査**: ユーザーに見えた会話、イベントを要約した時系列（上限つき）、§8 の指標、delegation の木、拒否された承認、エラーを返す。
    - ホスト上の DB を shell で読む代わりの、型付きの読み取り専用 tool とする。
      shell で読む方法は sandbox escalation を要求するため、現状では承認要求の原因になっている（§4）。
  - **サンプルの保存**: thread を評価のサンプルとして固定する。
    固定したサンプルには、問題の記述（例:「reviewer が明らかな見落としを通した」）と、焦点を当てる role を付ける。
  - **評価の起動**: 対象のサンプル、candidate の overlay、回帰確認に使うサンプル、繰り返し回数を受け取る。
    - 起動のたびに費用の見込みを示し、ユーザーの承認を得る。
      見込みは、元 thread の費用（§9 の価格表で付け直した値）に実行回数を掛けて出す。
    - 評価は `evorch eval run` を別プロセスとして起動し、裏で走らせる。完了は通知で Evaluator に届く。
  - **report の参照**: 採点結果、費用、過程の分析を返す。
  - **構成の反映**: 採用すると決めた overlay を、設定の差分として示す。
    ユーザーの承認を得てから、既存の設定保存処理で設定ファイルに書き込む。
- 相談の典型的な流れは次のとおり。
  1. ユーザーが、気になった thread の振る舞いを相談する。
  2. Evaluator が thread を特定して調査し、原因の仮説と、差し替える role の候補を示す。
  3. thread をサンプルとして固定し、candidate の overlay を作る。
  4. 費用の見込みを示して承認を得たら、評価を起動する。
  5. report を要約し、採用するかを相談する。採用するなら構成を反映する。

### 2. thread 開始時点の状態を記録する

- thread の最初の conversation root run を開始する直前に、project root で git を 1 回だけ読み取り、次の値を storage に保存する。
  - `HEAD` の commit
  - `git diff --binary HEAD`（未コミット差分）
  - 未追跡ファイルの一覧と内容（`.gitignore` 対象は除く）
- 記録は run の開始より前に終わらせる。agent が作業ツリーを変更する前の状態を取るためである。
  - git の読み取りには時間の上限を設ける。上限を超えたら「再実行不可」として記録し、run はそのまま開始する。
- tool 呼び出しごとの記録やロックは行わない。読み取り専用の git 呼び出しなので、ADR 0031 の待ちは発生しない。
- 次の場合は内容を保存せず、理由とともに「再実行不可」と記録する。切り詰めた状態から再実行することはしない。
  - 差分と未追跡ファイルの合計が 8 MiB を超える。
  - 差分や未追跡ファイルに、SecretGuard が検出する秘密情報が含まれる。
  - project が git 管理外である。
  - 記録に失敗した。
- 記録は 14 日で削除する。サンプルとして固定した thread の記録は、期限に関係なく残す。
- 対象は、新しく始めた thread の最初の run とする。次の thread は初版では対象外とする。
  - fork や巻き戻しで、別の thread の履歴から始めた thread
  - Orchestrator への escalation で作られた thread
- この記録より前に作られた thread は再実行の対象外とする。

### 3. 隔離した場所で再実行する

- 実行ごとに runner が所有するディレクトリへ `git clone --local --no-checkout` し、記録した commit を checkout して差分と未追跡ファイルを復元する。
  - `--shared` は使わない。clone の object がユーザーの `.git` に依存しないようにし、ユーザー側の gc で clone が壊れないようにする。
  - 元の remote は外し、`origin` は §4 のローカル bare repository に置き換える。
    bare repository は clone の `.git/evorch-origin.git` に置き、記録時の branch をそこに作って checkout した branch の upstream にする。
    `HEAD` が detached だった thread は detached のまま checkout する。
  - ユーザーの checkout と `.git` には書き込まない。
  - project の絶対パスに依存する thread があるため、手順のなかで path の違いを明示する。
    ただし同じ絶対パスでの再現は保証しない。
- 再実行は専用の storage DB とイベントバスを使う。ユーザーの DB と `sidebar.json` は読み取り専用の題材として扱う。
- ADR 0032 の許可ディレクトリは mount しない。それに依存する thread では、その旨を report に残す。

### 4. 副作用を遮断し、作業に必要な環境は再実行側で用意する

承認の自動拒否だけに頼ると、通常の作業も止まる。
2026-10-19 時点の手元の DB では、承認要求 51 件はすべて shell の sandbox escalation で、内訳は次のとおりだった。

- GitHub への通信（`git pull` / `git push` / `gh` / GitHub API）
- ホスト上の evorch DB や設定の読み取り（run の調査）
- テストが `/var/tmp/evorch-output-<uid>` に書き込めないための、sandbox 外での再実行
- 署名付き commit

そこで、escalation で許可するのではなく、再実行の環境の側で必要なものを用意する。

- shell は bwrap のネットワーク名前空間を分けて実行し、sandbox escalation は `Off` にする。
- clone の `origin` は、runner が所有するローカルの bare repository に向ける。
  - `git pull` / `git push` は、ネットワークなしで sandbox 内で成功する。
  - push された ref は実行結果として記録する。
- clone の git 設定で、commit 署名を無効にする。
- tool 出力の保存先（`EVORCH_OUTPUT_DIR`）を、実行ごとの scratch に向ける。
  sandbox が scratch を既定の保存先として渡すよう、通常の利用でも修正済みである（#275）。
- Cargo は `CARGO_NET_OFFLINE=true` で動かす。registry のキャッシュは既存どおり読み取り専用で mount する。
  新しい依存を取得する必要がある作業は失敗し、その旨を report に残す。
- 必要なら、両方の実行に同じビルドキャッシュ（target ディレクトリの複製）を事前に与える。片方だけを有利にしない。
- web 系 tool は `web_search` / `web_fetch` に限って許可する（URL ごとの既存の審査は維持する）。
  ページを操作できる agent browser tool は無効にする。
- 以上で用意できない承認要求（`gh`、GitHub API、ホスト上のファイルの読み取りなど）は自動で拒否する。
  - agent には、評価環境なので外部にはアクセスできないと返す。
  - 拒否した件数と内容は、指標として report に残す。
- 元 thread で承認された escalation から、外部の状態に依存する thread を事前に判定する。
  - 対象は、GitHub 以外への通信やホスト上のパスの読み取り。
  - 該当する thread は report で「外部依存」と明示する。
  - 再実行の結果でも、拒否した承認、失敗した web 系 tool、ネットワークや DNS の失敗に見える shell のエラーがあった実行は、report で外部依存の疑いとして示す。
- provider への通信と web 系 tool だけが外部に出る通信になる。

### 5. ユーザー役を LLM が担う

- 元 thread のユーザー発言・`ask_user` への回答を、`review_evidence` の時系列で再構成し、ユーザー役の LLM に参照させる。
- root run が `Waiting` になったとき、または `ask_user` が保留になったときに、ユーザー役が次のどれかを返す。
  - 回答する
  - 追加の指示を送る
  - 終了する
- ユーザー役が見るのは、ユーザーに見える会話だけにする。tool の内部ログは渡さない。
- ユーザー役は元のユーザーの意図と制約を伝えるだけにし、元のユーザーが与えていない解決策は与えない。
  元の要求が満たされた、または満たせないと判断した時点で終了する。
- thread goal（ADR 0028）を使った thread は、初版では対象外とする。

### 6. 比べるのは baseline と candidate の 2 つの再実行

- 元 thread の当時の構成（解決済みの chain や prompt）は storage に残っていない。
  そのため元 thread の結果は baseline にせず、ユーザー役と採点の参考情報にとどめる。
- baseline は現在の構成、candidate はそれに overlay を重ねた構成とする。
  overlay は `LoadOptions.cli_overrides` で最後に深マージし、role binding（`models` / `preset` / `generation`）と `role_profile` を差し替える。
  role ごとの tool 集合は ADR 0002 で固定されているので、差し替えの対象にしない。
- 各構成で、ターン数・token・tool 呼び出し・経過時間の予算を設ける。予算を超えた実行は「未完了」として採点に回す。
- 題材が少ないため、1 回きりの結果は揺らぎに左右されやすい。繰り返し回数は指定でき、既定は次のとおりとする。
  - 相談の対象のサンプルは、各構成 2 回。
  - 回帰確認に使うサンプルは、各構成 1 回。

### 7. LLM が採点する

各実行の終了後に、次を集める。

- 開始状態から最終状態までの diff
- ユーザーに見えた会話
- イベントから集計した指標（費用、token の内訳、cache の命中率、tool 呼び出しの種類別件数、エラーとリトライ、delegation の木、拒否された承認、経過時間）
- 指定があれば検証コマンドの結果（例: `cargo test --offline`）。検証コマンドは、同じネットワーク遮断のもとで各実行の最終状態に対して走らせる
  - 再実行の tool と同じ sandbox で動かす。600 秒（`--verify-timeout`）で打ち切り、時間切れは失敗とする。
  - 終了コードと、stdout / stderr の末尾（上限つき）を実行結果に残す。

ユーザー役と採点の request は、実行の指標と費用から除き、評価の overhead として別に示す。

採点は 2 種類行う。

- **一対比較**
  - 繰り返しの i 回目どうしで、baseline と candidate を組にする。
  - 元のユーザー要求に照らして、要求の充足・正しさ・範囲の守り方・効率の観点ごとに、A / B / 引き分けと理由を構造化して返させる。
  - サンプルに付けた問題の記述を渡し、その問題が起きたか・直ったかを独立した観点として判定させる。問題の記述がなければ、この観点は設けない。
  - 提示順を入れ替えて 2 回採点し、判定が食い違った観点は引き分けにする。
  - 未完了や失敗の実行も、その状態を伝えたうえで採点する。
  - 採点結果は出力ディレクトリの `judgments/` に組と提示順ごとに保存する。`eval report` を再実行しても採点し直さず、`--rejudge` を付けたときだけ採点し直す。
- **過程の分析**
  - 実行ごとに、イベントを要約した時系列（生ログではなく上限つき）を渡す。
  - 時間と token がどこに使われたか、遠回りや重複した tool 呼び出し、delegation の妥当性、差し替えた role の振る舞いを分析させる。
  - 2 つの実行の差を、差し替えた role とサンプルの問題の記述に焦点を当ててまとめさせる。

### 8. 費用は token 量ではなく金額で比べる

記録時点の `usage_requests.cost_usd` は使わない。
手元の DB では、正常に終わった request 22,997 件のうち 69.7% が `NULL` になっている。
subscription の provider は価格が付かないか、plan の 0 円になる。
そこで評価時に、次の手順で価格を付け直す。

- request ごとの token を、入力（cache なし）・cache 読み取り・cache 書き込み・出力に分けて保存する。
  そこに、評価時点の 1 つの価格表を適用する。
  - 価格表は models.dev と、設定ファイルの価格の上書き（`input_price` など）から作る。
  - 取得時刻を report に残す。
  - baseline と candidate には同じ価格表を使う。
- 価格の解決は、GUI と headless の runner が共用する `crates/recorder`（`crates/recorder/src/pricing.rs`）にある。
- fast（service tier `priority`）の料金と、入力量による段階料金は、request ごとに適用する（#276 で対応済み）。
  - usage の記録に service tier を残し、fast の request には models.dev の `experimental.modes.fast.cost` を使う。
  - models.dev の `cost.tiers` から、request ごとの入力量で段階を選ぶ。
- models.dev で解決できない model は、設定ファイルの model エントリで手動指定する。
  - 既存の `input_price` / `output_price` / `cache_read_price` / `cache_write_price` / `context_window` と `metadata_source = "manual"` を使う。
    別の models.dev エントリを指す `metadata_ref` も使える。
  - fast の料金は `fast_price`、段階料金は `price_tiers` で書く（#276 で追加済み）。
- subscription の provider（Codex・Claude・Kimi のサブスク）では、対応する API model の公開価格で「API 換算の費用」を出し、そう明記する。
  対応付けは models.dev の provider 候補と設定で決め、plan の 0 円は使わない。
  - Claude のサブスクは models.dev の `anthropic`、Codex は `openai`、Kimi は `moonshotai` の同じ model を使う。
    見つからなければ、plan のエントリの `canonical_model_id` が指す model を使う。
  - 既知の価格がすべて 0 になる場合は、価格を付けられない request として扱う。
- 価格を付けられない request が 1 件でもあれば、その比較は「費用不完全」とし、該当する model を列挙する。0 円とはみなさない。
- 2 つの実行が同じ model の同じ prompt 先頭を共有すると、provider の prompt cache が後の実行だけを安くする。
  これを避けるため、各実行の system prompt の先頭に実行ごとの識別子を入れ、実行をまたいだ cache の共有を断つ。
  実行内の cache はそのまま効かせる。
- 一対比較の効率の観点には、この金額を渡す。

### 9. 採点 model の設定

採点用とユーザー役用の model は評価設定で指定する。
採点 model が比べる候補と同じ model family なら、自己優遇のおそれとして report に警告を出す。

- `eval.judge` が未設定なら Orchestrator の binding を使い、どちらを使ったかを report に残す。
  採点 model は、candidate ではなくユーザーの設定のもとで、Orchestrator の枠に binding を入れて呼ぶ。
- 比べるのは、採点 model と、差し替えた role が baseline・candidate のどちらかで使う model である。
  model family は models.dev の `family`（なければ provider の接頭辞を除いた model 名）の先頭の語（`claude`・`gpt` など）で比べる。
回帰確認のサンプルを含めて複数 thread を走らせた場合は、thread ごとの勝ち・負け・引き分けの件数を並べる。
費用も thread ごとに並べる。勝敗を総合順位には変換しない。

### 10. 入口

- 普段の入口は Evaluator との相談（§1）とする。
- その下で動く実行と report は CLI とする。Evaluator の tool もこれを別プロセスとして起動する。
  - `evorch eval run --thread <id> --candidate <overlay.toml> --output <dir>`
    - `--problem <text>` で問題の記述、`--verify <command>` で検証コマンドを渡す。
    - 終わると、同じディレクトリの report を作る（`--no-report` で省略）。
  - `evorch eval report --output <dir>`
    - 各ディレクトリに `report.json` と `report.md` を書く。
    - `--output` を複数渡すと、thread ごとの観点別の勝ち・負け・引き分けと費用の表も出す。
- 出力ディレクトリには、各実行の storage DB、diff、会話、指標、採点結果を残す。
  `evorch inspect` を出力側の DB に向けて使えるようにする。
- GUI では、評価の進行と report を表示する。表示の詳細は後続で決める。

### 11. 置き換えと削除

1. arena を削除する（#272 で完了）。
   - `crates/arena`、GUI の Arena パネルとそのテスト、`storage::eval` / `eval_variant`、`docs/arena-acceptance.md` が対象。
   - `eval_trace` の読み書き API は消す。memory ledger は trigger で追記専用なので、既存の行と部分 index は読まれない履歴として残す。
   - 古いレイアウトを読めるように `PanelKind::Arena` の deserialize と、読み込み時のタブ除去は残す。
2. thread 開始時点の状態を記録する（§2）。
3. 隔離した再実行とユーザー役を、CLI の `evorch eval run` として実装する（§3–§6）。
4. 採点・費用の算出・report を実装する（§7–§10）。
5. Evaluator role とその tool、評価の完了通知を実装する（§1）。
6. `evorch benchmark` を削除する。
   - 対象は CLI、runtime の benchmark 専用の分岐（recorder・replay・model の凍結・tool 出力の境界）、`SnapshotStore`、`docs/benchmark.md`、`examples/benchmark`。
   - 再実行の仕組みが動いてから消す。

## Compatibility

- storage マイグレーションを追加する。
  - thread 開始状態の保存（§2）
  - サンプルとその問題の記述（§1）
- `eval_trace` は API だけを消し、マイグレーションは追加しない（§11 の 1）。
  ADR 0035 が arena の評価帰属に残した `oracle` alias は、読み手がなくなるので API と一緒に消える。
- 設定には次を追加する。既存の節は変更しない。
  - 評価用の節（ユーザー役・採点の model binding と既定の予算）
  - `[agents.roles.evaluator]`
- ADR 0002 の role 一覧に Evaluator を加える。他の role の tool 集合は変えない。

## Validation and limits

mock-openai と scripted model を使って、次を決定的にテストする。

- 開始状態の記録と、上限超過・秘密情報・git 管理外での「再実行不可」、14 日での削除とサンプルの保持
- clone と復元の一致
- ユーザー役との往復（回答・追加指示・終了）
- ネットワーク遮断、ローカル bare repository への pull / push、承認の自動拒否
- 予算超過
- 提示順を入れ替えた採点の集約
- 価格表による費用の付け直しと「費用不完全」の判定、実行をまたいだ cache 共有の遮断
- report の生成
- Evaluator の tool（調査・サンプルの保存・起動前の承認・完了通知・構成の反映前の承認）

実際の provider での評価は、利用者の手元で行う。

この仕組みは元 thread の厳密な再現ではない。

- model の揺らぎ、provider の cache、ユーザー役の応答の違いで、同じ構成でも結果は変わる。
- ネットワークに依存する作業、許可ディレクトリに依存する作業、goal を使う作業は、初版では正しく評価できない。
- LLM の採点には位置の偏りと自己優遇が残る。順序の入れ替えと警告で弱めるが、なくすことはできない。
- 1 thread の勝敗を一般的な model の順位として扱わない。構成を採用する前に、回帰確認のサンプルでも比べる。
- 実行ごとに provider の費用が 2 実行分以上かかる。繰り返しを含めると、相談 1 件で元 thread の数倍になる。
