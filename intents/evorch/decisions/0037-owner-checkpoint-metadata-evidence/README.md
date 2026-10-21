# ADR 0037 計測・検証 evidence

**採否に関する追記:** 最終再測定後、利用者は無休止readerの増加（17/33,948 → 18/25,230）を
「誤差の範囲として受け入れる」と判断し、Q9を採否対象から外した。
[ADRのユーザー判断](../0037-owner-checkpoint-metadata.md#性能受け入れq9)を参照。
これは統計的な非悪化の立証ではない。下記の測定値と元の数値条件の未達結果は維持する。

## 独立レビュー修正後の再検証・Q9 再測定（最終結果）

2026-10-21 21:01–21:07 JST、レビュー修正後の最終 Rust ソースを release ビルドして再測定した。
[事前固定計画](review-remeasurement-plan.md)、[driver](run-review.py)、[20反復全表](review-summary.md)、
[生データ](review-acceptance.jsonl)、[manifest](review-manifest.json)、[負荷・process記録](review-host.jsonl)。
各15秒×5反復、5,000,000 B、N=M=20、writer 0/100 ms休止、seed=3703。
全て直列実行し、各ケース直前の cargo/rustc/nextest/ベンチ並行処理は0。
開始時1分loadavgの範囲は4.28–8.02（24 logical CPU）であった。ただし稼働GUI等のサービスは
停止しておらず、専用ホスト隔離や全測定中の外乱不在を保証するものではない。
全20ケースで quick_check=ok、非contentionエラー0。before archived sourceは再度全2,263 blob一致を確認。

| writer休止 | 実装 | writer失敗/総数 (%) | reader失敗/総数 (%) | writer/reader成功ops/s |
|---|---|---:|---:|---:|
| 0 ms | before | 1/3,760 (0.0266) | 17/33,948 (0.0501) | 47.70 / 430.59 |
| 0 ms | after | 0/48,664 (0) | 18/25,230 (0.0713) | 638.36 / 330.72 |
| 100 ms | before | 0/2,925 (0) | 58/42,417 (0.1367) | 36.00 / 521.28 |
| 100 ms | after | 0/14,321 (0) | 0/382,701 (0) | 189.16 / 5,054.85 |

**元の数値条件ではQ9の無休止reader非悪化条件は未達。** 合算失敗率は0.0501%→0.0713%、rep1/2/3/5も悪化。
100 ms休止は数値条件を満たし、無休止writerの≤2%も満たすが、元の両モデル必須条件を満たしたとはしない。
採否上は冒頭に記した後日のユーザー判断によりQ9を対象外とした。
0件失敗の片側95%上限は、after writer 無休止0.00616%、100 ms休止0.02092%、
after reader 100 ms休止0.000783%。独立二項仮定であり、競合による相関・将来負荷を保証しない。
各反復の失敗件数・throughput・reader p50/p99と上限は全表に記録し、p99の単純平均は使用しない。

仮説（因果関係は未検証）: metadata短縮により無休止 writerの成功到着率が47.70→638.36 ops/s
（13.38倍）へ増加し、SQLite rollback journalのwriter競合・reader待ちが頻発する。
reader成功率だけでなくthroughputも430.59→330.72 ops/sへ低下、afterのp99は829–1,029 ms。
100 ms休止でreader失敗0・大幅なthroughput改善となることは、この仮説と整合するが単独では立証しない。
追加対策案は、(1) ownership writerのadmission/rate制御または公平な待機キュー、
(2) writerの連続取得抑制とretry/backoffの公平性、(3) reader/writer到着率を固定した補助試験で原因分離。
generation guardの削除やWALへの無条件変更は行っていない。元の数値条件は測定結果の評価として残し、
採否からのQ9除外は冒頭の明示的なユーザー判断として区別する。

実行の途中で、隔離環境のlibstdc++.so.6未設定により最初のbinary起動が失敗した（測定0件）。
LD_LIBRARY_PATHを明示して復旧した。続いて最初の有効測定1件の後に部分集計driverの空群処理が失敗した。
既存1件を破棄・再選別せず、同じbinary/plan/source hash・seed順序を検証して残り19件をappend再開した。
Rust測定ソースと比較/集計方法は変更していない。

最終検証: fmt --check、workspace/all-targets clippy -D warnings成功。
指定5crateは3,436 passed / 0 failed / 38 ignored（291 suites）、cache契約253 passed、I/O契約45 passed。
新規metadata6/crash3/contention3/shell14/storage7/maintenance2の計35件を各5反復、全て成功。
実行ログは指定worktree `target/review-fix-validation/`。レビュー指摘1/2/4の回帰は
未確認shellのラウンド終了前後active保持・IPC quiesce/handoff拒否、後付けstoreでの必須reference
保存拒否（round/finalize、未確認shellなし）、子プロセスguardの正常/panic終了を含む。

## 初回測定の前提と範囲（履歴）

2026-10-21、同一 Linux / Ryzen 9 7900 (24 logical CPU) / ext4・NVMe ホスト。
専用隔離ホストではなく、稼働 evorch・他処理が存在する。実環境DBにはアクセスせず、
指定 worktree の `target/owner-checkpoint-bench/` に新規の一時DBを作成した。
SQLite 3.53.2、rollback DELETE / synchronous=FULL、SHARED generation guard を維持する。

- before: `1d2cc55c` (#293) の本番 `OwnerPermit::checkpoint`（5 MB全履歴）。
- after: `1fb8b4c8` の本番 `OwnerPermit::checkpoint_metadata(Terminal)`（合成 receipt）。
- baseline は `git archive 1d2cc55c` の展開。全2,263ファイルを Git blob と照合し一致。
- 各 writer は別 thread 行、1 DB共有。reader は同じ permit の `mutation_guard()` を取得し即解放後1 ms休止。
- writers は開始barrier以降、完了後0/100 ms休止。15秒で新規操作を停止し、in-flight retry終了まで待つ。
- writerの失敗は本番10秒deadline/各attempt2秒を使い切った最終失敗。外側に追加retryはしない。
- after の実run_contexts保存・hash/redaction・events DB I/Oと競合、active遷移のコストは含めない。
  その順序とfenceは runtime/storage の別テストで確認する。
- `payload_bytes` フィールドは入力履歴のserialized長であり、afterの保存metadataサイズではない。
  after は run ID、時刻、messages_end、64桁digestだけを本番コードに渡す。
- `busy_attempts` は戻ってきた最終contentionエラー数（内部attempt数ではない）。
- `other_errors` は全ケースで空、`quick_check` は全ケース `ok`。
- 各反復でばらつきがあり、合算だけで将来の失敗率は保証しない。特にacceptance無休止rep=1のreaderは悪化。

## ファイル

- `acceptance.jsonl`: 5,000,000 B、N=M=20、0/100 ms、各2反復の全8ケース。
- `run-acceptance.py`: seed37で順序を混ぜたドライバ（前任の一時crate用）。
- `before.rs` / `after.rs`: この計測で使用した一時crateの全入力ソース。
- `matrix.jsonl`: 100,000 / 304,247 / 2,132,213 / 5,000,000 B × N=M=2/5/10/20 × 0/100 ms × before/after の補足各1反復（全64ケース）。引き継ぎ時点の28ケースを検証し、残り36ケースを同一binaryで再開・完了した。実行時間帯が分かれ、後半は隔離ホストではなくビルド等の他処理もあるため、単一セッションの均一負荷試験とはしない。
- `matrix-summary.md`: 補足全32組のwriter/reader失敗数と率。5MB・N=M=20・無休止のreaderは0%→0.0578%で、この補足反復はQ9未達。主要計測の合算だけで安定した非悪化を主張しない。

## 再実行

指定 worktree に以下の配置を作る（ソースにROOT絶対パスが埋まっているため、別パスなら変更する）。

```
target/owner-checkpoint-bench/baseline/  # git archive 1d2cc55c | tar -x
target/owner-checkpoint-bench/before/src/main.rs  # before.rs
target/owner-checkpoint-bench/after/src/main.rs   # after.rs
```

before/after のCargo.tomlは各々別workspace、edition=2024、version=0.1.0、
package名 `owner-checkpoint-bench-before` / `owner-checkpoint-bench-after`。
依存runtime/providersはbeforeではbaseline/crates、afterではworktree/cratesへの絶対path、
`rusqlite={version="0.40.2",features=["bundled"]}`, `serde_json="1"`。
afterだけstorageもworktreeのpath依存にする。`[profile.release] debug=0`。

```sh
# worktreeで。ホストNix daemonと利用可能なPython3を使う
TMPDIR=/tmp nix develop -c sh -c '
  export CARGO_TARGET_DIR="$PWD/target/owner-checkpoint-bench/build"
  cargo build --release --offline --manifest-path target/owner-checkpoint-bench/before/Cargo.toml &&
  cargo build --release --offline --manifest-path target/owner-checkpoint-bench/after/Cargo.toml
'
# run-acceptance.py を target/owner-checkpoint-bench/run.py に配置して実行
TMPDIR=/tmp nix develop -c python3 target/owner-checkpoint-bench/run.py
# devshellにPythonが無い場合は利用可能なPython実体を指定する。Rust/SQLiteコードは同じ。
```

計測済みbinary SHA-256:

- before: `f2f53ea6c6d8b1862700df4b90394b67aafdddd9edf14040e2ef8c5efbc67f47`
- after: `69aa5caf845643bbea07f186f8538389f1fe98038f590061f3c2f94d76d61fde`

## 検証

初回は指定worktreeで `TMPDIR=/tmp nix develop` を使用し、引き継ぎ後も全検証を再実行した。
最終コードの再検証は隔離環境で同じNix storeのtoolchainを使い、`TMPDIR=/tmp` と
worktree内の `EVORCH_OUTPUT_DIR` を指定した。libclangとglibc headersはNix storeの絶対pathで与えた。
詳細一時ログは前任分 `target/owner-checkpoint-validation/`、引き継ぎ分
`target/owner-checkpoint-validation-takeover/` にあり、本書とADRに結果を永続記録する。

| 検証 | 実際の結果 |
|---|---|
| cargo fmt --all -- --check | 成功 |
| cargo clippy --workspace --all-targets --locked -- -D warnings | 成功 |
| cargo test --locked -p runtime -p event-bus -p gui -p recorder -p storage | 最終コードで3,433 passed / 0 failed / 38 ignored、290 suites（doctest含む）。追加境界テスト前は3,432 passed |
| scripts/check-cache-contracts.sh | 253 passed、225 skipped |
| scripts/check-io-contracts.sh | 45 passed、1,319 skipped |
| metadata/crash/contention/shell_jobs + storage run_contexts ×5 | 最終コードで毎回32 passed / 0 failed（8+2+3+12+7）。前任時点は31 passed |
| 最終観測修正後 ownership::contention_tests | 19 passed |

初回旧配列assert2件失敗→metadata契約へ更新。次のshellキャンセルassert1件失敗は単独でも再現し、
store不在では不確実性を永続化できずhandleを保持すべきため、本ADRの安全契約に沿って更新した。
その後に上記全検証が通過した。引き継ぎ時にはdrain済みrunのidleが他runのactiveを無視して成功する
no-opを拒否するよう補強し、snapshot準備中のquiesceからIPC handoffまでの境界テストを追加した。
隔離環境での途中失敗はlibclang/headerの未設定、長いTMPDIRのSUN_LEN超過、共有artifact出力先の
書込不可であり、環境変数とworktree内出力先を設定後、失敗したbenchmark_wireの単独再実行と全回帰が成功した。
これら環境差を回避するために契約assertを弱めていない。実環境DBの掃除や電源断テストは実施していない。

## 履歴整理と旧ハッシュの対応

WIP/前任履歴は以下4単位に squash した。base は `1d2cc55c` のままであり、
#293を別コミットとして再取り込みしていない。

| 整理後 | 内容 | 旧ハッシュとの対応 |
|---|---|---|
| `2cbaec3b` | storage receipt API・redaction/digest・storageテスト | `6021cc29` のstorage部分 |
| `393ef71e` | owner metadata/fencing、agent loop/finalize、回帰テスト | `6021cc29` のruntime部分、`eac72454`、`19116669`、`1fb8b4c8`、`bbe52144`、`33628214`、`82fdd214`、`cbba4552` のruntime/test部分 |
| `8a3bc30e` | offline掃除コマンドと安全性テスト | `6021cc29` / `e06578bf` のmaintenance部分、`cbba4552` のtest分離 |
| `docs(adr): record owner checkpoint contracts and review evidence`（本書を含むコミット） | ADR0024 Addendum・0037・evidence | `e06578bf` のdocs部分、`ad8ad68c`、`9ef10648`、`871825d2`、`51b99fbd`、`cbba4552` のplan/docs部分、`6d89137a` |

したがって初回計測のafter=`1fb8b4c8`は、整理後`393ef71e`のowner metadata実装に対応するが、
新しいruntime回帰修正前の計測として履歴を維持する。最終再測定のafterは`2cbaec3b`+
`393ef71e`+`8a3bc30e`のRust tree（manifestのruntime/storage source SHA-256でも一致確認）。
文書コミット自身のハッシュは自己参照できないためsubjectで識別する。

整理前の最終WIP=`6d89137a`、tree=`f3fd1bdeafc81571841d94469cad351bbb41fe5e`。
3コミット構築後にdocsを復元したindexのtreeがこの値と完全一致し、全差分0を確認した。
その後の変更は本節（旧→新ハッシュ対応とコミット別検証記録）の追記だけであり、Rustコード・
計測データ・受け入れ条件は変更していない。

各コードコミットで `cargo check --workspace --all-targets --locked` が成功した。
コミット1は `cargo test -p storage` 306 passed、コミット2はmetadata/crash/contention/recovery/shellと
ownership unitの97 passed、コミット3はmaintenance2件がpassed、evorch CLI test targetのビルド・実行成功（unit test 0件）。
文書コミットでもworkspace/all-targetsのcheck・clippy -D warnings、fmt --checkと
metadata/crash/maintenance/storage run_contextsの18件を再検証した。
全コミットは設定済みOpenPGP鍵で署名し、pre-commit hookのfmtを通した。push/PR作成はしていない。

## 再レビュー2件への追補（2026-10-21）

上記の「最終再測定」とソースhashは、その測定時点の履歴である。今回、generation事前確認の
retryを修正したが性能ベンチは再測定せず、測定値・元のQ9未達結果を変更していない。
後日のユーザー受け入れ判断は冒頭およびADRのQ9節に記録した。

### 指摘1: generation事前確認の競合

- `validate_generation`にも既存の有限retryを適用し、各接続を`Wait::Budget`で開く。
  各試行でgeneration/Releasedを再確認し、guard/接続を解放してからbackoffする。
  SQLiteの一時競合だけを既存分類で再試行し、fencing/Releasedは再試行しない。
- 通常ラウンドとfinalizeの事前確認はblocking poolで実行する。通常ラウンドではCancel/Stopを
  観測し、finalizeは割り込みを無視するが有限予算を維持する。owner tx内の再検証は残す。
- EXCLUSIVEロックと仮想時計で、2秒の単一試行を超えて2.1秒後に解消する競合、
  注入した350 ms deadline/100 ms attempt、割り込み、競合後のgeneration変更/Releasedを検証した。
- 単一async workerの統合テストは、注入予算でguardの第2試行失敗を観測してからロックを
  解放し、Stop/Cancelの予定phase・terminal metadata・当該runだけのactive解放を確認する。
  deadline超過ではError・競合期限超過診断・active保持を確認する。EXCLUSIVEロックが
  event busのmutation guardも拒否するため、この失敗経路の終端イベントは公開されない。
  通常ラウンドの割り込みからfinalizeへの試験では、terminal snapshotと参照の一致も確認した。

### 最終ソースの検証結果

全て指定worktree内で`TMPDIR=/tmp nix develop`を使用した。ログは
`target/review-takeover-validation/`（最終ゲートは`final-*.log`）。

| 検証 | 実際の結果 |
|---|---|
| `cargo fmt --all -- --check` | 成功 |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | 成功 |
| `cargo test --locked -p runtime -p event-bus -p gui -p recorder -p storage` | 3,444 passed / 0 failed / 38 ignored、291 suites（doctest含む） |
| `scripts/check-cache-contracts.sh` | 253 passed、225 skipped |
| `scripts/check-io-contracts.sh` | 45 passed、1,324 skipped |
| 新規・関連テスト5反復 | 各111 passed / 0 failed（計555成功） |

反復対象はownership単体66、通常preflight/finalize境界1、metadata6、crash3、contention6、
shell14、maintenance2、storage run_contexts7/redaction6。新規8件を含む。
最初のEXCLUSIVE deadline試験では、既存のevent bus fenceを見落として終端イベントを期待した
assertが失敗した。fenceを変更せず、上記のError/診断/active保持を検証するよう試験を修正した。

### 履歴とコミット別検証

当初はautosquashを予定したが、利用者が追加確認への回答で「rebaseせずfixupを残す」と指定した。
そのため元の4コミットを変更せず、指摘1はruntime宛、指摘2はADR宛の署名付きfixupで保存した。
同じworktree内で下記各コミットへ順に切り替え、**全8コミットで
`cargo build --workspace --all-targets --locked`が成功**した後、元のbranchに戻った。

| コミット | 関連テストの実際の結果 |
|---|---|
| `2cbaec3b` storage | storage全体306 passed |
| `393ef71e` runtime | ownership/metadata/crash/contention/recovery/shell、97 passed |
| `8a3bc30e` offline | maintenance2 passed、CLI test target成功（0件） |
| `b2c5401f` ADR | fmt成功、contention/maintenance5 passed |
| `315f9128` 指摘1: retry/off-thread・EXCLUSIVE回帰 | ownership/metadata/crash/contention/recovery/shell、104 passed |
| `f32a8606` 指摘2: Q9ユーザー判断 | fmt成功、contention/maintenance8 passed |
| `3bd99c17` 指摘1: 通常preflight/terminal保存境界 | 上記runtime関連と通常preflight、105 passed |
| `76b10c6f` 指摘1: 第2試行の観測を明示 | 同上105 passed |

`315f9128`の初回並列実行で、既存
`a_begin_wait_and_a_commit_wait_share_one_attempt_budget`のtrace採取assertが1回失敗した
（期待`["commit"]`に対し空配列）。DB予算・rollbackのassertは通過した。
同じコミットのownership単体を直列化して全66件を再実行すると成功し、以降のコミット別単体も
直列実行した。最終ソースの全テスト・5反復では再現していないが、trace欠落の原因は未特定である。
失敗ログも`commit-5-first-attempt.log`に残した。この観測のために契約assertを弱めていない。

追加コミットは既存OpenPGP鍵で署名し、pre-commit hookを実行した。
実環境DBの操作、push、PR作成、履歴書換えは行っていない。
