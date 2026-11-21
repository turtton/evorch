# DG-3 — 本番 RecipeSource と再開対象の固定

## 0. 前提・範囲

設計提案。V1b-2a の実装前に DG-1〜3 を一括で独立レビューへ出す（V-06／J3）。本メモは承認・実行権限の根拠ではない。

正本：
- [承認済み計画](5a-v-plan.md) §3.1、§4、V1b-2a／V1b-2b、V-cache。
- 制約 `/var/tmp/evorch-output-1000/22/c80fc470-c898-4638-815a-e509155d3f30.txt` V-06／09／10／11／15／18。
- 初回レビュー `/var/tmp/evorch-output-1000/19/1be185d9-c04d-4337-b058-d04e2184f1f0.txt` M3。
- [Q2 設計](q2-design-final.md) §1、§5、§6（recipe は authority でない、Release と Resume は別）。
- 全体計画 `thread-9gkyvvvf`、`run-01m7tfrtj9kpxk7sxzpptzmt9c#331.3` DC-30／32〜35／39／40、cache 契約。

コードは main `794fa1ed79fec4a15ba805c86f54db9414df27e8`（`f794cd36` 以降）の行番号。パスは repository root 相対。型・API の追加案は未実装である。

## 1. 問題

`RecipeSource` は map を返す port だけがあり、本番の実装はない。recipe を間違って埋めると、restart は受理されたのに successor が再開できず、自己所有の未処分 graph を増やす。

- `crates/evorch-core/src/restart/public/implementation.rs:32-36,147-153`
- recipe の field／policy：
  `crates/runtime/src/quiesce/recovery.rs:6-31`
- root recipe の policy は全 member に適用され、欠落は Unsupported：
  `crates/runtime/src/runtime/recovery_graph.rs:255-270`
- current authority は CoreOwned と current settings を検査し、lane ledger と recipe の lane が不一致なら拒否：
  `crates/workbench-host/src/bootstrap/recovery_authority.rs:125-129,169-185`
- activation も lane／保存 descriptor／team を照合：
  `crates/runtime/src/runtime/recovery_activation/activate.rs:453-512`

したがって「root は全部 CoreOwned、他の field は全部 None」は実装案にならない。run の親がないこと、checkpoint があること、owner 行が自己であることだけでは core がその run の寿命を管理している証拠にならない。

## 2. 選択肢と比較

| 案 | 利点 | 問題・判断 |
|---|---|---|
| 全 root CoreOwned、lane/team/goal は None | 最少コード | CLI continuity昇格、情報欠落、successorでの不一致。明示的に不採用 |
| 全種類の metadata を集め、lane/team/goal も再開 | 利便性が高い | current lane binding、team claim reconciliation、goal coordinator の正当な再構築まで検証が必要。V1b-2a で未検証の対応を宣言しない |
| **plain core-owned conversation を許可し、lane/team/goal は gate 前拒否** | 新 authorityなし。対応範囲と負例を小さく固定できる | その run が残る間は公開 restartできない。利用者が通常操作で終える必要がある。**今回の推奨** |
| active run が一つでもあれば拒否 | 最も保守的 | 本番の安全な通常run再開を一切示せない。RecipeSourceを空で実装して終わらせる案は採らない |

拒否方針は V-09 が許す二択のうちの選択であり、「後で未処分から手動 Resume すればよい」という扱いではない。将来対応を拡張するときは、各種類の成功試験と current authority の照合を追加して独立レビューする。

## 3. 推奨案

### 3.1 情報源と core 所有の正の確認

本番 `HostRecipeSource` は、現在の host binding、runtime の capture 対象 metadata、storage の descriptor を read-only に照合して recipe を作る。stored recipe、receipt、UIの自己申告から所有方針を復元しない。

| 情報 | 正とする情報源 |
|---|---|
| live capture対象と親子関係 | runtime の現在の resume intent / incarnation。履歴一覧や active task 数だけから推測しない |
| core管理のroot | 現在の `CommandService.bindings.chat_runs` と、同じrootのruntimeのtrusted conversation設定、既に保持するchat permit。thread→rootが一意で一致することを確認 |
| goalの有無 | `bindings.goal_runs`／`goal_projects` と runtime の goal 関連付け。どれかが goal を示せば対象外 |
| lane | 現在のlane ledgerとruntime設定／保存descriptorのlane。どれかがlaneを示せば対象外 |
| team | 現在のruntime設定のteam／team_task／topologyとdescriptorのrenewable_team。どれかがteamを示せば対象外 |
| descriptor | storageの `run_context.config_json` を `RunRestoreDescriptor` としてdecode。current runtime metadataと矛盾すれば拒否 |
| settings selector | `CORE_RECOVERY_SETTINGS_REFERENCE`。パス・保存済みRunConfig・秘密ではない |

根拠：
- host bindings は commands が更新する：
  `crates/workbench-host/src/commands/mod.rs:42-67`
- current binding の公開snapshot：
  `crates/workbench-host/src/lane.rs:14-41`
- trusted conversation、team、ownership はRunConfigの別field：
  `crates/runtime/src/run.rs:195-212`
- descriptorはthread/lane/parent/team/project等を保持：
  `crates/runtime/src/restore.rs:177-220,228-237`

**推測・要実装検証**：`chat_runs` と runtime の trusted conversation 設定・保持 permit の一致を core 管理の正の識別に使えると考える。ただし既存 `BindingTable` はrootを返すだけで、core/CLI policyを型で保証していない（`lane.rs:16-41`）。したがって「bindingがあるから無条件CoreOwned」にはしない。V1b-2a で実際の登録経路を試験し、CLI invocation が同じ識別条件を満たす経路があれば拒否側へ倒す。新しい実行 authorityや永続的な所有schemaを追加して解決しない。

CLI が core の管理 API を呼ぶという transport と、`ResumePolicy::CliInvocation` が表す invocation-owned run は別物である。後者は、明示 continuity の proof を要する。今回の公開 restart 用 source はその proof を発行せず、CoreOwned に変更しない。

### 3.2 採否を先に固定する

graph は root と依存する全 member を一単位に検査する。root だけ見て child の特殊 metadata を落とさない。

| graph／run | 今回の方針 |
|---|---|
| 現hostが管理するplain conversation root、通常の安全なsnapshotあり | CoreOwned recipeを生成。既存planned再開へ |
| その通常の delegated child | graphのcore管理rootに従う。各memberのdescriptorと親子関係を保持して再開 |
| lane root／lane worker（core所有でも） | **gate前拒否** |
| team coordinator／team member／未reconcileのteam claim | **gate前拒否** |
| goal root／goalに属するrun | **gate前拒否** |
| CLI invocation-owned graph、continuity不在／存在 | **gate前拒否**。既存policyをCoreOwnedへ格上げしない |
| unbound root、所有方針不明、foreign permit、欠けたmember | **gate前拒否** |
| RAM-only queued launch、参照snapshot欠損／破損 | 既存preflightによる拒否。黙ってcapture対象から落とさない |
| active capture対象がゼロ | 正常に空のmap。情報源が読めないこととは区別 |

「通常の delegated child を含む正例」は必須とする。rootだけCoreOwnedにして、childのrecipeを作らない実装をこの試験で落とす。captureにroot fallbackがあっても、それを metadataの省略許可と解釈しない。

### 3.3 型と API の概略

既存 `RecipeSource::collect() -> AdminIo<Recipes>` は基本 port として維持する。source 内部で次の read-only DTO を使用する。

```rust
// 新設の観測用DTO。RunConfig／OwnerPermitを返すAPIではない。
struct RestartRecipeInventory {
    runtime_generation: u64,
    members: Vec<RestartRecipeMember>,
}
struct RestartRecipeMember {
    run: RunId,
    root: RunId,
    incarnation: u64,                // 実装時は既存の型に合わせる
    // liveなthread/parent/lane/team/goal/conversation分類の値
    // queuedやsnapshot参照の有無。prompt/tool本文は含めない
}
struct HostRecipeSource {
    // runtime、現在bindingの読み取り口、storage reader
}
```

必要な read-only getter は「実行可能な設定を返す」APIにしない。runtime の内部memoryを読み、authority-free な分類と観測世代だけを返す。snapshotのprovider inputはrecipeにコピーしない。

host binding の観測世代は、変更を公開する前に更新する共有の memory-only counter とする。commit が host mutex を再取得せずに照合できることが条件である。counterだけ先に読んで、その後別の世代のbindingを混ぜないよう、sourceのコピー前後でも一致を検査する。既存 `BindingTable` にそのcounterはないため（`crates/workbench-host/src/lane.rs:25-41`）、この小さい追加も必要な接続である。

処理：

1. runtimeのcapture対象inventoryとhost bindingの世代を観測する。host mutexの下ではbindingをコピーするだけで、DBを開かない。
2. bounded reader／blocking workerでdescriptor、lane ledger等を読む。構成する全memberを確認する。
3. §3.2の対象外を一件でも検出したら、mapの部分成功ではなく要求全体を拒否する。diagnosticはboundedな種類／opaque run referenceだけ。path／SQL／promptを返さない。
4. runtimeとbindingをmemoryで再照合する。読み取り中に変更があればConflict。成功したmapを「現在のrunと照合済み」という観測と結び付ける。
5. DG-1のread-only preflight proofに、recipe digestとruntime／binding観測世代を結合する。commitではI/Oなしに再検査。`Recipes` のmap自体をexecution authorityにしない。
6. preparing workerがcapture前の既存preflightを繰り返す。captureもdescriptor／memberを既存のexpected inventoryと照合する。古い観測から別runを同じrecipeで実行しない。

**API上の接続点**：現在のmap返却portだけにはbinding世代を運ぶfieldがない（`public/implementation.rs:32-36,61-65`）。実装では、DG-1が追加する opaque preflight observation と `Prepared` の内部fieldに結合する小さい補助portを定義する必要がある。推奨は `collect_observed` 相当の内部結果（map＋non-authority observation）を運び、既存 `collect` の意味を保つ形。全実装に安全な観測が必須で、未接続時に「常に一致」のdefaultを置かない。public/R leaf側のこの差分は V-admission担当と直列に調整する。

これは既存APIだけで整合が取れているとの主張ではなく、**未実装の追加設計箇所**である。将来、観測を運ぶためport全体の署名を変えるなら、fake実装・専用試験も同じ編集窓で更新する。V1b-2aがcompositionの根を並行編集して補わない。

### 3.4 recipe field の規則

| field | 値・拒否規則 |
|---|---|
| `settings_reference` | 常にcurrent設定を指す既存定数。古い設定内容は保存しない |
| `policy` | 正のcore管理確認に成功したrootのみCoreOwned。通常childもそのrootの同じpolicy。未知はUnsupported相当で要求拒否 |
| `goal_id` | 今回受理するgraphはgoal無しと確認済みなのでNone。goalが見えたらNoneに置換せず拒否 |
| `team` | 同上。current/savedのどちらかにteamがあれば拒否 |
| `lane` | 同上。ledger/current/savedのどれかにlaneがあれば拒否 |
| `descriptor` | decodeした対象memberの正確なdescriptorを保持。readエラーをNoneにしない。snapshot未作成のdurable queuedは既存preflightの許可条件を満たす場合だけ |

capture が実際のsnapshot descriptorを再読し上書きする既存処理を残す（`crates/runtime/src/runtime/recovery_graph.rs:271-291`）。sourceの先読みがcapture時の検証の代わりにならない。

### 3.5 「gate前拒否」と競合の意味

- 要求時に対象外runが存在すれば、inspectで拒否し、restart gate・tx・park・provider呼出しを発生させない。
- inspect→commitで対象外run／bindingが変わったらDG-1のmemory proofがstaleになり、遷移closureより前に拒否する。
- commit後、既存PreparingDrainsの間に新規runが入り得る点も試験対象にする。既存のDB preflightはhost gateのPreparingより前に行われる（`crates/evorch-core/src/restart/preparing.rs:58-99`）。ここでsourceの分類条件も再照合し、対象外検出をreversibleな拒否/cancelへ返して、対象外graphをparkしてから未処分にする挙動を許さない。
- 現行 preparing worker の一般エラーはFailedClosedへ遷移する（同 `116-125`）。したがって「sourceだけを実装すれば、競合時もgate不変」とは言えない。対象外／staleを既存cancelへ返すtyped区別と、最終memory照合をhost gate遷移と同じcoordinator→runtimeの既存排他内で行う接続が必要。新しいrunを黙って対象から外す対応は禁止する。
- 特にmemory比較と新規起動の間にも隙間を残さない。既存の `manual_launch_gate` による登録排他（`crates/runtime/src/runtime/restart_transaction.rs:45-57`）とquiesceの排他を使って、inventory照合とplanned admission閉鎖を同じmemory操作にする。現在のhelperの呼出しを並べただけで原子的だとは仮定しない。この組合せのlock整合性は未検証なので、登録と最終照合をbarrierで競合させる試験を必須にする。
- gateを閉じた後の新規外部起動は既存planned admission規則に従う。既存の自動内部起動による取消規則を、core所有への昇格で迂回しない。

この補強は既存state machine／時計を複製するものではない。必要なpublic/preparing leaf変更は **V-admissionで定義する観測proof、V1b-2bでの最終compose** の編集窓に載せる。計画のファイル境界に収まらなければ、実装前に親へscope調整を返す。V1b-2aだけで競合安全を達成済みとしない。

## 4. 不変条件

1. recipeは証拠であり、現在のbinding・execution authority・planned gateを代替しない。
2. CLI／unknownをCoreOwnedへ昇格しない。sourceのためのowner start／claim、binding生成、continuity issuerを作らない。
3. 依存graphの全memberを採否検査する。欠落・破損を無視して小さいmapで通さない。
4. lane/team/goalは、今回すべて公開restartのgate前拒否。Noneで隠してsuccessorでBlockedにしない。
5. 通常memberのdescriptor・parent・projectを維持する。provider history／ToolResults／affinity／model/settingsをrecipe構築で書き換えない。
6. descriptorとmapを返しただけではResumeしない。release／照会／再接続／ACK再送による暗黙activationはゼロ。
7. targetはcurrentかローカル設定候補IDのみ。`LocalTargets`構築は別責務で、recipeからpath/argv/envを受け渡さない。

## 5. 試験（負例と mutation）

`public_restart_compose::recipes` に採否表をfixture化する。privateな任意authority constructorや製品mainのtest hookは使わない。

| fixture | 固定する結果／観測 | 検出する mutation |
|---|---|---|
| plain core会話root | field照合、既存planned pathで再開。次provider要求まで確認 | collectが常に空／Unsupported |
| core root＋通常delegated child | 全memberのdescriptor/parent維持、全体の既存authority照合 | rootだけmap、child情報をNone化 |
| lane root／worker、ledgerだけlaneの例 | inspectで拒否。gate/tx/park/provider=0 | laneをNone固定、rootだけ検査 |
| team coordinator／member／claimあり | 同上。reconcileしていないteamを通さない | teamをNone固定 |
| goal root／関連run、bindingだけgoalの例 | 同上 | goal_id=None固定、goal binding無視 |
| CLI invocation（proof有/無） | 今回どちらもgate前拒否。CoreOwned生成0 | 全rootをCoreOwned |
| unbound／foreign／unknown root | gate前拒否、owner claim/start=0 | 「所有不明ならcore」をdefault |
| descriptor破損・欠落・参照不一致 | 既存許可queued例以外は拒否 | decode errorをNoneに変換 |
| no-active-run と storage unavailable | 前者は空で成功、後者は拒否 | read failureを空mapに変換 |
| inspect後lane/team/goalを付与 | memory proof stale、gate/tx不変 | 観測世代未照合 |
| PreparingDrains中に対象外runを追加 | final preflightでcancel、対象外park0、tx0 | source再検査省略、一般errorでgate FailedClosed |
| あるchildだけ対象外 | graph全体拒否、通常memberの部分capture0 | member filterで対象外を削除 |

成功を「recipesが作れた」だけで判定しない。通常root＋childの正例は、既存CoreGraphRecovery→current authority→typed activation予約→runtime activationを通すcompose試験が必要。execを要する証拠はV-exec、製品同士はT2/T3／F4-Eの担当であり、composeだけで製品完成とは呼ばない。

cacheはV-cacheで入力由来mock＋独立wire比較を使う。公開restartと明示Resumeの後に履歴・ToolResults・system・model/settings・affinityを維持し、異build/schema変更時だけ既存の一要求tools-only境界、その次turnのprefix再利用を確認する。固定cached-token値だけでは合格にしない。

## 6. 影響するファイル・PR 対応

| PR | 影響 |
|---|---|
| V0-switch | 既存型の StorageConfig／StorageHandle accessor を merge 前に追加。runtime／owner／reader／diagnostics は既存の参照口を再利用。collector／sender の未定義型は先行参照せず、製品入口も触らない |
| V-admission | DG-1のopaque preflight observation、`restart/public/implementation.rs`のPrepared、必要なruntime memory再照合の追加。RecipeSourceとの接続を定義。collector 型と同じ PR で中央入力・bootstrap accessor・構築と自動 recovery 開始前の hook 接続を追加 |
| **V1b-2a** | `crates/evorch-core/src/admin_composition/restart/{recipes,targets}.rs`。HostRecipeSource／LocalTargets構築、`public_restart_compose::{recipes,targets}`。必要なread-only metadata getterはruntime/host担当と編集窓を合意 |
| V1b-1 | finalization sender の型・中央入力・accessor を同 PR で追加。collector の中央編集は V-admission 後に直列化。B2 (a) 後 |
| **V1b-2b** | 正しいsourceをcoordinatorへinstallし、既存preparing leafの分類再照合／typed拒否を接続。通常root＋child再開と対象外拒否のcompose検証 |
| V-exec／V-cache | 実execとprovider/cacheに関する証拠を追加 |
| T2／T3 | 製品artifactで公開経路を検証。T2以外がF4-P fixtureを拡張しない |

V1b-2aはDG-3独立レビューとV0-regが前提。中央ファイルを触らない範囲の実装は並行可能だが、観測proofの最終接続はV-admission後に直列化する。中央入力と起動順は [DG-2 §4.3](5a-v-dg2.md#43-中央入力と-accessor-の確定表m2) を正とし、V0-switch → V-admission → V1b-1 の編集窓を守る（親の決定 msg-1216／M2）。V1a-2 の collector 配線を V1b-1 待ちにしない。将来lane/team/goalを再開するPRは本計画に暗黙追加しない。

本メモは読み取りと設計のみ。source識別の登録経路網羅、borrow/type整合、全試験・mutation・cache E2Eは未検証であり、実装PRの完了証拠が必要である。
