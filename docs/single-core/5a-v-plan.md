# 5a-V：公開 restart と 19a までの計画

この改訂は、公開機能の実装状態と製品経路の検証状態を分け、旧21 PRとT1〜T4公開列車を単一のPR Aにまとめる。

決定の出典：

- `question-41eda9b4579b24b09e86a96a7e751928`：19aの範囲、B3-09、公開restart、PR構成、mutation、レビュー運用。
- `question-0d0be08211df9f38f725636e5a62affa`：実行中CLI invocationがある場合のrestart受付前拒否。

## 現状

| 項目 | mainにある範囲 | 状態の扱い |
|---|---|---|
| 公開スイッチと登録 | V0-switch／V0-reg、公開境界検査、製品入口の登録 | `MANAGEMENT_PUBLISHED` は `true`。PR Aで公開restartを完了（#781、merge `6a5e782d`）。公開と製品経路の検証は別の状態として記録する |
| restart composition | quarantine／owned-graphs install、admission、live collector、targets／recipes／finalization、およびcold起動・planned successor両入口のcomposition | これらの実装を作り直さない。結合で判明した不具合だけを直す |
| 5a-Vの公開・検証 | restartの公開はPR Aで完了。公開restartの製品経路とsuccessor確認を含む | placeholder試験を完成・製品検証の証拠に数えない |
| 17 通常CLI | 通常CLIはcore clientとしてinvocationを開始・追跡する | PR Bで完了（#780、merge `4e053793`）。`--db`／`--web-tools-enabled`／`--run-js-enabled` は廃止。実行中invocationがあるrestartは理由付きで拒否する |
| 17c／17d／17e | eval、maintenance、offline／inspect／benchmark境界の主要経路 | main収録済みとして扱い、19aでは回帰確認する |
| 18a／18b | idle基盤とcrash後の再接続・共有予算 | PR Cで完了（#779、merge `a0d21082`）。異常終了の証拠がある場合だけ数え、予算は5分に3回 |
| 19a | 既定起動をcore clientにし、core管理更新操作をGUIに提供 | PR Dで完了（#782、merge `e61a5629`）。引数なし起動はcore client、embeddedは`--legacy-embedded`／`--demo`を明示。SetThreadExtraReadDirs画面は後続 |

**19aまでの一区切り：完了（PR A〜Dはすべてmerge済み）。**

**公開と検証完了は別の状態として記録する。** 公開は製品の管理機能を有効化する変更がmainに入った状態。検証完了は製品から公開restartを実行し、successorへの移行と基本の再開を確認し、製品ELFのexpiry（capture後の実時間10秒、exit 74、所有する子の回収）も確認した状態である。一方を他方の証拠として扱わない。

## 19aまでのPR

順序は **A → B → C → D**。各テーマを1 PRにまとめ、ファイル数を分割上限にしない。登録だけのplaceholder先行PRは作らない。

### PR A — 公開restart

**状態：完了**。#781（merge `6a5e782d`）。実行中turnからrestartを要求すると`Cancelled`になる不具合を修正し、管理CLIに`core quarantine`／`core graphs`、GUIにrestart機能を追加した。

**範囲：** 公開スイッチ、公開境界検査、GUI restart接続・表示、管理CLIのrestart／Q1／Q2導線、製品からsuccessorへの基本restartと関連試験。

**必須条件：**

- 実製品の公開要求からsuccessorへ移行し、単一coreと対象runの再開を保つ。二重実行・データ消失を起こさない。
- GUIが確認、状態、拒否理由、結果不明を正しく表示する。Q1／Q2の復旧操作は管理CLIから利用できる。
- 未認証Web／Agentへの管理公開をしない。既存の確認・対象照合・監査境界を迂回しない。
- 失敗を成功表示せず、既存の期限・終了処理を無制限に延ばさない。
- restartで送信済み履歴、ToolResult、cache affinityを改変しない。

**基本検証：** featureless製品、GUI起源の通常run、管理CLIからの1回のrestartを確認する。PID／epoch、lifetime lock、再開、二重実行なしを確認し、入力由来mock cacheと独立したwire assertionsを使う。製品ELFのexpiryは実時間10秒、exit 74、所有する子の回収を確認する。未認証拒否、古い確認／監査失敗、未処分graph拒否は既存contract試験を再利用して代表セルを確認する。CEFと二回連続restartは含めない。

### PR B — 通常CLIをcore clientにする（17）

**状態：完了**。#780（merge `4e053793`）。`--db`／`--web-tools-enabled`／`--run-js-enabled`を廃止し、実行中のCLI invocationがあるrestartは理由付きで拒否する。

**範囲：** 通常run、invocationの所有・停止、terminalの質問／承認、CLI E2E。5b-Rを依存にしない。

**必須条件：**

- 通常CLIは設定DB／runtimeを所有せず、connect-or-startのみを使う。失敗時にembedded fallbackしない。
- Ctrl-C／切断／終了は、そのinvocationのrun incarnationとsubtreeだけを停止する。他runへの入力はqueue中なら自分の入力だけを取消し、投入済みならdetachする。ACK不明時に無条件再送しない。
- 質問・承認、`--no-wait`、終了コード`0/1/3/130`を契約どおり扱う。
- 5b-R完成までは、実行中CLI invocationがあるrestartをgate／capture前に理由付きで拒否する。拒否判定とinvocation受付を排他し、管理CLI接続自体は拒否理由に数えない。
- crash後にCLI runを自動復活させない。

### PR C — crash予算と障害再接続（18b、18aの差分）

**状態：完了**。#779（merge `a0d21082`）。異常終了の証拠がある場合だけ数え、予算は5分に3回。異常切断時にGUIは予算内で自動再起動する（親の決定：`question-182c5ee382012a1f6bf339e9ee339bf9`）。

**範囲：** config directory単位の共有・永続crash予算、既存起動排他との接続、GUI connection actorとCLIのcore-loss処理、状態表示、invocationに伴うidle／status差分。

**必須条件：**

- 自動再起動予算をconfig directory単位で5分に3回とし、複数clientで共有・永続化する。
- 同時再接続で二重起動・予算二重消費を起こさず、予定restartとAlreadyRunningの合流をcrashに数えない。
- 予算超過・台帳異常を無限再試行で隠さず、手動復旧の理由を表示する。version mismatchを理由に稼働中coreを強制停止しない。
- CLI runや結果不明のmutationを再送・自動復活させない。保持すべき作業がある間はidle終了しない。

### PR D — 単一coreを既定にする（19a）

**状態：完了**。#782（merge `e61a5629`）。引数なし起動をcore clientにし、embeddedは`--legacy-embedded`／`--demo`を明示した場合に限定。flakeのlibexecに`evorch-core`を追加した。管理更新画面はproject登録・名前変更、許可ディレクトリ、provider、Agent Webツールを含み、SetThreadExtraReadDirsは未対応と表示する（ユーザーの決定：`question-9fda1d95f934b57b308cad34e661e0f2`）。

**範囲：** GUI既定起動、設定directory、core／GUI／CLI／既存js-workerの配置・起動、既定起動統合試験。管理更新操作の欠落があれば、その最小差分を含める。

**19aの条件はPR A〜C、既定起動、および既存の管理更新操作を保つことに限定する。** 特別なflagなしのGUIがcore clientとして動き、失敗時にembeddedへ戻らない。同一configのGUI／CLIは単一coreを共有し、配布物からcoreとjs-workerを起動できる。通常操作、公開restart、障害表示に誤動作・hang／flakyを残さない。17c／17d／17eとjs-workerの既存smokeを回帰する。

## 後続に回す範囲

- **5b-R／CLI continuity：** 実行中CLIが予定restartをまたいで継続する機能。19a後に行い、それまではPR Bの受付前拒否を使う。
- **F4-E：** CEFを含む確認、二回連続restart、旧世代残存／リークの確認。基本の一回restartはPR Aで重複計上しない。20の変更前に基準を採る。
- **O-V2／O-V3：** 実browser／GUIのagent observation、およびobservation＋Stop／restart＋cacheの横断確認。O-V2の後にO-V3を行う。
- **Q1／Q2専用GUI画面、read-only GUI parity：** 管理CLIで代替する。誤表示・事実と異なる説明は後回しにしない。
- **20：** 旧owners.db／claim／shared_feed等の所有権撤去。19a後。
- **19b：** client crate内部依存allowlistのゼロ化。20後。
- **SetThreadExtraReadDirs画面：** 19aで後回しにした。現在は未対応と表示する。
- **CLI crash-continuity結合試験：** 実際にcoreをcrashさせ、実行中CLIのrunが復活しないことを確認する。
- **配布物のGUI／CLI共有smoke：** 配布物を実際に配置した状態で、GUIとCLIが同じcoreを共有することを確認する。
- **restart preflightの観測変化（#781 S3）：** preflight内で観測が変わると一部の要求が`Cancelled`になる。安全側に倒れる挙動で、follow-upとして残る。
- **Agent Webツール設定の現在値取得：** coreから現在値を取得する手段を追加する。
- 全callerへのmutation展開や統計性能gateは19aの前提にしない。新しい安全境界で効果が高いmutationのみ任意で行う。B3-09は別途縮小した条件で扱う。

## 共通のレビュー・must運用

- mustは製品の誤動作、データ破損、セキュリティ、hang／flaky、事実と異なる記載に限定する。
- レビューは2往復までとし、それを超えた場合は親が裁定する。独立Reviewerは維持する。
- mutationは効果がある代表箇所だけ任意で実施し、必須条件にしない。
- cache契約は維持し、入力由来mock cacheと独立wire assertionsを使う。
