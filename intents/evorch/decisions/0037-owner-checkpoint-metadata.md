# ADR 0037: owner checkpoint を小さな所有権メタデータにし、履歴を run_contexts に一元化する

Status: Accepted（2026-10-21、案1。最終再測定では元のQ9数値条件は未達。後日のユーザー判断で無休止readerの増加を受け入れ、Q9を採否対象から除外。下記「性能受け入れ（Q9）」参照）

## Context

`owners.db` の `owner_checkpoints.payload` は、tool ラウンド終了時と finalize 時に
`visible_messages()` の全履歴を書いていた。復元時の本番の読み手は存在せず、復元は
ADR 0027 の `run_contexts`（events DB）を読む。後者は raw 履歴と compaction checkpoint
を保存するので、投影済みの owner payload より情報が多い。thread を共有する親子 run が
同じ owner 行を上書きするため、従来の payload も thread 全体の一貫した履歴ではなかった。

計測資料（`dblock/RESULTS-worker152.md`、`PROPOSAL-worker152.md`、
`TABLES-worker152.md`、相談用 `ADR-DRAFT-owner-checkpoint.md`）では、実 DB の owner
payload は平均 304,247 B、最大 2,132,213 B、DB は 47,628,288 B であった。
DELETE/FULL、N=M=20、5,000,000 B の無休止 writer モデルで writer 失敗率は
22.09%、1 KB 化モデルは 1.17% だったが、reader は 0% → 7.91% に悪化した。
これは採用実装の値ではない。22 件の本番 locked Error がすべて owners.db 起因かは未証明である。

PR #293（`1d2cc55c`）で DB/operation/phase 等の観測、tx 外直列化、有限 retry と
残予算管理が入った。本 ADR はその上の payload 縮小本体を扱う。

### 前提と選択肢

- 所有権の権威は `owners.db` の owner/generation/active/lease にだけ置く。
- rollback journal の SHARED guard が claim COMMIT を止める fencing を維持する。
  WAL 化、権限判定のキャッシュ、無 guard の emit は採用しない。
- 案0（履歴を残して retry だけ）では、生履歴の重複・サイズ依存が残る。
- 案2（immutable ファイル + fsync + GC）は独立復旧を提供するが、新たな永続層を要する。
- **案1を採用する。owner checkpoint を events DB から独立した復旧履歴にはしない。**
  履歴の正本は `run_contexts` とし、owner の記録は助言的な参照とドレイン境界である。

## Decision

### 1. payload と互換性（Q1・Q5・Q6）

DDL と `ThreadOwner` の永続形式は変更せず、既存の3列の行の payload に JSON オブジェクトを置く。

```json
{
  "v": 1,
  "generation": 7,
  "boundary": "terminal",
  "runs": {
    "run-…": {
      "boundary": "terminal",
      "checkpointed_at_ms": 0,
      "snapshot": {
        "kind": "run_context",
        "updated_at_ns": 0,
        "messages_end": 12,
        "sha256": "…"
      }
    }
  }
}
```

`boundary` は `turn_end | terminal | idle`、snapshot は上記または
`{"kind":"none","reason":"no_run_store | write_failed | skipped"}` である。
SHA-256 は **storage が redaction・JSON 正規化して実際に保存する messages_json** の bytes に対する値である。
入力の生履歴の digest ではない。storage writer はコミット後に receipt を返す。
run ID は map のキー、generation は行と payload にあり、lease/permit や本文・失敗理由の生文字列は入れない。

現在 active な run の既知の参照と、今回完了した直近1 run を保持する。run ごとにも boundary を持ち、
親子の境界を混同しない。idle は全 active run のドレイン後にだけ許し、最後の1件まで残す。
サイズは既知の active run 数に比例し、履歴長には比例しない。run 数の固定上限は設けない。

旧配列（`[]` を含む）、壊れた JSON、未知 v は usable な参照なしとして扱う。
claim/handoff/release は payload を復元条件にしない。generation が変わった旧参照は次の checkpoint で捨てる。
`INSERT OR IGNORE … '[]'` の release/claim プレースホルダーは維持する。
新バイナリが旧 DB を上書きでき、旧バイナリの位置指定3値 INSERT もそのまま動く。
混在時に旧バイナリが再び配列を書くことはあるため、全バイナリ更新まで生履歴の再導入は防げない。

互換 `checkpoint(messages)` / `checkpoint_permit` は本文を無視し、run permit は turn_end、
thread permit は idle として none(skipped) を記録する。idle は他 run が active なら拒否する。
本番 agent loop は明示的な `checkpoint_metadata` 経路を使う。履歴を書き出す新経路は作らず、
redaction は既存 `run_contexts` の storage 境界に一本化する。

### 2. 書き込み順序・確定点・fencing

1. owners の短い generation guard で事前確認し、直ちに解放する。events DB I/O 中は guard を持たない。
2. tx 外で `run_contexts` の snapshot を同期保存し、コミット済み redacted receipt または none を得る。
3. owners.db の Immediate tx 内で owner を再読し、lease/generation を再検証する。
   `checkpoint_run` による active 除去、他 run の参照との merge、payload と state の更新を一緒に commit する。
   **この COMMIT がドレイン・バリアの確定点**である。
4. #293 の retry は接続/tx を drop してから backoff し、各 attempt で再検証する。
   予算、キャンセル、SQLite error 分類、callback を再実行しない規則は変更しない。

履歴の serialize/hash/redaction/外部 I/O は owners 書込み tx に入れない。tx 内は小さな metadata と
state のみを扱う。terminal は tool-end で既に active から外れた run の参照も更新できるが、
Released または旧 generation の permit は拒否し、active を復活させない。

2つの DB の原子性は取らず、2相コミットもしない。

| 許容する状態 | 発生境界 | 扱い |
|---|---|---|
| 履歴が新しく参照が古い | snapshot commit 後の kill、owner tx の rollback・fence | 復元は run_contexts を直接読む。owner の権威は進まない |
| 参照が履歴より新しい／一致しない | WAL/NORMAL の OS クラッシュ・電源断、保持期限削除、同一runの後続保存 | updated_at_ns / messages_end / sha256 の一致を検証し unavailable とする。claim を止めない |
| 参照 none | run store 不在、snapshot 拒否・失敗、非保存境界 | 境界は commit できるが、その境界の履歴復旧を保証しない |
| 旧 snapshot が後継履歴を上書き | 同一run IDに対する既存 last-writer-wins | owner参照は fence、履歴は fence しない。不一致を検出できるだけである |

SQLite のプロセス kill と OS/電源断は異なる。events DB は WAL/NORMAL で、同期応答は SQLite の
コミット完了であって FULL fsync への昇格ではない。process kill 後の回復は試験するが、電源断による
直近 WAL commit 消失を防ぐとは主張しない。参照の照合 API は利用不可を返すだけで、修復しない。
本番の復元は参照を辿らず `run_contexts` を直接読むため、存在しない参照消費者や診断を新設しない。

### 3. 保存失敗と終端順序（Q2・Q3）

owner checkpoint **用 snapshot** の失敗は警告・既存 snapshot 診断を残し、none(write_failed) で続行する。
run store がなければ none(no_run_store) とする。GUI は RunStore::open 失敗時に
`continuing without persistent run restore` と警告して run store なしで起動できる。
容量上限（既定10 GiB）での writes_suspended、I/O、writer 切断、SQL 拒否などが失敗経路である。
本番での snapshot 失敗頻度の定量値は未計測であり、「低頻度を実証済み」とはしない。

続行時は前の snapshot までしか戻れない、または disk restore できない。owner metadata は履歴の
代用品でなく、成功したドレインだけが残る。所有権の保存失敗は従来どおり致命である。

**別の必須保存と混同しない。** prepared-context（起動・圧縮後 prefix の保存）と
assistant-reference の保存失敗は、利用者の追加確認に従い従来どおり致命・今回の範囲外とする。
run store 設定時の tool dispatch 前 intent 保存失敗も、未記録の副作用を実行しないため致命のままである。
各経路の呼び出し頻度と影響は次のとおり（保存**失敗率**の本番定量値はいずれも未計測）。

| 経路 | 呼び出し頻度・場所 | 失敗時の影響 |
|---|---|---|
| owner 用ラウンド snapshot | `agent_loop.rs::save_checkpoint`、完了 tool round ごと（tool なしの完了応答も含む） | 通常の履歴保存失敗は警告し none、履歴は直前の有効 snapshot まで。assistant-reference 必須保存失敗、または未確認 shell 効果を保存できない場合は Error、後者では active/handle 保持 |
| owner 用 terminal snapshot | `agent_loop/finalization.rs::finalize`、各終端に1回（owner 失敗時は Error 再保存を試行） | 通常の履歴保存失敗は未確認 shell 効果がなければ none で完了可、あれば致命・active/handle 保持。assistant-reference 保存失敗は shell の有無にかかわらず Error |
| prepared-context | `agent_loop.rs` 起動時の `persist_checkpoint`、`compaction/mod.rs` の `persist_prepared_context`（圧縮時） | store 設定時の保存失敗は Error。provider 入力 prefix を永続化できないまま新規実行しない |
| assistant-reference | `agent_loop.rs` の完了応答に対する `capture_pending_reference_at` → `runtime.rs` / `run_store/review.rs` の保存（応答ごと） | store 設定時の保存失敗は Error、pending reference を破棄し未同期を記録。説明・review lineage の欠落を隠さない |
| tool intent | `agent_loop.rs::persist_tool_intent`、tool dispatch 前（tool batch ごと） | Error として dispatch を止める |

store 不在は上記既存の必須保存でもエラーに変更しない。保存に失敗した場合と不在の場合を混同しない。
したがって store が容量制限で初めから停止していれば、prepared-context により run 自体は Error に
なる場合がある。それでも finalize の owner checkpoint は none で進められる。

finalize は ADR 0027 §5 の順序を保つ：shell/post-edit の回収 → 終端 snapshot → owner 参照の commit →
shell handle 解放／workspace cleanup・引継ぎ準備 → 終端通知の公開。
snapshot の phase はその時点の予定 phase とし、owner checkpoint に失敗したら Error へ遷移し
best-effort で再保存する。owner COMMIT は失敗しているので Error snapshot を新たに指す参照は作らない。

run の途中から共有 `AgentRuntime` に store を付けられるため、ephemeral な assistant-reference
を snapshot の作成中に durable へ昇格させる場合もある。`SnapshotError::AssistantReference` は
履歴 snapshot の単なる保存失敗と型で区別し、ラウンド側は呼び出し元へ返し、finalize でも
shell の有無にかかわらず Error とする（Q2）。`none(write_failed)` に畳み込まない。

shell の回収失敗は引き続き致命である。未確認 shell 副作用があり、その状態を snapshot に記録できない
（store 不在・skip を含む）場合も致命とし、active を下ろさず handle を保持する。
単なる履歴欠落と違い、後継が副作用の有無を判断できないまま進む危険があるためである。
この判定は terminal だけでなくラウンドの TurnEnd commit **前**にも行う。
致命条件のときは `owner_turn_open` を true のまま維持し、active を外す checkpoint を行わず、
finalize が保存を再試行する。再試行でも保存できなければ quiesce 後も active は残り、
handoff / release は拒否される。通常の none snapshot は引き続き許可し、この安全条件を
全ての none に拡張して通常 run の完了を不必要に止めない。

### 4. 既存 DB の掃除（Q4）

**起動時自動ではなく、明示的な offline 保守コマンドを採用する。** 旧バイナリは協調ロックを持たず、
lease や socket 応答だけでは「プロセスが存在しない／これからアクセスしない」と断定できないためである。
実環境 DB への実行はこの変更では行わず、利用者確認後に別途実行する。

```sh
# 全GUI/CLI/ownerを停止し、再起動も止め、十分な空き容量を確認した後だけ実行
TMPDIR=/tmp nix develop -c cargo run --locked -p evorch --bin evorch -- \
  owner-checkpoints-compact --root /absolute/path/to/evorch/threads --all-owners-stopped
```

- 明示 ack、root 内に `.sock` がないこと、既存 DB が DELETE mode であることを要求する。
  stale socket も拒否する。停止を別途確認した場合だけ利用者が stale socket を取り除く。
- DB を作らず READ_WRITE で開き、busy_timeout=0、locking_mode=EXCLUSIVE、BEGIN EXCLUSIVE で
  競合 reader/writer がいれば変更前に拒否する。lock 取得後にも socket を再確認する。
- secure_delete=ON で全 payload を `'[]'` に置き換えて commit し、同じ接続・exclusive lock のまま
  VACUUM する。所有権 state/generation/schema は変えない。途中で新 owner が起動しても DB は同時アクセスさせない。
- lock を持たない休眠プロセスを SQLite だけで検出することは不可能であり、全停止 ack は必須条件である。
  VACUUM は最大およそ DB の2倍の追加空き容量を要し、失敗したら全停止を維持して再実行できる。
- 目的は容量回収と redaction 前の生コピー削除である。ただし filesystem snapshot/backup/SSD の物理消去は
  保証しない。コピーを取る場合も秘密を含むものとして管理し、不要な backup は別途廃棄する。

### 5. 他 ADR・範囲（Q7・Q8・Q10）

ADR 0024 Decision 4 の checkpoint は本 ADR のドレイン境界であることを Addendum で明確化する。
ADR 0027 の復元契約（履歴と実行権を分け、現在の権限を付与する）を変更しない。
`run_contexts` への generation fence は別課題として明記し、今回は実装しない。
旧世代の遅れた snapshot は既存 last-writer-wins を維持する試験で固定し、owner参照・active は変えない。

writer 公平性、reader batch、backpressure、checkpoint coalescing は別スコープである。
payload 縮小後に reader 失敗が増えても無 guard で通す・WAL 化する対策は取らない。

## Consequences

- owners.db に新しい生履歴を増やさず、owners の writer 占有が会話長に依存しなくなる。
- run store 不在時の独立した復旧履歴は提供しない。必要になれば案2を別 ADR で再検討する。
- 小さい metadata でも無休止 writer の到着率は増える。writer 成功率だけでは性能合格にならない。
- 旧配列を assert した4テストは非空履歴の往復を検証していなかった。明示 checkpoint は metadata 契約へ
  更新し、idle quiesce の release/claim プレースホルダー `[]` は別契約として維持する。
- 新規試験は metadata、none、旧形式混在、複数active run、旧世代遅延、snapshot照合、secret非複製、
  5 MB×1,000回、cleanup拒否とVACUUMを対象とする。
- process-kill 試験は保存前／保存後・owner tx前／未commit二行tx／commit後を対象とし、後継 claim と
  continue_goal を確認する。未commit境界は同等 SQL による rollback journal 試験であり、SQLite内部の
  COMMIT syscall の任意位置・電源断を注入する試験ではない。

### 性能受け入れ（Q9）

当初の数値条件は、5,000,000 B、N=M=20 で writer 最終失敗率 ≤2%、かつ reader 失敗率が before 比で悪化しないことを、
無休止モデルと writer 完了後100 ms休止モデルの両方で要求した。
実 Registry/OwnerPermit を使う dblock 一時 crate による before/after を以下に記録する。

**最終再測定後のユーザー判断:** 無休止readerの増加（17/33,948 → 18/25,230）は
「誤差の範囲として受け入れる」と利用者が判断し、Q9を本ADRの採否対象から外した。
これは利用者による受け入れ判断であって、統計的な非悪化を立証したという意味ではない。
測定値と、元の数値条件では無休止readerが未達であるという結果は変更しない。

#### 独立レビュー修正後の最終再測定

[事前固定計画](0037-owner-checkpoint-metadata-evidence/review-remeasurement-plan.md)に基づき、
before=main `1d2cc55c`、after=最終Rustコードを5,000,000 B・N=M=20・各15秒×5反復、
0/100 ms休止で直列測定した。各ケース前に並行ビルド・テスト0を確認した。
全20ケースのquick_check=ok、非contentionエラー0。

| writer休止 | 実装 | writer最終失敗/操作（率） | reader失敗/操作（率） |
|---|---|---:|---:|
| 0 ms | before | 1/3,760 (0.0266%) | 17/33,948 (0.0501%) |
| 0 ms | after | 0/48,664 (0%) | 18/25,230 (0.0713%) |
| 100 ms | before | 0/2,925 (0%) | 58/42,417 (0.1367%) |
| 100 ms | after | 0/14,321 (0%) | 0/382,701 (0%) |

**元の数値条件では無休止readerの非悪化条件は未達であり、Q9全体は未達。**
この測定結果は維持する。採否上は上記の後日のユーザー判断によりQ9を対象外とした。
0失敗の片側95%上限はafter writer 0/100 msで0.00616%/0.02092%、after reader 100 msで0.000783%。
独立二項仮定の参考値で、競合の相関や将来率は保証しない。
各反復throughput・reader p50/p99・全失敗件数は
[全表](0037-owner-checkpoint-metadata-evidence/review-summary.md)を参照。
無休止writer throughputは約13.38倍、reader throughputは約23%低下した。
writer到着頻度増によるreader starvationが仮説（未検証）であり、公平なadmission/待機キュー、
連続writer抑制、到着率を固定した原因分離試験を追加対策案とする。
既存generation guardを弱める変更や性能条件に合わせるための無断の追加制御は行わない。

#### 初回測定（履歴）

2026-10-21、before=`1d2cc55c`（#293）、after=`1fb8b4c8` を release ビルドし、
5,000,000 B・N=M=20・各15秒×2反復を seed=37 で順序を混ぜて単独実行した。
DELETE/FULL、reader の guard 保持追加待機0 µs、各 reader 操作後1 ms休止、writer は0/100 ms休止。
全8ケースの `quick_check=ok`、非contentionエラー0。before の展開元は全2,263ファイルを
base の Git blob と照合して一致した。

| writer 休止 | 実装 | writer 最終失敗/操作（率） | reader 失敗/操作（率） |
|---|---|---:|---:|
| 0 ms | before (#293) | 0/1,112 (0%) | 49/7,764 (0.6311%) |
| 0 ms | after | 0/15,159 (0%) | 18/5,433 (0.3313%) |
| 100 ms | before (#293) | 0/1,242 (0%) | 30/17,677 (0.1697%) |
| 100 ms | after | 0/5,748 (0%) | 0/160,294 (0%) |

**この2反復合算では、両モデルで指定数値条件を満たした。** ただし専用の隔離ホストではなく、
reader の反復別失敗数は無休止 before=5/6,207・44/1,557、after=5/2,521・13/2,912 と大きく変動した。
無休止 rep=1 単独は reader が0.0806%→0.1983%に悪化しており、改善が全反復で一様だとは主張しない。
将来の負荷・失敗率の保証や独立 Reviewer の受け入れ判定を代替する値ではない。

これは実 `OwnerPermit` / `Registry` を呼ぶ **ownership マイクロベンチ** である。
before は全履歴の `checkpoint`、after はコミット済みと仮定した固定長合成 receipt を使う
`checkpoint_metadata(Terminal)` を測る。各 writer は別 thread 行、同一DBを共有し、idle な所有権 state
への反復更新を行う。after の snapshot 自体の保存/redaction/hash と events DB の競合、実 tool 実行、
active の遷移は測定に含まれない（順序/fence/active の正しさは別テスト）。
生データ、入力ソースと再実行手順は [evidence](0037-owner-checkpoint-metadata-evidence/README.md) に保存する。
補足マトリクス（100 KB〜5 MB、N=M=2/5/10/20）は同 evidence に記録する。

補足5秒×各1反復の全64ケースも完了した（前任28ケース＋引き継ぎ後36ケース、同一binary）。
全件 quick_check=ok、非contentionエラー0、after writerの最終失敗0。ただし readerは32組中10組で悪化した。
特に5,000,000 B・N=M=20の無休止は before=0/3,650（0%）、after=1/1,729（0.0578%）で、
**この補足反復ではQ9のreader条件未達**。100 ms休止は20/4,044（0.4946%）→0/21,984（0%）。
主要15秒×2反復の合算では指定数値条件を満たす一方、補足を含めて一様な非悪化は確認できず、
安定した性能受け入れの成立は未確認である。時間帯と共有ホスト負荷による差も分離できない。
writer公平性や無guard化を本変更に追加して数値を合わせることはしない。
[補足全表と失敗率](0037-owner-checkpoint-metadata-evidence/matrix-summary.md)を参照。

### 実装検証

独立レビュー修正後の最終コードでもfmt --check、workspace/all-targets clippy -D warnings成功。
指定5crateは3,436 passed / 0 failed / 38 ignored、cache契約253 passed、I/O契約45 passed。
metadata/crash/contention/shell/storage/maintenanceの計35件を各5反復し全て成功した。
レビュー回帰はstore不在/保存拒否と実shell・ownership・IPC quiesce/handoff、後付けstoreでの
assistant-reference保存拒否（round/finalize）、crash子プロセスのpanic時kill/waitを含む。
以下は修正前の検証履歴である。

`TMPDIR=/tmp nix develop` で fmt --check、workspace/all-targets clippy -D warnings が成功した。
引き継ぎ後の最終コードでも同じNix toolchainでfmt/clippyと全回帰を再確認した。
指定5crate (`runtime/event-bus/gui/recorder/storage`) は3,433 passed / 0 failed / 38 ignored、
cache契約253 passed、I/O契約45 passed。新規 metadata/crash、ownership contention、shell jobs、
storage run_contexts は各5反復（毎回32 passed）。最終payload観測修正後の所有権unit19件も成功した。
追加試験はsnapshot準備中のquiesce→履歴保存だけではhandoff不可→参照COMMIT後handoff成功を確認し、
他runがactiveの間はdrain済みrunもidle成功を返さないよう補強した。
初回に旧配列assert2件、次にstore不在時の未確認shell handle解放assert1件が失敗したが、
本文非複製・不確実性を永続化できない場合のhandle保持という本ADRの契約へ更新後に通過した。
隔離環境のlibclang/header、一時path長、artifact出力先による途中失敗は環境を修正して再実行し、
契約assertや本番コードを弱めず通過した。詳細は[evidence](0037-owner-checkpoint-metadata-evidence/README.md)。
既存の ignored テストは実行成功に含めない。実環境DBの掃除とOS/電源断試験は実施しない。

## Related

- [ADR 0024](0024-process-owner-claim.md): owner/generation/claim/ドレイン
- [ADR 0027](0027-restore-contract.md): run_contexts と復元権限、終端順序 §5
- [ADR 0018](0018-sqlite-storage-schema.md): events DB WAL/NORMAL/single-writer
- PR #293 / `1d2cc55c`: ownership 観測・tx外直列化・有限 retry
