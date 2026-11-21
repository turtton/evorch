# DG-1 — 未処分 graph の live admission

## 0. 前提・範囲

設計提案であり、実装済み・試験済み・承認済みという意味ではない。
DG-1〜DG-3 をまとめて、実装担当者・Advisor とは別 context の Reviewer による独立レビューへ出す（V-06、計画 §3.1／J3）。通過前に V-admission を始めない。

正本：
- [承認済み計画](5a-v-plan.md) §3.1、§4、V-admission、V1a-2、V1b-2b。
- 計画レビュー制約 `/var/tmp/evorch-output-1000/22/c80fc470-c898-4638-815a-e509155d3f30.txt`（特に V-06／07／11／14／18）。
- 初回レビュー `/var/tmp/evorch-output-1000/19/1be185d9-c04d-4337-b058-d04e2184f1f0.txt` M2。
- [Q2 設計](q2-design-final.md) §2.5、§5.1〜5.5、§6。
- 全体計画：`thread-9gkyvvvf`、`run-01m7tfrtj9kpxk7sxzpptzmt9c#331.3` §2.5、DC-32〜35／39／40。

コード根拠は main `794fa1ed79fec4a15ba805c86f54db9414df27e8`（`f794cd36` の子孫）での行番号。以下のコードパスは repository root 相対。新しい型・API はすべて提案である。

## 1. 問題

1. `UndisposedAdmission::admit` は同期 commit 中に呼ばれる。revision と空集合の検査から gate 遷移までを一つの排他区間にしなければ、照会後の Release／Resume と restart がすれ違う。
   - `crates/evorch-core/src/restart/public/implementation.rs:38-46,168-210`
2. 現行 Q2 の aggregate は DB を読み、診断用 `SnapshotClock` で revision を付ける。これは予約でも CAS でもない。revision の加算値や診断 fingerprint を受付権限に転用しない。
   - `crates/workbench-host/src/bootstrap/recovery_disposal/facade.rs:219-234`
   - `crates/workbench-host/src/bootstrap/recovery_disposal/snapshot.rs:5-24`
   - `crates/workbench-host/src/bootstrap/recovery_disposal/view.rs:408-486`
3. **lock を追加するだけでは V-07 を満たさない。** 現行の `admit` closure は `preflight_restart_capture` を呼ぶが、その先には DB の `run_context`／`resume_intent` 読み取りがある。これも lock 外へ分ける必要がある。
   - `crates/evorch-core/src/restart/public/implementation.rs:200-209`
   - `crates/runtime/src/runtime/restart_transaction.rs:93-128,154-167`
   - 既存の「DB preflight と memory-only consume の分離」は同ファイル `197-234` にある。別の capture 境界を作る理由にはしない。

## 2. 選択肢と比較

| 案 | 利点 | 欠点・判断 |
|---|---|---|
| DB 再読＋`SnapshotClock` 比較 | 現行診断の再利用が容易 | I/O と TOCTOU が残る。明示的に不採用 |
| 非同期通知／定期 refresh だけの mirror | 読み取りは速い | write と通知の間に空集合を受理できる。通知欠落でも通る。不採用 |
| Q2 live registry＋変更開始の同期 fence＋確定後の再投影 | commit を memory-only にでき、Unknown を閉じた状態で保持できる | 全変更入口への接続と lock 順序の試験が必要。**推奨** |
| collector lock を持って DB・owners を読む | 同一 process の観測を直列化しやすい | V-07 と Q2 の mutex 下 I/O 禁止に反する。不採用 |

## 3. 推奨案

### 3.1 所有と責務

Q2 の live collector を host の寿命で一つだけ構築する。Q2 facade と restart admission adapter は同じ handle を共有し、各々で collector を作らない。

- 上位の所有・DTO 投影は `workbench-host::bootstrap::recovery_disposal` に置く。
- runtime の graph reservation／physical-owner の変更入口には、host に依存しない小さい observation/fence interface を置く。runtime → workbench-host の依存は作らない。
- collector は authority ではない。owner permit、実行 authority、graph token、durable CAS の既存検査を省略させない。
- collector は uninitialized で生成する。空集合を返す default／不在時の代替を作らない。
- install は管理スイッチ off のときも構築するが、off で AdminService が collector を呼ばないという V-01 は維持する。

現在の graph reservation は process-local で、manual の予約取得は memory-only、automatic は予約前に DB を読む。collector lock から automatic の API 全体を呼んではならない。
根拠：`crates/runtime/src/runtime/recovery_control.rs:1-38,320-336,357-372`。

### 3.2 revision と投影

概念型：

```rust
// 新設。field と更新 constructor は非公開。
struct LiveGraphCollector {
    state: Mutex<LiveState>,
}
struct LiveState {
    core_epoch: u64,
    revision: u64,                 // checked_add。再利用・wrap 禁止
    readiness: Readiness,           // Rebuilding / Ready / Uncertain
    graphs: GraphFactsByIdentity,   // 完全な membership。表示の64件とは別
    pending: PendingObservations,
    restart_reservation: Option<RestartId>,
}
struct ObservationTicket { /* graph scope, revision, effect identity */ }
```

新設 API の概略：

```text
snapshot() -> Result<UndisposedGraphs, AdminFailure>       // memory-only
begin_change(scope) -> ObservationTicket                 // 同期 fence
publish_confirmed(ticket, facts) -> Result<(), ...>       // I/O 結果の検証済み投影
mark_unknown(ticket)                                     // exclusion を推測しない
admit(revision, transition) -> Result<RestartId, ...>      // core 側の既存 trait を実装
```

revision は collector 内の観測世代であり、DB revision の和でも wall clock でもない。初期化完了、変更開始、確定結果の公開、Unknown 化、警告への移動で進める。集合が A→B→A、count が 0→1→0 でも元の revision に戻さない。変更のない read では進めない。枯渇・poison・初期化失敗は unavailable とし、空へ変換しない。epoch が違う確認は既存の core epoch 検査でも拒否する。

graph key は封印済み adopted inventory に限定し、任意の wire ID で registry を増やせない。pending ticket は既存の graph reservation／共通 continuation の上限内とし、容量不足は新規操作を拒否する。表示参照の64件制限を内部の完全性制限に転用しない。

未処分述語は `GraphOwnershipSummary::is_undisposed`／`is_incomplete_disposal` を共用する。別の「restart 用定義」を書かない。
根拠：`crates/workbench-host/src/admin/bindings.rs:90-142`。

| 観測 | 投影・受付 |
|---|---|
| 検証済み adoption、保持した自己世代、未 activation、未 cleanup 完了 | 未処分。ManualPending／ManualRunning も除かない |
| 同世代で失効済みだが cleanup 部分成功 | 未処分のまま |
| activation の FULL commit 確定 | 未処分から除外 |
| 必要な自己処分が全て確定完了 | 未処分から除外 |
| 失効確定後に generation が変化し残処分不能 | `incomplete_disposals` の警告。blocker ではない |
| 失効／activation の ACK 不明、読み取り失敗 | exclusion を確定しない。受付不可 |
| cold、foreign、元から別 generation | 未処分に混ぜない。新規 claim で集合に入れない |
| 自己 graph が64 thread超 | 未処分・restart blocker。表示専用でも除外しない |

### 3.3 更新プロトコル（通知遅延を正しさに使わない）

1. eligibility／所有観測／処分進捗に影響する処理は、**effect より前**に collector の短い lock を取り、ticket と pending fence を登録する。
2. lock を放す。既存の graph token、owners guard、main DB FULL transaction、scratch cleanup を従来の順序で行う。collector mutex を I/O 越しに保持しない。
3. 元の supervisor が DB・owner の readback を行う。ticket と対象の世代がまだ一致することを検証し、短い lock の下で facts と revision を公開する。古い readback は新しい pending を解除できない。
4. Unknown／panic／supervisor 消失では fence を自動解除しない。query と同 ID replay は effect を再実行しない。元の supervisor の readback が確定したときだけ解消する。

必須の接続先は次のとおり。単に Q2 の UI 操作だけを囲う実装では不足する。

| 変更経路 | 接続する位置 |
|---|---|
| physical adoption と保持 permit の seal | 初期 inventory 構築中は Rebuilding。seal と全 graph 観測完了後に公開 |
| 自動 initial recovery／Blocked retry | automatic reservation の memory 部分へ入る前に fence。DB 事前検査は lock 外 |
| 手動 Resume／Release | commit の token 取得より前に fence。失敗時も確定結果だけを公開 |
| activation、失効、cleanup progress、late ACK 解消 | FULL commit／readback に基づいて facts 更新。要求受領だけでは除外しない |
| owner validity 喪失・generation 観測・successor 停止 | 利用不能を先に fence。確定 readback 後に分類し直す |

hook は effect より前の明示呼び出しとし、event bus の best-effort 通知で代用しない。DB writer の新しい bypass が増えた場合も、境界検査と試験が赤になる構成にする。

**未検証の実装リスク**：全 write callsite の網羅性は本メモだけでは証明していない。既存 runtime reservation はこの fence を持っていない（`recovery_control.rs:12-38,320-372`）。V-admission は callsite 一覧と対応 hook を PR 本文に示し、網羅できない経路があれば管理公開を fail-closed のまま止める。

外部 owner 変更は、この process の mutex では止められない。保持集合を DB から拡張せず、世代の逆戻りを認めないため、失った所有を理由に未観測の自己 graph が新規出現することはないという前提を使う。これは physical-owner の単調な generation と封印済み permit 集合に依存する**推論**である。保持 permit の出所は Q2 §1.2、実コードの診断入口は `crates/runtime/src/runtime/physical_owners.rs:229` 以下。喪失観測が遅れた場合は保守的に blocker を残し、DB 読み直しなしで「今も owner」と断定しない。

### 3.4 admission と lock 順序

固定順序は **public state → admission/collector guard → core coordinator → runtime の既存 memory lock**。Q2 側は collector → runtime graph reservation。下位 lock を保持して collector に callback する逆順を禁止する。host mutex／DB／owners registry／provider に collector lock 内から入らない。更新結果の公開は下位 guard を放した後に行う。

`admit` の概念手順：

```text
lock collector
  Ready、pending=0、core epoch、revision 一致を確認
  完全な未処分集合が空であることを確認
  restart reservation が未取得であることを確認
  transition を一度だけ呼ぶ                 // 同じ guard を保持
  成功した既存 RestartId を reservation に記録
unlock collector
```

transition の実行前に guard を落として再比較する実装は禁止する。closure の失敗時は既存 coordinator の結果を保持し、collector が独自に gate を巻き戻さない。

reservation は次の restart 状態機械ではなく、Q2 の新規 effect を admission と排他にする fence に限る。解放は既存 coordinator が該当 ID の reversible cancel／通常復帰を確定した後、core lock を放して行う。FailedClosed から勝手に再開しない。

| 競合の勝者 | 結果 |
|---|---|
| Q2 fence 登録が先 | stale revision／pending／非空により restart 拒否。gate 不変 |
| restart の atomic admission が先 | Q2 新規 effect は Conflict／Busy。照会と既存操作 readback は可 |
| Release の main 失効だけ完了 | cleanup 完了まで restart 拒否 |
| Resume 要求受理だけ完了 | activation 確定まで restart 拒否 |
| 同 ID replay | 共通 operation lookup が先。新しい admission・effect を起こさない |

### 3.5 closure の I/O 除去

V-admission に `public/implementation.rs` の小さい修正を含める。

- inspect で recipe と DB の read-only capture preflight を取得し、opaque な観測 proof を `Prepared` に保持する。
- commit の `admit` 内では、その proof と現在の runtime membership／incarnation／recipe 観測世代の **memory-only 再照合**と既存 `request_inner` のみを行う。
- host binding の再照合は、公開前に更新される共有の観測世代を読む方式とし、collector 内から host mutex を取り直さない。runtime metadata の変更も同じ観測を無効化する。具体的な getter と登録 hook は DG-3 の接続対象とする。
- I/O を含む `preflight_restart_capture` を commit から呼ばない。既存 `RestartPreflight` の分離方式を使い、必要な照合 helper を runtime 内に追加する。DB を読み直す `prepare_restart_quiesce` 全体を closure に移さない。
- 既存の preparing worker の DB 再検証、時計、capture 境界は残す（`crates/evorch-core/src/restart/preparing.rs:58-99`）。観測が変わったら stale として再 inspect する。lock 内で refresh しない。
- DG-3 の recipe metadata 検証もこの memory 観測に含める。DB-only fingerprint と live metadata の変更を取り逃がさない。

これは既存 API をそのまま呼べば完成するという主張ではない。proof の lifetime／必要な runtime getter の追加は提案であり、V-admission の実装対象に明記する。

### 3.6 Q2 診断との整合・再構築

- `OwnedGraphsQuery::Undisposed` は同じ collector の投影と revision を返す。preflight が見た revision と `admit` が照合する revision を一致させる。
- count は全 graph を数え、参照は最大64件・truncated を使う。thread超過の `over_limit_count` と警告の `incomplete_disposals` を保つ。参照切詰め後の長さで空を判定しない。
- pending 中も確定した未処分情報は保持する。ただし安全な snapshot を作れない場合は StorageUnavailable 等で照会を失敗させ、成功した `count=0` を捏造しない。UI は unavailable を「未処分なし」と表示しない。
- detail/list の DB 診断は引き続き診断であり、graph CAS とその revision を保つ。aggregate の観測 revision と graph/control revision を混ぜない。detail だけの `SnapshotClock` が残る場合も admission からは参照不能にする。
- cold 起動は Rebuilding → cold 分類。保存データ・receipt から live ownership や ticket を復元しない。
- planned successor は physical proof に基づく permit 保持と inventory の読み取り後に Ready にする。auto recovery が始まる前から hook を接続する。公開前の読み取りを途中で許可するなら unavailable を返す。
- 初期化の I/O 失敗は unavailable のまま。process 再起動で旧 revision／旧 operation の authority を復活させない。

## 4. 不変条件

1. 同じ lock 区間で revision・空集合を検査して一回だけ gate 遷移する。
2. collector lock／同期 commit 中の DB・filesystem・owner・provider I/O はゼロ。
3. 未確定な effect を成功・除外として扱わない。fence は Drop で楽観解除しない。
4. 診断と受付は同じ述語・同じ aggregate revision。診断を execution/disposal authority に変換しない。
5. release は resume しない。Q2 owner 行は Running のまま。他 owner／generation に触れない。
6. 第二の restart 時計、状態機械、migration、receipt authority を作らない。

## 5. 試験（負例と mutation）

同期は clock／barrier／channel／join のみ。sleep と retry 増量は禁止する。

| 試験 | assertion | 検出する mutation |
|---|---|---|
| 空・同 revision の正例 | closure 1回、既存 gate 1遷移 | closure の未実行／二重実行 |
| empty を読んだ後に Q2 fence | stale／pending 拒否、closure 0、tx 0 | guard を落としてから比較 |
| admission closure を barrier で止める | Q2 の新規予約が追越さない | 遷移前の unlock |
| 同 count・異 membership、0→1→0 | 旧 revision 拒否 | count のみ比較、revision 再利用 |
| Release 失効済み・scratch 未確定 | blocker 維持 | main 成功だけで除外 |
| Resume ACK 不明／late readback | 追加 activation 0、確定まで受付不可 | 要求受領時に除外、Unknown の Drop 解除 |
| aggregate と admission の照合 | 変更なしなら同 revision、変更後は Conflict | aggregate に旧 SnapshotClock を使用 |
| 64/65 thread、64/65 graph | count 正確、参照最大64、超過は blocker | 表示上限を集計上限に使用 |
| cold／foreign／revoked 後の世代変更 | cold は非 blocker、残処分は警告のみ | cold を加算、警告を blocker に混同 |
| commit の I/O spy | reader／writer／owner／filesystem 呼出し0 | closure の DB preflight 復活 |
| rebuild／poison／revision 枯渇 | unavailable、gate 不変 | エラーを空集合へ変換 |
| automatic と manual の予約競合 | 一方だけが effect、旧 token 不使用 | automatic の fence 接続漏れ |

既存 Q2 boundary／disposal／resume 試験も回帰させる。`scripts/check-q2-boundaries.sh` に collector を含む製品経路の bypass 検査を追加する。mutation は実装 PR で実施し、本メモでは未実施。

## 6. 影響するファイル・PR 対応

| PR | 主なファイル／責務 |
|---|---|
| V0-switch | `crates/workbench-host/src/bootstrap/mod.rs` の StorageConfig／StorageHandle accessor を merge 前に追加。未定義の collector 型は参照しない。composition 入力は既存型の取得口を使う |
| **V-admission** | `crates/workbench-host/src/bootstrap/recovery_disposal/{collector,admission}.rs`（新設）、同 `facade.rs`／`view.rs`、runtime の `recovery_control.rs`／`physical_owners.rs`／`recovery_disposal.rs` 等の同期 hook、`restart/public/implementation.rs` の I/O 分離、必要な runtime preflight helper、専用試験。**collector 型の定義と同じ PR で composition の根・入力と `bootstrap/mod.rs` の accessor／構築を追加し、自動 recovery 開始前に hook を接続する** |
| V1a-2 | `admin_composition/recovery/owned_graphs.rs` で共有 collector を facade に渡すだけ。collector 本体を編集しない |
| V1b-1 | finalization sender の型と中央入力・accessor を追加。collector の中央編集は V-admission の後に直列化する |
| V1b-2b | 同じ handle を coordinator の admission adapter に渡し、本番 compose 試験 |
| V-safe／V-ops-q2／T3 | 本番認可・監査と「拒否→一覧→処分／再開→通る」の統合確認 |

**順序の確定**：`OwnedGraphsFacade::new` の引数を collector 必須に変更するため、**V-admission → V1a-2** とする。空 collector の暫定 constructor で並列化しない。中央入力は [DG-2 §4.3](5a-v-dg2.md#43-中央入力と-accessor-の確定表m2) を正とし、**V0-switch → V-admission → V1b-1** の編集窓とする。親の決定（msg-1216／M2）により、従来の「V-admission は根・`bootstrap/mod.rs` を編集しない」という制約は collector 入力・accessor・構築・自動 recovery 前の hook 接続に限って置き換える。これに V1b-1／B2 (a) 待ちは加えない。

実装の hook 数が計画の約12ファイルを超える場合は、独立レビュー後に分割／編集窓を親へ戻す。網羅性を落として小さく見せない。本作業ではコード変更・テスト実行・commit・PR 作成は行っていない。
