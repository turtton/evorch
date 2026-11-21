# DG-2 — host の排他的所有と finalization の本番構築

## 0. 前提・範囲

設計提案。DG-1〜3 を一括して独立レビューへ出し、通過後に V1b-1／V1b-2b を実装する（V-06、計画 §3.1／J3）。本メモ自体は受け入れ判定ではない。

**改訂（M1・M2）**：独立レビュー `/var/tmp/evorch-output-1000/45/8545592e-6569-42d2-8a70-8a49fadfd96b.txt` の must 2件と、親の決定（msg-1216）を反映した。DG-2 は修正要求の状態であり、この差分の再レビュー前に V1b-1／V1b-2b を始めない。中央入力の確定表は §4.3 を正とする。

正本：
- [承認済み計画](5a-v-plan.md) §3.1、§4、V1b-1／V1b-2b、§7。
- 制約 `/var/tmp/evorch-output-1000/22/c80fc470-c898-4638-815a-e509155d3f30.txt` V-06／08／10／11／12／18／19、巻き戻し follow-up。
- 初回レビュー `/var/tmp/evorch-output-1000/19/1be185d9-c04d-4337-b058-d04e2184f1f0.txt` M3。
- 全体計画 `thread-9gkyvvvf`、`run-01m7tfrtj9kpxk7sxzpptzmt9c#331.3` DC-20／30〜33／38〜40。
- [Q2 設計](q2-design-final.md)：次 restart は未処分が空の場合だけ。Resume／Release と finalization を混同しない。

コード根拠は main `794fa1ed79fec4a15ba805c86f54db9414df27e8`（`f794cd36` 以降）。パスは repository root 相対。以下の追加型・API は概略であり未実装。

## 1. 問題

- `start_finalization` は `&mut BootstrappedHost` を取り、capture 後に `take_planned_exit` で bridge 等を移す。`Arc<Mutex<WorkbenchHost>>` だけを持つ adapter では呼べない。
  - `crates/evorch-core/src/restart/finalization.rs:21-51,104-127`
- 製品は cold／successor とも `run_with_web(&host, ...)` しか渡していない。
  - `crates/evorch-core/src/main.rs:188-194`
  - `crates/evorch-core/src/planned_entry.rs:76-84`
- `FinalizationEnvironment` には instance、entry の signal mask、同一 signal reader、control drain owner、successor argv が要る。tx 作成だけでは接続は完成しない。
  - `crates/evorch-core/src/restart/finalization/later.rs:108-130`
- lifecycle loop に DB 読み書きや finalizer の待機を持ち込むと、signal 処理・control drain が止まり、finalizer が loop へ drain を依頼して deadlock する。
  - `crates/evorch-core/src/lifecycle.rs:194-234`
  - `crates/evorch-core/src/lifecycle/control.rs:16-27`
- 管理 continuation が finalization 完了を待つこともできない。管理 task 自身が stage 4 の `area.shutdown()` の join 対象なので、待つと依存が循環し、1秒の join 期限で FailedClosed になる。start 受理と finalization 完了は分離する。
  - `crates/workbench-host/src/admin/service.rs:475-484`
  - `crates/workbench-host/src/bootstrap/planned_exit.rs:255-302`
  - `crates/workbench-host/src/areas/tasks.rs:278-288`
  - `crates/evorch-core/src/restart/finalization.rs:294-311`

## 2. 選択肢と比較

| 案 | 比較 |
|---|---|
| `BootstrappedHost` 全体を `Arc<Mutex<_>>` にする | wide lock／await 越し所有／shutdown の順序が変わる。19 constructor callsite まで波及しやすい。不採用 |
| finalization adapter に host を移して task で実行 | lifecycle と adapter が host の実行主体を競合する。V-08 に反する。不採用 |
| 既存 finalization を共有参照だけで動くよう改造 | `take_planned_exit` の排他的転送を崩す。capture 境界の別実装を誘発。不採用 |
| **製品専用 mutable loop＋prepared command mailbox** | host の排他所有と既存の非同期 finalizer を維持。constructor は不変。準備の I/O を loop 外へ分けられる。**推奨** |

## 3. 推奨案：一回限りの prepared command

### 3.1 host を所有する側にだけ実行入口を置く

`LifecycleDriver::new(&host)` は変更しない。既存の `run`／`run_with_web`（共有 host を使う既存試験・利用者）も残す。

製品用に `run_owned_with_web(&mut host, ..., finalization_owner, ...)` を追加する。host 自体の所有者は main／planned entry の stack のままで、loop が稼働中だけ排他的に借りる。名前の owned はこの排他借用を意味する。

内部の loop 処理は共用し、以下のような private enum で共有／排他を分ける。`DerefMut` の偽装、unsafe cast、共有 host のときの panic fallback は作らない。

```rust
// 新設概念型。既存の public constructor は不変。
enum LoopHost<'a> {
    Shared(&'a BootstrappedHost),
    Owned(&'a mut BootstrappedHost),
}
// Shared variant には finalization receiver を結び付けない。
// Owned variant のコマンド分岐だけが &mut host を取り出せる。
```

loop は一度に一つの request を取り出し、同期の `start_finalization` を呼んだら、すぐに select に戻る。DB 完了も `wait_finalization` もここで await しない。既存 finalizer task が `ControlDrainHandle` を使って同じ loop へ依頼できる状態を保つ。

**推測・未検証**：この enum で既存 `run_with_clock` の借用期間を短く区切れると考える。根拠は loop が host 参照を event bus／notify に使い、mut host が必要なのは同期転送だけであること（`lifecycle.rs:157-178`、`finalization.rs:104-127`）。実際の Rust borrow check は V1b-1 で確認する。constructor 署名変更が必要になったら、その時点で計画どおり PR を分け、19 callsite を黙って変更しない。

### 3.2 依頼と実行の分離

```rust
// 新設。製品 loop に結び付く receiver は一つだけ、Clone 不可。
struct FinalizationInbox { /* bounded receiver, per-restart delivery latch */ }
struct PreparedFinalization {
    id: RestartId,
    tx: RestartTransaction,          // Preparing の readback 済み
    current: Arc<CurrentElf>,
    checked: CaptureCheckedTarget,   // #710 の capture 前再 probe 済み
    reply: oneshot::Sender<StartReceipt>,
}
// environment は request ごとに一度だけ組み立てて move する。
// context は loop に残り、別の新規 RestartId にも同じ instance 等を供給できる。
struct ProductExitContext { /* instance, mask, signals, control, argv */ }
```

`FinalizationOwner` の実装は host を保持しない。既存 trait の役割を次の順で実現する。

1. 共通 continuation で、該当 `RestartId` の finalization slot を一回だけ占有する。二重呼び出しは既存結果へ合流／Conflict とし、新しい writer job を作らない。
2. 検査済み target と trusted product input から metadata を固定し、既存 writer の `RestartMutation::Create` を **一度だけ**送る。blocking な writer/readback は `spawn_blocking` または既存の bounded reader 上で行う。loop 上では行わない。
3. ACK を失ったら同じ restart ID の readback だけを行う。既存 tx があれば metadata 全体と phase=Preparing を照合する。異なる内容・別 phase・不在が確定した場合は失敗。再 Create しない。
4. create/readback が確定した場合だけ、move-only の `PreparedFinalization` を bounded mailbox へ一回送る。受理上限は一つの進行 restart に対して一つ。slot は reply 受領より前に consumed とする。
5. **host-owning loop だけが**その ID 用の一回限りの environment を ProductExitContext から組み、`FinalizationTarget::new(...).with_environment(...)` とともに move して、既存 `start_finalization(id, &mut host, tx, target)` を呼ぶ。ここでの Arc／signal handle の clone は同じ instance／reader の共有であり、再取得ではない。
6. loop は同期 start が受理された直後に `StartReceipt::Accepted` を返す。continuation はそれを受けたら **直ちに `Ok(AdminExecutionOutcome::AwaitRestartRecovery)` を返して終了する**。`wait_finalization` や後続 stage の完了は待たない。この値は「exec 成功」ではなく、既存 AdminService が管理 producer を解放し、durable な結果確定を restart recovery に委ねるための返り値である（`admin/service.rs:479-483`）。新しい成功タイマーも、area task 内の完了監視 task も作らない。

既存入口は Preparing tx を前提とする（`finalization.rs:17-28,59-66`）。Create の既存利用例は `crates/evorch-core/src/restart/tests/finalization.rs:30-40`。後続 task の所有は `finalization.rs:112-127` をそのまま使う。

#### 配送待ち・返信待ちの所有と全終了経路での解放（M1）

receiver は `LifecycleDriver` に残したままにせず、owned loop の呼び出しスコープに move した guard が一つだけ所有する。共通の終了処理は **receiver を閉じ、未処理 command を拒否して reply sender を drop し、receiver 自体も drop する**。`close()` だけでは buffer 内の reply sender が残るため不十分である。

- bounded mailbox の空き待ち（`send`／予約待ち）は receiver の drop で必ず Err にする。
- buffer 内の未処理 command は `OwnerStopping` の拒否返信を試み、成否にかかわらず command と reply sender を drop する。
- 取り出した command の reply sender は同期分岐の guard に保持し、受理／拒否を直ちに送る。早期 return や unwind でも sender を drop する。別の長寿命構造体に移して返信待ちを残さない。
- reply の切断は continuation を直ちに解放し、共通の Unknown として返す。start の再配送はせず、この管理 task の中で finalization 完了や DB readback を待って判定を引き延ばさない。必要な後続照会は既存の readback 契約に従い、新しい effect は起こさない。
- DB 準備中だった continuation も、後から閉じた mailbox へ配送を待ち続けない。DB 準備とその確定処理は既存 writer の責務であり、loop の終了処理がそれを join／await する設計にはしない。

通常の loop 退出では共通 tail の最初に上記解放を行い、その後の `transition_to_shutdown` → listener/backend/client/web の停止 → 呼出元の `host.shutdown()` という既存の相対順序を保つ。tail を通らない早期エラー／unwind でも同じ guard の Drop が receiver と未返信 command を破棄する。Drop は memory-only で、DB I/O や非同期待機をしない。

対象は signal による退出（`lifecycle.rs:195`）だけでなく、**accept 失敗（`:264`）、control drain 中の signal（`:233`）を含む全経路**である。driver が呼出元に残ることや、通常 `break Ok(())` 以外の経路でも sender／reply を保持し続けないことを試験する。終了時に既に capture 済みなら、その finalizer 自体は abort せず、従来の planned cleanup に任せる。

### 3.3 二重実行・失敗・不明

配送 latch は transport の ACK ではなく「この restart の start を試行したか」を記録する。新しい restart 状態機械ではない。初期化し直して同じ ID を retry できる API は公開しない。

| 場所 | 故障時の扱い |
|---|---|
| tx Create 前 | slot を消費したまま失敗。既存 capture 前 cancel のみ。追加 Create なし |
| tx Create ACK 不明 | 同 ID readback のみ。確定できなければ Unknown。start しない |
| mailbox の send 失敗 | owner unavailable として失敗／不明を記録。別 loop・別 task へ迂回しない |
| send 後に reply 消失 | continuation は直ちに Unknown で終了。配送を再送せず、start が無かったと推測しない。後続照会は既存 readback だけ |
| start の戻り値が Err | 同じ ID の start を再呼び出ししない。既存 coordinator の phase／cleanup に従う |
| capture 後の失敗 | 既存 FailedClosed／shutdown cleanup。旧業務へ戻さない |
| 別 ID の遅延 request | loop の現在 ID／phase と不一致なら拒否。target を差替えない |

再実行禁止は **同じ restart ID・同じ finalization effect** に対するもの。capture 前 cancel が確定した後に、利用者が改めて確認した新規 restart を要求できる既存仕様までは禁止しない。その際も旧 ID の tx／mail を再利用しない。

### 3.4 signal との競合

最終的な線形化点は、既存 `capture_boundary` の coordinator mutex 内にある。`start_finalization` の手前で独自の capture flag を作らない。
根拠：`crates/evorch-core/src/restart/finalization.rs:54-102`。

| 観測順序 | 設計上の結果 |
|---|---|
| signal が観測済み、request 未処理 | shutdown を先に latch。mailbox を閉じ、未処理 request と reply sender を破棄。capture 0 |
| signal と mailbox がともに ready | signal を先に poll する優先順。shutdown 勝ち、start 0 |
| request を取り出したが current ID／Sealed／期限が不一致 | start の既存検査で拒否。再配送しない |
| start の同期呼び出しが capture を確定、その直後 signal | capture 勝ち。resource 転送は中断せず、既存 shutdown／watchdog／planned cleanup が担当 |
| DB 準備中に signal | loop は DB を待たず終了へ進む。準備 task は mailbox closed を観測して start しない。durable tx の結果不明は readback だけ |
| control drain 中に signal | 現行どおり drain へ Err を返し shutdown（`lifecycle.rs:218-233`） |
| accept 失敗／その他の loop 退出 | 共通 guard が receiver と未返信 command を破棄し、配送・返信の待機を解放してから host shutdown へ進む |

signal 優先は新しい timeout を意味しない。mailbox 分岐以外の idle/client/drain の意味を変えない。未観測の OS signal が同期 start の間に届く場合まで「必ず capture 前に勝つ」とは主張しない。そこは既存 capture と watchdog の順序契約を使う。

## 4. 本番入力の構築

### 4.1 `FinalizationEnvironment`

| 入力 | 本番での取得・寿命 |
|---|---|
| `Arc<Instance>` | cold の `Instance::acquire_default` の結果を直ちに Arc 化。successor でも confirm-ready 後の同じ instance を Arc として保持。第二の Instance／lifetime lock を取得しない |
| `Arc<BlockedShutdownSignals>` | `main` の thread 起動前 `at_entry()` の同じ mask を Arc 化し、cold と planned の両入口へ運ぶ。successor の inherited-FD 検査より前に別 inbox を開かない |
| `OperatorSignals` | bootstrap watchdog から譲渡された同じ inbox で `from_inbox` を一度だけ呼ぶ。その clone を loop と environment に共有。新しい signalfd を作らない |
| `ControlDrainHandle` | 同じ `LifecycleDriver::control_drain()` の clone。listener／接続 task を所有する loop へ戻るものだけ |
| argv | 起動時に保存した typed product options（正規 config-dir、web override 等）から内部構築。旧 `--launch-fd`、旧 successor bootstrap 引数、旧一時 FD をコピーしない。継承用の新 FD は既存 finalizer が追加する |

根拠：
- entry mask：`crates/evorch-core/src/main.rs:38,94-103`
- cold instance と signal：同 `119-143`
- successor inbox 転送：`crates/evorch-core/src/planned_entry.rs:23-29,64-70`
- clone は同じ Reader：`crates/evorch-core/src/lifecycle/signals.rs:13-39`
- environment constructor は値の保持だけ：`restart/finalization/later.rs:108-130`

wire／UI／CLI は target ID だけを選ぶ。argv・path・env を public DTO から取り込まない。診断・監査にも path を出さない。entry の生 `std::env::args_os()` 全体を再利用しない。

**未検証**：B2 (a) の Installed binding の型・追加の起動オプションは、調査基準 main の後で変わり得る。V1b-1 は B2 (a) merge 後に同じ instance/launch owner を渡す形へ rebase し、別の lock owner を導入しない。argv の完全な allowlist はその時点の `args.rs` と突き合わせる。

### 4.2 `RestartMetadata`

`crates/storage/src/recovery/restart.rs:12-29` の全 field を次のように固定する。試験の `"old"`／`"new"`、schema `62` 等を製品へコピーしない。

| field | 情報源／規則 |
|---|---|
| `restart_id` | 共通監査に結合した canonical `RestartId` の public ID。再送で変更しない |
| `capture_id` | slot の初回準備で一度だけ生成する bounded opaque ID。readback で作り直さない |
| `config_identity` | 保持している Instance の canonical identity。path ではない |
| `old_epoch` | 同じ core の hello／host epoch。wire の自己申告値を使わない |
| `successor_epoch` | この操作用に一回固定する old より大きい値。推奨は `checked_add(1)`。overflow は拒否。物理 handoff record にも同値を渡す |
| `old_build` | 起動時記録した実行中 ELF の build identity。revalidate は loop 外 |
| `successor_build` | `CaptureCheckedTarget::request().description().build_identity` |
| `target_identity` | 検査済み target の `request().id()`。path を ID にしない |
| handoff／snapshot version | running build と target が互換検査済みの製品定数／description。固定テスト値を使わない |
| `storage_version` | 実際に開いた storage の schema と target の schema range の一致を検査した値 |
| `old_builtin_tool_names` | `runtime.restart_builtin_tool_names()`。標準8 tool や role-filtered 一覧で代用しない |
| compiled/effective fingerprint 4欄 | 既存 handoff/catalog capture の検証済み値がある場合だけ。集約できない全 run の effective 値を捏造しない。`Option` が None でも既存 per-run evidence の検証を省略しない |

target の照合入口：`crates/core-launcher/src/handoff_targets.rs:93-125`。description の意味：`crates/core-launcher/src/handoff.rs:36-66`。full builtin inventory：`crates/runtime/src/runtime/restart_handoff.rs:436-456`。

**推測としての選択**：successor epoch の `old+1` は本メモの推奨 allocator であり、既存の公開 allocator を確認したという意味ではない。物理 record の条件は `successor_epoch > predecessor_epoch`（`crates/core-instance/src/handoff.rs:39-47`）。既存の確定 allocator が B2 後に提供されるならそれを使い、別 allocator は作らない。

`FinalizationTarget` による metadata と target の照合（`restart/finalization/later.rs:35-49`）は残す。新 adapter は catalog 比較・capture・exec を再実装しない。

### 4.3 中央入力と accessor の確定表（M2）

**親の決定（msg-1216）**：既存型の accessor は V0-switch の merge 前に追加する。新型を V0-switch に先行参照させない。collector handle は V-admission、finalization sender は V1b-1 が、型の定義と同じ PR で中央へ追加する。従来の「中央への後続追記は V1b-1 だけ」「V-admission は中央を触らない」という記述を、この範囲に限って置き換える。

| 項目 | 型を定義する PR | 中央ファイルへ入れる PR | 起動順での位置 |
|---|---|---|---|
| storage 設定の read-only accessor（`StorageConfig`） | 既存 storage の型を使用。新型なし | **V0-switch、merge 前**。`workbench-host/src/bootstrap/mod.rs` に製品用 accessor、composition の `Inputs` はそこから取得 | host の DB 構築後、Q1／Q2 facade と finalization owner の構築前 |
| storage writer の clone accessor（`StorageHandle`） | 既存 storage の型を使用。新型なし | **V0-switch、merge 前**。同 `bootstrap/mod.rs` と composition 入力。試験専用 `composition.storage_writer` には依存しない | writer 初期化後、facade／owner の構築前。getter は I/O なし |
| runtime／owner／control host、reader／diagnostics の既存参照 | 既存の型を使用。新型なし | 新しい中央 accessor は作らず既存 API を使用。V0-switch の `Inputs { host, backend }` から取得 | host 構築後。reader は既存 AdminService、diagnostics は初期 recovery の構築後に使用 |
| **collector の handle とその accessor／composition 入力** | **V-admission** | **V-admission** が `bootstrap/mod.rs`、composition の根・入力へ追加。collector 構築と runtime hook の接続も同 PR | **bootstrap 内で、初回の自動 recovery が始まる前**。planned は `activate_after_ready` が `start_deferred` を呼ぶ前。cold も recovery の開始前。Rebuilding 中も hook を接続済みにし、完了まで空を返さない |
| **finalization sender と accessor／composition 入力** | **V1b-1** | **V1b-1** が `lifecycle.rs`／`lifecycle/control.rs` と composition の根・入力へ追加 | `LifecycleDriver::new` 後、両製品入口の install 前。receiver の所有は owned loop に渡す。sender の取得だけで start しない |
| `ProductExitContext`（instance／mask／signals／control／typed argv の受渡し） | **V1b-1** | **V1b-1** が両入口と必要な composition 入力に追加 | entry mask は thread 起動前。instance／signals／driver の確定後に同じ所有物を束ね、loop へ渡す |
| `FinalizationEnvironment` の本番構築 | 既存 restart の型を使用。新型なし | 新しい中央 field は追加しない。**V1b-2b** の finalization leaf が V1b-1 の context と sender 契約を実装し、既定の install 本体に登録 | 検査済み command を loop が受理するとき、一回だけ構築・move |
| `RestartMetadata` の本番構築 | 既存 storage の型を使用。新型なし | 新しい中央 field は追加しない。**V1b-2b** が §4.2 と既存 accessor を使う | capture 前の continuation 上で一回固定し、DB 作成・readback 後に command を配送 |

自動 recovery の開始点の根拠は `crates/workbench-host/src/bootstrap/mod.rs:766-780` と `crates/evorch-core/src/planned_entry.rs:70`。collector を main／planned entry の後段 install で初めて接続する案は採らない。**V-admission が bootstrap 内の構築・hook 接続まで担当する**ため、V1a-2 が必要とする collector 配線は B2 (a)／V1b-1 待ちにならない。

同じ中央ファイルの編集窓は **V0-switch → V-admission → V1b-1** と直列にする。V1b-1 自体は B2 (a) merge 後。V1a-1 は V0-switch の既存型 accessor を使い、V1a-2 はさらに V-admission を待つ。V1b-2a の leaf は並行可能だが、観測 proof の最終接続は V-admission 後。V1b-2b の install 本体はこれらの入力が揃ってから実装する。これ以外の中央 field 追加は担当 PR を親へ確認し、並列 PR が独自に追記しない。

## 5. shutdown と公開方針の不変条件

1. finalization を始める製品 callsite は host-owning loop だけ。mailbox sender は `&mut host` を得られない。
2. loop は DB read/write、ELF hash/probe、metadata の DB 解決をしない。start は既存の同期 resource transfer だけ。DB 準備 worker の待機も loop に持ち込まない。
3. listener/backend/client/web を止めた後に呼出元が `host.shutdown().await`。その後 backend → host → instance の drop 順序を保つ。
4. `host.shutdown()` 内の **planned cleanup 待ち → owner.release_ready → scratch cleanup停止 → bridge終了**を変更しない。根拠：`crates/workbench-host/src/bootstrap/mod.rs:967-986`、`crates/evorch-core/src/restart/README.md:232-239`。
5. lifecycle の終端で `transition_to_shutdown` を呼ぶ既存順序も維持する（`lifecycle.rs:268-276`）。capture 後の task を配送側が abort しない。
6. 300秒、120秒、30秒、probe 5秒、lease 5秒＋grace 10秒、stage-5 10秒を変更しない。clock／capture／catalog の第二実装は禁止。
7. 非 Linux は既存 Unsupported 実装を使い、gate／tx より前に拒否する。
8. 管理操作 off、未認証 Web／Agent／System の拒否は AdminService の前段で完結し、mailbox・tx に到達させない。
9. start 受理後、管理 continuation は `AwaitRestartRecovery` を直ちに返す。loop の全終了経路で配送待ち・返信待ちを解放し、管理 task の待機が stage 4 や `host.shutdown()` の join を妨げない。

**公開済み build → off build の planned restart（follow-up への推奨回答）**：target を勝手に on build に差替えず、互換な off build への明示要求は既存 target 検査に従う。successor の管理 API／広告は off のままで、残る未処分 graph の Release／Resume はその build では利用できないことを target 確認時の運用説明に明記する。通常の自動 recovery の権限は緩めない。手動回復が必要なら運用者が on build へ切替える。off を回避する runtime kill switch／隠し enable API は作らない。

この説明は build identity に管理スイッチの属性を新設する提案ではない。候補 build の属性を機械判別できる根拠は未確認なので、自動で安全な候補だけに絞れるとは主張しない（`HandoffDescription` の現 field は `core-launcher/src/handoff.rs:36-47`）。

## 6. 試験（負例と mutation）

| 試験 | assertion | 検出する mutation |
|---|---|---|
| Shared loop の型／配線 | finalization receiver を渡せない。sender に host 不在 | unsafe shared→mut／第二 host owner |
| cold／successor の owned loop | 同じ coordinator／instance／control owner が渡る | 別 coordinator／別 Instance の構築 |
| 二重依頼と不明 ACK | Create最大1、start最大1。同 ID再送はreadbackだけ | latch を ACK 後に立てる、再送で latch reset |
| DB worker を barrier で停止 | loop は signal／client drain を処理可能 | loop 内で writer／reader 待機 |
| signal と mail 同時 ready | signal優先、capture0 | select 順序逆転／shutdown後 start |
| capture直後 signal | resource transfer完遂、planned cleanup待機 | capture後 task abort／owner先行解放 |
| Preparingでないtx／異metadata／staleID | start失敗、代替targetなし、再試行0 | tx/readbackの照合省略 |
| mailbox閉鎖／reply消失 | 追加start0、Unknownを成功にしない | resend／別taskでの直接start |
| start 受理後の管理 task と stage 4 | 管理 task が `AwaitRestartRecovery` で終了し、実際の stage 4 の `area.shutdown()`／WorkersJoined を既存の1秒期限内に通過する。期限を伸ばさない | continuation に `wait_finalization`／後続 stage 待ちを復活させる |
| 未処理依頼を残した loop 終了 | signal／accept 失敗／drain 中 signal の3経路で、buffer 内の返信待ちと bounded 配送待ちの両方が解放され、driver を残しても `host.shutdown()` が完了する | receiver を driver に残す、`close()` だけで未返信 sender を保持、Err／早期退出の解放漏れ |
| argv組立 | 旧launch/successor FDなし、config/webを維持 | 生argv全コピー／wireのargv採用 |
| full builtin names／build照合 | standard8以外のbuiltinも含む | fixture定数／標準8だけに縮退 |
| offと未認証の負例 | facade／mail／tx呼出し0 | enable確認を広告だけで代用 |
| 非Linux | gate/tx前Unsupported | ダミーenvironmentでtx作成 |

既存 lifecycle／idle／restart／planned_exec／watchdog／shutdown ordering の回帰も行う。時計は既存 test clock、競合は barrier/channel/join。製品 main/bootstrap へ試験入口を足さない。product artifact 試験は B2 (a) 後の `guard-cargo.sh` 経由、F4-P fixture の拡張は T2 だけ。

## 7. 影響するファイル・PR 対応

| PR | ファイルと責務 |
|---|---|
| V0-switch | §4.3 の既存型 accessor（StorageConfig／StorageHandle）を merge 前に追加。collector／sender の未定義型は先行参照しない。既存 `Inputs { host, backend }` から取得できる参照は再利用 |
| V-admission | collector 型の定義と同じ PR で、その中央入力・bootstrap accessor・構築・runtime hook 接続を追加。自動 recovery 開始前の接続まで担当 |
| **V1b-1**（B2 (a) merge後） | `crates/evorch-core/src/{main,planned_entry,lifecycle}.rs`、`lifecycle/control.rs`。mutable product loop、mailbox骨格、entry mask／instance／signals／argvの転送、両入口のinstall hookを各1箇所。constructor不変 |
| **V1b-2b** | `admin_composition/restart/finalization.rs`、根 `restart.rs` のinstall本体。FinalizationOwner、metadata、environmentの構築、coordinator登録、`shared-test-support/composed_core.rs` と故障試験 |
| V1b-2a | target／requirements の供給だけ。host所有構造を編集しない |
| V-safe／V-exec／V-cache | compose経路・実exec・cacheの統合検証。`scripts/check-cache-contracts.sh` を実行 |
| T1〜T4 | 公開switch、製品両入口とhello、native実restart、性能記録。公開前のhookを製品機能の公開と取り違えない |

V1b-2b は DG-1/V-admission、V1a-2、V1b-1、V1b-2a の後。中央入力の担当と編集順は §4.3 に従い、同表にない不足は親へ戻す。`composed_core.rs` が大きくなる場合は helper 分割を親に相談し、製品への test hook で省略しない。

本メモの設計・file:line 照合のみを行った。実装・ビルド・故障試験・mutation は未実施。
