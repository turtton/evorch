# 5a-V 設計メモ DG-1〜DG-3 の独立レビュー

## 判定

| メモ | 判定 | 着手への影響 |
|---|---|---|
| DG-1（live collector） | **承認** | V-admission は着手できます。 |
| DG-2（host の所有と finalization） | **修正要求**（must 2 件） | V1b-1 と V1b-2b は再レビューまで着手できません。 |
| DG-3（RecipeSource） | **承認** | V1b-2a は着手できます。 |

- DG-2 の再レビューは、下の M1 と M2 の差分だけで足ります。
- V1a-2 の配線は M2 の決定を待ちます。必要な accessor が、今の V0-switch に無いためです。

確認した範囲は次のとおりです。

- 読み取りだけで確認しました。ビルドと試験は実行していません。
- メモが挙げた file:line は、main `794fa1ed` で全件を開いて照合し、すべて内容と合っていました。
- V0-switch は worktree `.worktrees/5a-v0-switch`（`686fcc75`）を読みました。GitHub 上の PR の状態は見ていません。

## must（DG-2）

### M1. continuation が finalization の完了を待つと、公開 restart は毎回 FailedClosed になります

DG-2 §3.2 の手順 6 は「continuation は start receipt／既存 `wait_finalization` の結果を観測し」と書いています。`wait_finalization` を待つ実装は成立しません。

- **根拠**
  - `FinalizationOwner` の continuation は、管理操作の task の中で待たれます（`restart/public/implementation.rs:217-245`、`admin/service.rs:475-484`）。
  - この task は host の `area_tasks` に載り、停止通知を無視します（`admin/service.rs:278-284`、`bootstrap/mod.rs:648-654`）。
  - capture 後の stage 4 は、`area.shutdown()` で全 task の終了を 1 秒以内に待ちます（`bootstrap/planned_exit.rs:12,255-302`、`areas/tasks.rs:278-288`）。
  - `wait_finalization` は失敗したときしか返りません（`restart/finalization.rs:294-311`）。
  - 既存の fake は `AwaitRestartRecovery` をすぐ返しています（`tests/public_restart_coordinator/fixture.rs:110-121`）。
- **影響**
  - 成功する経路でも stage 4 が期限切れになり、exec に進めません。
  - V1b-2b の試験は capture 後まで通さないので、V-exec まで気づけません。
- **必要な修正**
  1. start が受理されたら、continuation は直ちに `AdminExecutionOutcome::AwaitRestartRecovery` を返します。`wait_finalization` は待ちません。
  2. 配送待ちと reply 待ちは、loop がどの経路で終わっても必ず解放します。経路は signal、accept の失敗（`lifecycle.rs:264`）、drain 中の signal（`lifecycle.rs:233`）です。解放されないと、`host.shutdown()` の `area_tasks.shutdown()`（`bootstrap/mod.rs:963`）が止まります。
  3. 試験を 2 行足します。
     - start 受理後に管理 task が終わり、stage 4 が通ること。
     - 未処理の依頼を残して loop が終わっても、`host.shutdown()` が完了すること。

### M2. 「V0-switch が中央入力を先行定義する」が、現物とも型の定義順とも合いません

DG-2 §7、DG-1 §6、DG-3 §6 は、V0-switch が入力と accessor を先に入れる前提です。計画 §4 も同じ前提です。

- **根拠**
  - V0-switch の入力は `Inputs { host, backend }` だけです（`admin_composition.rs:16-19`）。`bootstrap/mod.rs` は変更していません。
  - 既存の型だけで足せる accessor も、今は無い状態です。
    - `StorageConfig` と writer の handle は製品から取れません（`bootstrap/mod.rs:131,142-144`）。
    - F4-P の harness は試験専用の `host.composition.storage_writer` を使っています（`product_exec/old_core.rs:209-213`）。
    - `StorageQuarantineStore::new` と `OwnedGraphsFacade::new` は、どちらもこの 2 つを要求します（`quarantine/store.rs:19`、`recovery_disposal/facade.rs:76-82`）。
  - collector の handle と finalization の sender は、V-admission と V1b-1 が初めて型を定義します。V0-switch では参照できません。
  - 自動 recovery は `activate_after_ready` の中で始まります（`bootstrap/mod.rs:766-780`、呼び出しは `planned_entry.rs:70`）。DG-1 §3.6 は、その前に collector の hook をつなぐことを求めています。この接続を担当する PR が決まっていません。
- **影響**
  - V1a-1、V1a-2、V1b-2b が facade と owner を組み立てられません。
  - 追記できるのは V1b-1 だけなので、配線が B2 (a) 待ちになります。
  - hook の接続が自動 recovery より後になると、未処分が消えず、2 回目の restart が通らなくなります。安全側ですが、F4-E の「連続 2 回」が落ちます。
- **必要な修正**
  1. DG-2 に、中央入力と accessor の確定表を 1 つ置きます。列は、項目、型を定義する PR、中央ファイルへ入れる PR、起動順での位置です。
  2. 既存の型だけで足りる項目（`StorageConfig`、writer の handle）を、V0-switch の merge 前に入れるか、V1b-1 に回すかを決めます。
  3. collector の構築と hook の接続を、自動 recovery の開始より前に置く PR を決めます。
  4. 決定に合わせて、計画 §3.3 の依存を直します。V1b-1 に回すなら、V-admission の後に V1b-1、その後に V1a-2 という順になります。

## should（follow-up）

### DG-1

- **crate の境界**：workbench-host は evorch-core に依存しません（`evorch-core/Cargo.toml:44`）。collector は `RestartId`（`restart.rs:45`）を保持できず、`UndisposedAdmission` も実装できません。collector は型に依存しない操作だけを持ち、trait の実装は evorch-core に置いてください。そのファイルの担当 PR も決めてください。
- **予約の解放**：解放の規則（§3.4）はありますが、契機と試験がありません。解放されないと、cancel の後の restart と Q2 の操作が拒否され続けます。「cancel または期限切れの後に、再度の admit と Q2 の操作が通る」試験を足してください。
- **初期化と自動 recovery の並行**：Rebuilding の間に確定した結果を、古い初期観測が上書きしない規則と、barrier の試験を足してください。
- **lock の順序**：逆順を検出する試験の行がありません。
- **binding の観測世代の hook**：DG-1 は DG-3 に、DG-3 は V-admission との調整に委ねています。担当 PR を 1 つ決めてください。

### DG-2

- **signal 優先の実現方法**：loop の `select!` は順序を固定していません（`lifecycle.rs:194`）。全体を `biased` にすると、idle・client・accept の公平性が変わります。mailbox の分岐の中で、signal の latch を待たずに確認する方式を明記してください。
- **`old_epoch` の情報源**：「hello／host epoch」は、invalidation の後は別の値になります。
  - hello の epoch は invalidation で進みます（`projection.rs:115-116`、`backend.rs:195`）。
  - 管理用の epoch は起動時の値のままです（`admin/service.rs:91-108,642-653`）。
  - control drain の検査は hello の値と比べます（`lifecycle/control.rs:30-37`）。
  - 管理用の値を使うと、invalidation を経た core では capture 後に必ず失敗します。hello の現在値と明記し、invalidation の後に restart する試験を足してください。
- **successor epoch の上限**：successor 側は `u64::MAX` を拒否します（`bootstrap/planned.rs:109-111`）。割り当ての時点でも拒否してください。
- **capture 前の確定失敗**：誰も cancel しないと、Sealed のまま最大 300 秒、業務が止まります。既存の再 probe の失敗（`implementation.rs:237-240`）と同じく、すぐ cancel する主体を決めてください。
- **`Arc<Instance>` の drop 順**：loop 側の clone が残ると、host の後に instance という順序が保証できません（`main.rs:191-194`）。

### DG-3

- **試験の行と PR の対応**：§5 の表は全部 `public_restart_compose::recipes` に置く書き方です。競合の 2 行は V-admission の proof と preparing の変更が要ります。行ごとに V1b-2a か V1b-2b かを書いてください。
- **preparing の変更の担当**：失敗を FailedClosed から cancel に変える差分（`restart/preparing.rs:116-125`）は、計画のどの PR にもありません。V1b-2b はすでに L なので、V-admission か別の小さい PR を勧めます。
- **最終照合の排他**：今の最終照合は `manual_launch_gate` の外で比較しています（`restart_transaction.rs:224-235`、`quiesce.rs:528-555`）。排他の中へ移す差分も、同じ担当に含めてください。
- **既存 root への後付け**：最終照合の後に、drain 中の入力が既存の plain root へ goal・lane・team を付ける場合の結果が、表にありません。
- **内部 run と CLI の行**：
  - learning・evaluation などの内部 run を、表に明記してください。
  - main には CLI が core で run を起こす経路がありません（`evorch/src/cli/run.rs`）。「CLI invocation」の fixture を、試験入口なしでどう作るかを決めてください。
- **拒否理由の表示**：対象外の run による拒否は、今は「conflict (the target changed)」と表示されます（`implementation.rs:149-153,370-372`）。利用者は、どの run を終えればよいか分かりません。有界な種類と次の操作を返してください。

### 計画書への反映（3 本共通）

- 依存を 2 つ追加します。V-admission の後に V1a-2（DG-1 §6）と、V-admission の後に V1b-2a の proof 接続（DG-3 §6）です。
- 計画 §4 の表に無いファイルの担当を決めます。
  - `restart/public/implementation.rs`
  - `restart/preparing.rs`
  - runtime の hook と getter
  - `workbench-host/src/lane.rs` の観測世代

## 承認したメモの制約

### DG-1（V-admission、V1a-2、V1b-2b）

| ID | 制約 | 根拠 | 検証 |
|---|---|---|---|
| G1-01 | collector は host の寿命で 1 つ。Q2 facade と admission が同じ handle を共有する。空集合を返す代替を作らない。未初期化・poison・revision の枯渇は unavailable。 | §3.1、§3.2、V-07 | rebuild・poison・枯渇の試験、構築箇所の grep |
| G1-02 | `admit` は collector の lock を保持したまま、Ready、pending 0、epoch、revision、空集合、予約なしを確かめ、遷移を 1 回だけ呼ぶ。lock を放して比べ直さない。`SnapshotClock` を参照しない。 | V-07、§3.4 | barrier の競合試験 |
| G1-03 | collector の lock 中と同期 commit 中は、DB・filesystem・owner・provider・host mutex に触れない。closure は memory の再照合と `request_inner` だけ。 | V-07、Q2 §5.3、§3.5 | I/O spy、`check-q2-boundaries.sh` |
| G1-04 | 未処分の判定に効く変更は、すべて effect の前に同期で fence を登録する。除外は確定した readback だけ。Unknown・panic・Drop で解除しない。event の通知で代用しない。 | §3.3 | PR 本文の callsite 一覧、経路ごとの試験。網羅できなければ公開しない |
| G1-05 | 判定は `GraphOwnershipSummary` の 2 述語を共用する。件数は全件、参照は最大 64。64 thread 超は blocker、警告は blocker にしない。cold と foreign は数えない。 | Q2 §2.5、Q2-10 | 64／65 件の試験 |
| G1-06 | lock の順序は、public state、collector、coordinator、runtime の順。下位の lock を持ったまま collector を呼ばない。 | `implementation.rs:2`、§3.4 | 競合試験 |
| G1-07 | admission の後、Q2 の新しい effect は Conflict か Busy。解放は取消か通常復帰の確定後だけ。FailedClosed では解放しない。 | §3.4 | 取消後の再 admit の試験 |
| G1-08 | hook は自動 recovery の開始前につなぐ。つなぐ前に effect が始まりうる構成では Ready にしない。 | §3.6 | successor の起動順の試験 |
| G1-09 | revision は collector 内の単調な世代。同じ集合に戻っても再利用しない。読み取りでは進めない。照会と `admit` は同じ revision を使う。 | §3.2、§3.6 | A→B→A と 0→1→0 の試験 |
| G1-10 | V-admission の後に V1a-2。空の collector の暫定 constructor を作らない。V-admission は composition の根と `bootstrap/mod.rs` を編集しない。 | §6、V-11 | diff |
| G1-11 | 同期は clock・barrier・channel・join だけ。製品に試験入口を足さない。第二の時計・状態機械・receipt による権限を作らない。 | V-18、DC-32、DC-39 | diff、レビュー |

### DG-3（V1b-2a、V1b-2b）

| ID | 制約 | 根拠 | 検証 |
|---|---|---|---|
| G3-01 | `CoreOwned` にするのは、正の確認に通った plain な会話 root と、その通常の child だけ。CLI・unbound・不明・foreign を昇格しない。owner の start／claim、binding の生成、新しい権限や永続 schema を作らない。 | V-09、DC-35、§3.1 | `recipes` の採否表 |
| G3-02 | lane・team・goal は今回すべて拒否する。`None` で隠さない。情報源のどれか 1 つが示せば対象外。graph の全 member を検査し、1 件でも対象外なら要求全体を拒否する。 | V-09、§3.2 | 負例で gate・tx・park・provider が 0 |
| G3-03 | 読み取りの失敗・破損・欠落を、空の map や `None` にしない。active な run が無い場合（空で成功）と区別する。 | §3.2、§3.4 | storage 不可と破損の負例 |
| G3-04 | 情報源は、現在の binding、runtime の live な metadata、storage の descriptor。保存済みの recipe・receipt・UI の申告から所有方針を作らない。path・argv・env・prompt を運ばない。 | §3.1、DC-30、DC-39 | diff、DTO の検査 |
| G3-05 | host mutex の下では binding をコピーするだけ。DB は reader か blocking worker で読む。 | §3.3、Q2 §5.3 | I/O spy |
| G3-06 | map は権限ではない。観測世代と結び付け、inspect と commit の間に変わったら拒否する。未接続のときに「常に一致」とする既定を置かない。 | §3.3、DG-1 §3.5 | inspect 後に付与する負例 |
| G3-07 | PreparingDrains 中の対象外の run は、host gate が Preparing になる前に取消で返す。park して未処分にしない。最終照合と受付の閉鎖は同じ memory の排他で行う。V1b-2a だけで競合に安全とは言わない。 | §3.5、DC-32、V-19 | barrier の競合試験 |
| G3-08 | 正例は、root と child が既存の recovery、現在の authority、型付きの予約、activation まで通ること。child の descriptor と親子関係を保つ。 | §5 | compose 試験。exec は V-exec |
| G3-09 | V1b-2a の最初の作業は、正の確認（`chat_runs`、会話の設定、permit の一致）が成り立つかを、登録経路の列挙で確かめること。成り立たなければ拒否側に倒し、親へ返す。 | §3.1 の「推測・要実装検証」 | PR 本文の経路一覧 |
| G3-10 | recipe の構築で、履歴・ToolResults・affinity・model・settings を書き換えない。map の返却・照会・再接続・ACK の再送で activation しない。 | §4、V-15、DC-33 | V-cache、`check-cache-contracts.sh` |

## 未解決の確認事項

- **V0-switch の扱い**：merge 前に accessor を足せるか。足せなければ V1b-1 に回します（M2）。
- **preparing の失敗分類の変更**：#710／R の担当の合意と、担当 PR が要ります。
- **lane・team・goal の拒否**：これらの run が残る間、公開 restart は使えません。V-09 は許していますが、利用者への影響を親が了承しているかを確かめてください。
- **内部 run による拒否**：learning などが動いている間、inspect が拒否する可能性があります。capture の対象に入るかは確認していません。入るなら、T2 と T3 が不安定になります。
- **`LoopHost` の借用**：成立するかは、DG-2 の記載どおり未検証です。コードを読んだ限り、loop が host から借り続ける値はありません（`lifecycle.rs:172-176`）。
