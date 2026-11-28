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
    - 実装（§11 の 5）では次のようにした。
      - CLI は GUI の実行ファイルと同じディレクトリの `evorch` を使い、なければ PATH から探す。どちらにもなければ、起動前に失敗として返す。
      - thread ごとに 1 プロセスを起動し、まとめて 1 つの group とする。group の記録、candidate の overlay、各プロセスの stdout / stderr は `<user config dir>/eval/.groups/<id>/` に置く。各評価の出力は `<user config dir>/eval/<日時>-<thread>/` に置く。
      - プロセスは Evaluator の run が終わっても走り続け、GUI のプロセスが終わると止まる（Linux では親の終了時に SIGTERM を受ける）。GUI は終了を待ってプロセスを回収する。終わらなかった評価は、次の GUI からは中断として見える。
      - 全プロセスが終わると、group ごとに 1 回だけ通知する。内容は thread ごとの結果で、report の要約か、終了状態と stderr の末尾である。
        通知先は、起動した run から親へ遡り、次に同じ会話の新しい run の中で、最初に見つかった生きている対話 run とする。通知はユーザー権限のない内部メッセージとして届き、待機中の run を起こす。どの run にも届かなくても、GUI の診断として表示する。
  - **report の参照**: 採点結果、費用、過程の分析を返す。
  - **構成の反映**: 採用すると決めた overlay を、設定の差分として示す。
    ユーザーの承認を得てから、既存の設定保存処理で設定ファイルに書き込む。
    - 書き込めるのは、設定保存処理が扱う `agents` / `routing` と、既存の `role_profiles.<name>` の同じ節に限る。書き込んだあと、GUI は設定保存の後と同じように model を組み直す。
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

- 実行ごとに runner が所有するディレクトリで `git init` し、元の repository から記録した commit だけを `git fetch --no-tags` で取得して checkout し、差分と未追跡ファイルを復元する。
  - `git clone` は使わない。clone は記録より後の branch・tag・object を持ち込み、再実行 agent が後続の実装に気づいてしまう。
    clone には記録した commit とその祖先の object だけが入り、ユーザーの repository の ref・tag・到達不能 object・`FETCH_HEAD` は入らない。
  - `--shared` / alternates / hardlink は使わない。clone の object がユーザーの `.git` に依存しないようにし、ユーザー側の gc で clone が壊れないようにする。
  - 元の remote は持たず、`origin` は §4 のローカル bare repository にする。
    bare repository は clone の `.git/evorch-origin.git` に置き、clone から記録時の branch だけを push して作り、checkout した branch の upstream にする。
    `HEAD` が detached だった thread は detached のまま checkout し、`origin` には何も置かない。
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
  - 初版では新しいパネルを作らない。起動と反映の承認は既存の承認カード、完了は会話への通知と診断で示す。進行と report は Evaluator が `eval_report` で読む。

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

## Addendum（2026-11-28、実失敗に基づくツール説明・プロンプトの評価）

### Context

実際の失敗から作った sample で、モデルごとのツール説明とプロンプトの精度と context 量を比べたい。
そのために、本 ADR の評価を次のとおり拡張した（#829 sample の checks、#836 モデル別ツール説明、#844 共通 overlay と changes、#856 judge と report）。
別の評価基盤、ケース DB、scheduler は作らない。1 thread = 1 sample のまま、既存の thread 再実行を使う。
手順は [docs/eval-tool-optimization.md](../../../docs/eval-tool-optimization.md) に置く。

以下は §6（overlay の対象）、§7（採点）、§8（費用）、§1 の sample の保存を補う。矛盾する記述はこの追補を優先する。

### 1. sample の checks と出典

- `EvalSample` に次を加える（`eval_samples.spec` JSON 列、storage schema v78。旧形式の互換層は作らない）。
  - `kind`: `failure`（既定。実際の失敗）と `regression`（うまくいった thread。変更で壊れないことの確認）。
  - `source_refs`: 問題を観測した run、event、改善候補。存在と thread への所属を保存時に検査する。証拠の参照であり、replay の run と一致させる条件ではない。
  - `tags`（判定の意味には使わない）、`checks`、`check_version`（採点意味論の版）。
- `checks` は型付きの 3 種だけで、式言語は持たない。
  - tool の成功呼び出し数（下限、上限のどちらか、または両方）
  - tool のエラー数の上限
  - 診断コードの発生数の上限
- 各 check は、対象範囲（delegation 木全体、または role 集合）と、呼び出し範囲（直接の呼び出しのみ、または `run_js` 内の子呼び出しを含む）を持つ。role/category の新しい分類体系は導入しない。
- 上限は checks 32 件、追加メタデータ 64 KiB。超過は切り詰めずに拒否する。追加した文字列にも既存の秘密情報検査をかける。
- checks は任意である。宣言しない sample と、sample を持たない `eval run` は従来どおり動き、report に「機械判定なし」と示す。
- 保存時の検査（`eval_save_sample`）。provider を呼ばず、元 thread の記録に checks を当てる。
  - checks を宣言した `failure` sample は、fail する check が 1 つも無ければ受理しない。
  - `regression` sample は、完全な記録で全 check が pass であることを求める。
  - 元の記録で fail することは、replay で失敗が再発する証明ではない。再発率は有料の baseline 反復でしか測れない。
- 採点は共通のイベント採点関数（`eval/src/checks.rs`）で行い、`pass` / `fail` / `indeterminate` と理由・根拠参照を返す。`RunMetrics` の集計は正本にしない。
  - 成功は終端イベントで数え、`parent_call_id` で親子を二重に数えない。
  - 記録の欠落、読み取り上限による切り捨て、対象 role の未出現、未終端の call を検出する。未観測を根拠にした pass は出さない。観測済みの上限超過は fail にできる。
  - check の結果と replay の有効性は別に示す。未完了の replay は、全 check が pass でも品質合格にしない。
- `evorch eval report --checks-only` は、記録済みの証跡だけを読んで再採点する。judge、simulator、検証コマンド、provider を使わず、`checks-<sample hash>.{json,md}` を別に書く。manifest と `report.json` は書き換えない。sample を固定していない旧い評価は、現在の sample による「事後分析」と明記する。
- runner は元 DB の sample を読み、開始時に manifest の `sample`（hash 付き）へ固定する。checks 本体は CLI 引数で渡さない。後から sample を書き換えても、過去の manifest は変わらない。

### 2. overlay の合成と対象

§6 の overlay は role binding と `role_profile` を差し替えるものだった。これを次のとおり広げる。

- 構成は 4 層で合成する。

  ```text
  baseline  = user 設定 + project 設定 + common
  candidate = baseline + candidate
  ```

  - project 層が設定できるのは `role_profile` の選択だけである。replay の構成は、再現した project 設定で作る。
  - **common**（`--common`、`eval_start` の `common`）は、baseline と candidate の両方に掛かる。対象は `agents`、`routing`、`role_profile`、`role_profiles` に限る。評価するモデルを両群で固定するために使い、比較の対象にはしない。日常利用の設定は変えない。
  - **candidate** は `eval`、`tools`、`comment_checker` を変更できない。common では `model_tool_descriptions` を設定できない。
  - simulator、judge、budget は両群で共通で、common と candidate では変えられない。
- candidate が変えられるものに `model_tool_descriptions` を加える。
  - キーは provider profile と、routing 上の正確な model ID（`+fast` は別の ID）。`tools` の下には置かない。
  - 対象は、build が所有する固定の builtin tool のトップレベルの説明だけである。tool 名、引数 schema、順序、権限、公開範囲、実装は変えない。
  - `run_js` の生成説明、動的な team tool、MCP tool、外部 host tool への指定は拒否する。
  - 1 説明は非空で 8 KiB 以下（UTF-8）、1 モデル 64 tool 以下、run が保存する snapshot は 128 KiB 以下。超過と秘密情報は拒否する。
- プロンプトの比較には、role binding の appendix preset だけを使う。baseline 用と candidate 用に別名の preset を用意し、開始時に内容の hash を固定する。`family-*` は family 単位で共有の presets ディレクトリから解決されるため、overlay では差し替えられない。モデルごとの system 全文の差し替えは対象外とする。
- manifest に `changes` を持つ。`swapped_roles` は binding の差分だけを表し、説明だけの candidate を架空のモデル交換として詰め込まない。
  - `changes` は、変わった binding、ツール説明（profile、route の model ID、tool 名、baseline と candidate の hash）、差分が及び得る role（`affected_roles`）を持つ。
  - `eval_start` は、実効的な changes が空なら拒否する。
- provider を呼ぶ前に、次の比較を拒否する。
  - 何も変えない candidate
  - 固定 builtin でない tool の説明
  - ある role のモデル設定と説明を同時に変える candidate
  - どの role にも届かない説明、wire に届かない説明
- manifest の `pins` に、sample、common、candidate、設定、prompt preset の hash、承認 digest、build、設定上のモデルを固定する。run ごとに、実際に使ったモデルと、run が固定した説明の snapshot を `observed` に記録する。
  - `eval_start` は thread ごとの digest を承認の前後で照合し、承認待ちの間に sample、overlay、設定、preset が変わっていたら再確認を求める。runner も `--approved-digest` と不一致なら実行せず、各 replay の前にも照合する。

### 3. 判定は機械判定を主とし、LLM judge は補助とする

- 品質の判定は、決定的な checks と既存の検証コマンド（`--verify`）を主とする。LLM judge は意味的な品質の補助で、機械判定の退行を平均点や費用の削減で相殺しない。
- judge には実際の `changes` を渡す。「candidate は `swapped_roles` だけが違う」という前提は削除した。比較の payload は A/B の位置で表し、順序を入れ替えてもどちらが baseline か分からない。
- 両順序の判定を残し、両方 tie と、判定の食い違いによる tie（`order_disagreement`）を区別する。prompt と payload の版を judge cache の同一性に含める。
- 自己優遇の警告は、changes に基づき、説明を変えたモデルも対象にする。
- report は、初期 context、初回 request の usage、累積 usage、品質、実効差分、費用、有効性を分けて示す。
  - 初期 system / tools の量は、各 replay の runtime が `BaseContextReport` から取る概算（bytes / 4）で、実測の token ではない。初回 request の usage は provider の報告で、履歴なども含むため、ツール説明だけの token とは呼ばない。
  - usage や価格が欠ける費用は `unknown` とし、0 円にしない。agent、simulator、judge は別に示す。
- 次は判定保留（pending）とし、成功にも退行にも数えない: 説明が使われなかった、別モデルへ fallback した、replay の記録が無い、replay が未完了、checks が確定しない、judge の順序が食い違った、judge が失敗した、preflight が拒否した。
- 採用は人が判断する。baseline も candidate も pass なら、改善ではなく正常の維持として扱う。採用した設定は利用者管理の設定として反映し、新規 run から有効にする。`eval_apply_config` は agents と routing だけを保存し、この追補では拡張しない。

### 4. モデル別ツール説明は run 開始時に固定し、cache 境界を作らない

- run が入るときに、routing が選んだ候補（profile と model ID）で 1 回だけ説明を選び、その run では変えない。モデル切替には追随しない。
  - 選んだ identity、説明の map（空でも）、hash、開始時の family を snapshot として、既存の `run_contexts.config_json` に最初の provider request の前に保存する。保存に失敗したら実行を続けない。
  - restore と fork は保存済みの snapshot を使い、現在の設定から選び直さない。snapshot が無い古い record は「override なし」として復元する。壊れた snapshot は復元を拒否する。
  - 子 run は自身の開始時のモデルと設定で選び、親の map を継承しない。
  - 開始時の family も固定する。binding を編集してから restore しても、`delegate` の追記は変わらない（意図した挙動）。
- 適用は `tool_catalog` の 1 つの純粋な処理にまとめ、live loop、preview、restore、planned handoff が共用する。説明の差し替えは、role の可視性 filter、`run_js` の生成説明、harness が付ける hint や family の追記より前に行う。harness の追記は override で消せない。
- `prepare_model_switch` と送信直前の compose には、再選択も適用器も足さない。履歴、送信済みの ToolResult、cache affinity、system、model と設定は、説明のために書き換えない。
- したがって、説明の override は新しい cache 境界を作らない。設定の編集も境界ではない。既存の境界（model switch、compaction、build 更新を伴う restart の tools-only 境界）の扱いは変えない。
- build の更新で対象の tool が無くなったときは、保存済みの override を適用せず、診断を残す。再公開も別名への転用もせず、snapshot は書き換えない。新規 run は、現在の設定に未知の tool があれば実行前に拒否する。
- benchmark の記録と replay は、override が掛かる場合に provider を呼ぶ前に拒否する。
- AGENTS.md の Cache correctness gate は変更しない。cache E2E は、入力から導いた mock cache と独立した wire 検査で、通常 turn、モデル指定、`ResponsesScopeChanged`、compaction、fallback、子 run、fork、restore、planned handoff、build 更新後の restart と、境界直後の次 turn の prefix 再利用を確かめている（#836）。

### 5. 費用と停止策の限界

- 評価の費用は、既存の価格表（§8）と実測の usage から出す。usage や価格が欠けたら 0 円ではなく「不明」とする。
- `eval_start` は、承認の前に次を示す。replay と judge の組の数、agent のモデル、budget、budget の token 上限を既知の最高単価で換算した額（replay ごと、全体）、simulator と judge の別枠、停止条件。
  - **この換算額は、厳密なドル上限ではない。** budget は request が完了してから判定するため、進行中の request と並行する子 run は超過し得る。単価は最も高い値であり、典型的な request の支払いではない。simulator と judge は、この額に含めない。価格が不明なら換算も不明とする。
  - 新しいドル予算管理の仕組みは作らない。厳密なドル上限が必要なら、provider 側の強制上限を確認できるまで実行しない。
- 停止策は、既存の `eval.budget`（1 replay の delegation 木全体の token、tool 呼び出し、user turn、経過時間）だけである。超えた replay は未完了として記録し、停止を要求した後は次の replay を始めない。`evals/run-js` の watchdog や `--cost-limit` は、thread replay には無い。
- 実測の参考値（固定課題のハーネス `evals/run-js`、thread replay の予測値ではない）: r4 は 72 run で $28.407（1 run あたり約 $0.39）。暴走した 1 run は 487 request、約 $12.6 を使った。
- 有料の実行は、対象、件数、budget、換算額を示した利用者の承認のたびに行う。この ADR と計画の承認は、有料 pilot の承認を含まない。

### 6. 「適用済み」の条件と既知の制限

説明の override は、次の 4 つがすべて成り立つときだけ「適用済み」と数える。どれか 1 つでも確認できなければ、適用済みと数えない。

1. run の snapshot が、changes の (profile, route の model ID) を選んでいて、その tool を含む。
2. その tool が、run 自身の記録の `tool_names` にある。
3. その role の request がすべて、その role の run が固定した (profile, base model) に応答されている（usage の台帳で確認する。baseline と candidate の両方）。1 つの既知のモデルに固定されない role は、fallback と区別できないので判定保留にする。
4. 説明が、そのモデルの wire に届く（`BuiltinCatalog::description_reaches_wire`。preflight と同じ関数）。

既知の制限:

- **tool calling に対応しないモデル**: 送信時に tools が空になる。評価は、router が通常は tool calling に対応するモデルを選ぶことを前提にし、確認はしない。
- **`apply_patch` の説明は確認できない**: Responses の wire は説明を固定の文に置き換え、他の wire は確認していない。そのため `apply_patch` は数えず、評価ではその説明だけの override を拒否する。通常の run の設定は受理するが、Responses 系のモデルでは wire に届かない（runbook の注意欄を参照）。
- **role 単位の保守的な照合**: 3 は role 単位で判定する。同じ role の run のモデルが一意に決まらなければ、実際に説明が使われていても保留になる。
- edit interface の絞り込み（`edit_tools::narrow`）は、apply-patch interface のモデルには `edit` と `write` を、edit interface のモデルには `apply_patch` を送らない。送られない tool の説明は、4 を満たさない。
- 実際の thread での再発率と、実 provider での費用は、この追補の時点で未確認である。

### 7. この追補で変えないこと

- 別の評価基盤、ケース DB、scheduler、任意 script を埋め込む checks、一般の採点 DSL、自動最適化は作らない。
- role/category の再設計は対象外とし、妨げない。
- モデルごとの system 全文の差し替えと、tool の引数 schema 内の説明の変更は含めない。
- AGENTS.md の Cache correctness gate と Compatibility policy は変更しない。
