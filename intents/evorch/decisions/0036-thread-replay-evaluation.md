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

## Decision

過去の thread を題材にし、開始時点の状態から thread 全体を再実行する。
baseline と candidate の 2 つの構成で同じ条件のもとで走らせ、LLM が一対比較と過程の分析を行う。
arena と `evorch benchmark` はこの仕組みで置き換え、削除する。

### 1. thread 開始時点の状態を記録する

- conversation root run を開始する直前に、project root で git を 1 回だけ読み取り、次の値を storage に保存する。
  - `HEAD` の commit
  - `git diff --binary HEAD`（未コミット差分）
  - 未追跡ファイルの一覧と内容（`.gitignore` 対象は除く）
- tool 呼び出しごとの記録やロックは行わない。読み取り専用の git 呼び出しなので、ADR 0031 の待ちは発生しない。
- 差分や未追跡ファイルが上限サイズを超える場合、git 管理外の project、記録に失敗した場合は、その root を「再実行不可」と記録する。
  切り詰めた状態から再実行することはしない。
- この記録より前に作られた thread は再実行の対象外とする。

### 2. 隔離した場所で再実行する

- 実行ごとに runner が所有するディレクトリへ、記録した commit から `git clone --shared` し、差分と未追跡ファイルを復元する。
  - 元の remote は外し、`origin` は §3 のローカル bare repository に置き換える。ユーザーの checkout と `.git` には書き込まない。
  - project の絶対パスに依存する thread があるため、手順のなかで path の違いを明示する。
    ただし同じ絶対パスでの再現は保証しない。
- 再実行は専用の storage DB とイベントバスを使う。ユーザーの DB と `sidebar.json` は読み取り専用の題材として扱う。
- ADR 0032 の許可ディレクトリは mount しない。それに依存する thread では、その旨を report に残す。

### 3. 副作用を遮断し、作業に必要な環境は再実行側で用意する

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
  - ホストの evorch と同じ `/var/tmp/evorch-output-<uid>` が読み取り専用で見えるため、evorch 自身のテストが失敗する問題がある。
  - これは通常の利用でも escalation の原因になっているので、sandbox が scratch を既定の保存先として渡すよう別途修正する。
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
- provider への通信と web 系 tool だけが外部に出る通信になる。

### 4. ユーザー役を LLM が担う

- 元 thread のユーザー発言・`ask_user` への回答を、`review_evidence` の時系列で再構成し、ユーザー役の LLM に参照させる。
- root run が `Waiting` になったとき、または `ask_user` が保留になったときに、ユーザー役が次のどれかを返す。
  - 回答する
  - 追加の指示を送る
  - 終了する
- ユーザー役が見るのは、ユーザーに見える会話だけにする。tool の内部ログは渡さない。
- ユーザー役は元のユーザーの意図と制約を伝えるだけにし、元のユーザーが与えていない解決策は与えない。
  元の要求が満たされた、または満たせないと判断した時点で終了する。
- thread goal（ADR 0028）を使った thread は、初版では対象外とする。

### 5. 比べるのは baseline と candidate の 2 つの再実行

- 元 thread の当時の構成（解決済みの chain や prompt）は storage に残っていない。
  そのため元 thread の結果は baseline にせず、ユーザー役と採点の参考情報にとどめる。
- baseline は現在の構成、candidate はそれに overlay を重ねた構成とする。
  overlay は `LoadOptions.cli_overrides` で最後に深マージし、role binding（`models` / `preset` / `generation`）と `role_profile` を差し替える。
  role ごとの tool 集合は ADR 0002 で固定されているので、差し替えの対象にしない。
- 各構成で、ターン数・token・tool 呼び出し・経過時間の予算を設ける。予算を超えた実行は「未完了」として採点に回す。
- 同じ構成を複数回走らせる回数は指定できるようにする（既定は 1 回）。

### 6. LLM が採点する

各実行の終了後に、次を集める。

- 開始状態から最終状態までの diff
- ユーザーに見えた会話
- イベントから集計した指標（費用、token の内訳、cache の命中率、tool 呼び出しの種類別件数、エラーとリトライ、delegation の木、拒否された承認、経過時間）
- 指定があれば検証コマンドの結果（例: `cargo test --offline`）。検証コマンドは、同じネットワーク遮断のもとで各実行の最終状態に対して走らせる

採点は 2 種類行う。

- **一対比較**
  - 元のユーザー要求に照らして、要求の充足・正しさ・範囲の守り方・効率の観点ごとに、A / B / 引き分けと理由を構造化して返させる。
  - 提示順を入れ替えて 2 回採点し、判定が食い違った観点は引き分けにする。
- **過程の分析**
  - 実行ごとに、イベントを要約した時系列（生ログではなく上限つき）を渡す。
  - 時間と token がどこに使われたか、遠回りや重複した tool 呼び出し、delegation の妥当性、差し替えた role の振る舞いを分析させる。
  - 2 つの実行の差を、差し替えた role に焦点を当ててまとめさせる。

### 7. 費用は token 量ではなく金額で比べる

記録時点の `usage_requests.cost_usd` は使わない。
手元の DB では、正常に終わった request 22,997 件のうち 69.7% が `NULL` になっている。
subscription の provider は価格が付かないか、plan の 0 円になる。
そこで評価時に、次の手順で価格を付け直す。

- request ごとの token を、入力（cache なし）・cache 読み取り・cache 書き込み・出力に分けて保存する。
  そこに、評価時点の 1 つの価格表を適用する。
  - 価格表は models.dev と、設定ファイルの価格の上書き（`input_price` など）から作る。
  - 取得時刻を report に残す。
  - baseline と candidate には同じ価格表を使う。
- 価格の解決は現在 GUI（`crates/gui/src/model/pricing.rs`）にある。これを headless の runner からも使える crate へ移す。
- 現在の価格の付け方は、次の 2 つの料金を無視しているので、評価用に補う。
  - fast（`+fast`、service tier `priority`）の request は、基本の model ID で記録され、標準の料金で計算されている。
    models.dev には fast の料金（`experimental.modes.fast.cost`）が別にある。gpt-6-astra では標準の 2 倍。
    usage の記録に service tier を残し、その料金を適用する。
  - models.dev の入力量による段階料金（`cost.tiers`。gpt-6-astra は 272k token を超えると入力が 2 倍）を読んでいない。
    request ごとの入力量で段階を選ぶ。
- models.dev で解決できない model は、設定ファイルの model エントリで手動指定する。
  - 既存の `input_price` / `output_price` / `cache_read_price` / `cache_write_price` / `context_window` と `metadata_source = "manual"` を使う。
    別の models.dev エントリを指す `metadata_ref` も使える。
  - fast の料金と段階料金を手動で書く欄は、まだないので追加する。
- subscription の provider（Codex・Claude・Kimi のサブスク）では、対応する API model の公開価格で「API 換算の費用」を出し、そう明記する。
  対応付けは models.dev の provider 候補と設定で決め、plan の 0 円は使わない。
- 価格を付けられない request が 1 件でもあれば、その比較は「費用不完全」とし、該当する model を列挙する。0 円とはみなさない。
- 2 つの実行が同じ model の同じ prompt 先頭を共有すると、provider の prompt cache が後の実行だけを安くする。
  これを避けるため、各実行の system prompt の先頭に実行ごとの識別子を入れ、実行をまたいだ cache の共有を断つ。
  実行内の cache はそのまま効かせる。
- 一対比較の効率の観点には、この金額を渡す。

### 8. 採点 model の設定

採点用とユーザー役用の model は評価設定で指定する。
採点 model が比べる候補と同じ model family なら、自己優遇のおそれとして report に警告を出す。
複数 thread をまとめて走らせた場合は、thread ごとの勝ち・負け・引き分けの件数を並べる。
費用も thread ごとに並べる。勝敗を総合順位には変換しない。

### 9. 入口

- 初版は CLI とする。
  - `evorch eval run --thread <id> --candidate <overlay.toml> --output <dir>`
  - `evorch eval report --output <dir>`
- 出力ディレクトリには、各実行の storage DB、diff、会話、指標、採点結果を残す。
  `evorch inspect` を出力側の DB に向けて使えるようにする。
- GUI からの起動と表示は後続にする。

### 10. 置き換えと削除

1. arena を削除する。
   - `crates/arena`、GUI の Arena パネルとそのテスト、`storage::eval` / `eval_variant`、`docs/arena-acceptance.md` が対象。
   - `eval_trace` の読み書き API は消す。memory ledger は trigger で追記専用なので、既存の行と部分 index は読まれない履歴として残す。
   - 古いレイアウトを読めるように `PanelKind::Arena` の deserialize と、読み込み時のタブ除去は残す。
2. thread 開始時点の状態を記録する（§1）。
3. 隔離した再実行とユーザー役を実装する（§2–§4）。
4. 採点・費用の算出・report を実装する（§5–§9）。
5. `evorch benchmark` を削除する。
   - 対象は CLI、runtime の benchmark 専用の分岐（recorder・replay・model の凍結・tool 出力の境界）、`SnapshotStore`、`docs/benchmark.md`、`examples/benchmark`。
   - 再実行の仕組みが動いてから消す。

## Compatibility

- thread 開始状態の保存に storage マイグレーションを 1 つ追加する。
- `eval_trace` は API だけを消し、マイグレーションは追加しない（§10 の 1）。
  ADR 0035 が arena の評価帰属に残した `oracle` alias は、読み手がなくなるので API と一緒に消える。
- 設定には評価用の節を追加する（ユーザー役・採点の model binding と既定の予算）。既存の節は変更しない。

## Validation and limits

mock-openai と scripted model を使って、次を決定的にテストする。

- 開始状態の記録と、上限超過時の「再実行不可」
- clone と復元の一致
- ユーザー役との往復（回答・追加指示・終了）
- ネットワーク遮断、ローカル bare repository への pull / push、承認の自動拒否
- 予算超過
- 提示順を入れ替えた採点の集約
- 価格表による費用の付け直しと「費用不完全」の判定、実行をまたいだ cache 共有の遮断
- report の生成

実際の provider での評価は、利用者の手元で行う。

この仕組みは元 thread の厳密な再現ではない。

- model の揺らぎ、provider の cache、ユーザー役の応答の違いで、同じ構成でも結果は変わる。
- ネットワークに依存する作業、許可ディレクトリに依存する作業、goal を使う作業は、初版では正しく評価できない。
- LLM の採点には位置の偏りと自己優遇が残る。順序の入れ替えと警告で弱めるが、なくすことはできない。
- 1 thread の勝敗を一般的な model の順位として扱わない。構成を採用する前に、複数の代表的な thread で比べる。
- 実行ごとに provider の費用が 2 実行分以上かかる。
