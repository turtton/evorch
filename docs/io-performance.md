# I/O と保存性能の回帰対策

2026-10-01 時点。DB の容量上限は SSD 書き込み量の上限ではない。小さい DB
でも、INSERT/DELETE、インデックス更新、WAL の再利用と checkpoint が継続すれば
累積書き込みは増える。保存する必要のないイベントを入口で除き、必要な更新を
まとめ、処理量と累積 I/O の両方で検証する。

## 調査から得た方針

[紹介された gist](https://gist.github.com/jun76/bf5f8fdde0e3866f537fc9422c65326d)
は Codex の診断ログ DB を対象にした非公式対処を説明している。
[Codex issue #28224](https://github.com/openai/codex/issues/28224) では、
高頻度の診断イベントの追加・削除と SSD 書き込みの増加が報告された。
Codex 側には [保存バッチを大きくする修正 #39294](https://github.com/openai/codex/pull/39294)
と [SQLx の警告が診断ログ保存へ戻る循環を除く修正 #39592](https://github.com/openai/codex/pull/39592)
がある。これらは発生源、頻度、自己参照をそれぞれ検査する必要性を示している。
報告値はその環境の計測であり、evorch や現在の全 Codex ビルドへ外挿しない。

evorch で実測した全履歴の再集計は、CPU と読み取り増加の原因だった。
報告された 130 GiB の書き込みの発生元はまだ確定していない。
以下はその値の説明ではなく、同種の増幅を防ぐ設計上の方針である。

- 復元に必要な状態変化・終了イベントは保持し、通常の heartbeat を永続ログへ
  無制限に積まない。ストリーミング差分はサイズ・時間の上限を持ってまとめる。
- 容量判定は通常の追加保存で過去 payload を走査しない。複数セッションの切替、
  複数 writer、再起動と日付境界でも正しい上限を保つ。
- usage は分単位で集計する。保留値なしの flush、変更なしの状態保存は書き込まない。
- 診断用カウンターはメモリ上で更新する。診断を記録するための I/O が、同じ診断の
  発生源へ戻る経路を作らない。警告は状態変化時か制限した頻度で出す。
- 保持容量、書き込み速度、キュー滞留を別々に見る。WAL や DB のサイズ差だけで
  SSD 書き込み量を判定しない。

GUI の診断保存は `diagnostics.persistence` で `off` / `warnings` / `all` を選ぶ。
既定の `warnings` は warning/error を保存し、info 診断は live 表示に留める。
stderr の `log_level` とは独立した設定で、会話・監査イベントには適用しない。
`all` は調査時にだけ使う。`metrics.enabled = false` は usage 保存も停止する。

```toml
[diagnostics]
persistence = "warnings"

[metrics]
enabled = true
```

GUI 保存では通常の ownership heartbeat を省略する。同じ run の隣接した差分を
最大 100 ms / 32 KiB / 元イベント 256 件でまとめ、意味のあるイベント境界と
終了時には残りを保存する。設定されたイベント上限が32 KiBより小さい場合は
その上限を使い、UTCの日付境界でも分割する。元の event bus には変更を加えない。
`StorageBridge::monitor().snapshot()` は未保存件数・バイト数・最古の待ち時間、
保存・結合・省略・失敗件数をメモリ上で返す。5 秒以上の滞留警告は stderr へ
最大 1 分に 1 回出し、警告自身を同じ保存経路へ流さない。

本番GUIは `OwnedStorageBridge` が停止時の有限snapshotと結合待ちの差分を保存し、
bridgeをjoinしてからSQLiteを閉じる。起動途中の失敗でも同じ順序になる。
所有権は先にquiesceして新規turnを止め、heartbeatによる解放を保留する。
flush barrierで先行保存の完了を確認してから解放し、Released通知も最終排出で保存する。
保存待ちの同一世代は有効に保ち、外部の世代変更による拒否は維持する。
停止要求をbridgeが観測した時点のsnapshotを区切りとし、その後のイベントは
終了待ちを延長しない。プロセス強制終了は正常終了の保証に含まれない。

storage v12はセッション・UTC日ごとのpayloadバイト合計を保持する。
INSERT/UPDATE/DELETEと同じトランザクション内のtriggerで更新し、別writerのcommit後も
履歴payloadを再走査しない。追加保存の集計更新は2ページ分で、WAL予算に含める。
既存DBの初回移行だけは履歴から合計を作るため読み取りが発生する。
通常保存のベンチマークはこの一回の移行費用とは分けて扱う。
`StorageHandle::statistics()`は受理件数・payloadバイト数・失敗・保存時間・usage flushを
メモリ上で返す。統計取得と空のusage flushはDBへ書き込まない。

所有権のGUI表示用照会はreadonly接続を再利用し、繰り返しの接続・スキーマDDLを除く。
世代・leaseの値は毎回DBから読み、実際の変更の検証は新しい接続とtransactionで行う。

## PR ごとの決定的なゲート

```sh
scripts/check-io-contracts.sh
```

`crates/storage/tests/io_contracts.rs` は一時ファイル DB と公開 `StorageHandle`
API を使う。SQLite PROFILE の実 VM 命令数で、128 件と 8,192 件の履歴へ
同じ 96 件を保存した処理量を比較する。二つのセッションと GUI stream を交互に
使い、単一writerと独立した二つのwriterの両方で検証する。
64 倍の履歴で warm 処理が 2 倍を超えたら失敗する。全履歴集計の positive
control で、計測器が走査量の増加を実際に捉えることも確認する。

WAL の検証では、warm-up 前に一時 DB だけを truncate checkpoint し、warm-up 後に
reader の snapshot を保持して WAL の再利用を止める。計測中に追加された実フレーム数を
1 保存あたり 6 ページ + 分割余裕 16 ページ以内に制限する。空の flush と maintenance
を 32 回実行しても WAL フレームが増えないことも検証する。この制御された条件の
WAL 量は決定的な DB 書き込み予算であり、物理 SSD 書き込みの測定ではない。

同じスクリプトで所有権の1,024回の表示照会を256 KiB以内のreadに制限するLinuxテストと、
GUI差分集約・保存上限・日付境界・正常終了・監査記録の回帰テストも実行する。

CI は compile 後にこのスクリプトを実行する。通常の workspace test にも同じ
integration test が含まれる。負荷に依存する elapsed-time の合否閾値は設けない。
予算の緩和は、増加理由と代表 workload の計測値をレビューに添える。

## 大きな履歴での比較

```sh
cargo run --release -p storage --example storage_performance
cargo run --release -p storage --example storage_performance -- \
  --histories 1000,10000,100000,1000000 --appends 1000 --payload-bytes 64
```

各サイズで独立した一時 DB を作り、同じサイズのイベントを同じ件数だけ受理した
ことを検証する。最初の保存は `cold_first_append` として別計測し、二つのセッションと
stream を温めてから `warm_appends_and_checkpoint` を計測する。ここでの cold は
writer の初回であり、OS ページキャッシュが空という意味ではない。

実際の WAL・容量制限を使い、seed と warm 計測の前に checkpoint 条件を揃える。
warm-up 後は writer 自身で checkpoint する。他接続による WAL のリセットは
`data_version` を変えて意図した再集計を発生させ得るため、warm 計測に混ぜない。
warm の計測には最後の checkpoint も含める。履歴の seed、DB open、結果の JSON 出力
は保存時間に含めない。通常の上限を超える workload は拒否し、制限を無効化して
ベンチマークだけを成功させない。利用中の DB やシステムのキャッシュは操作しない。

JSON Lines に elapsed、CPU 時間、Linux `/proc/self/io` の差分と毎秒の値を出す。
CPU 時間は OS の clock tick 精度なので短い処理では 0 になり得る。取得できない
カウンターは `null`。`getconf CLK_TCK` がない場合も CPU 秒数は `null` になる。
プロセス全体の値には計測用 `/proc` 読み取りの小さな負担も含まれる。
同じビルド・設定・workload で比較し、`write_bytes` もファイルシステムや writeback
条件に左右される診断値として扱う。大規模計測は毎 PR ではなく保存経路変更時と
リリース前に行う。

### 2026-10-01の検証例

最終版のreleaseビルドで、64バイトの差分を300件保存しcheckpointまで行った値。
時間とOSカウンターはこの環境の参考値で、CIの合否条件にはしない。

| 既存履歴 | 保存時間 | 論理read (`rchar`) | OS `write_bytes` |
| ---: | ---: | ---: | ---: |
| 1千件 | 24.4 ms | 226,085 | 7,794,688 |
| 1万件 | 22.3 ms | 262,959 | 7,716,864 |
| 10万件 | 36.0 ms | 291,639 | 7,864,320 |
| 100万件 | 22.9 ms | 275,265 | 7,815,168 |

複数セッションを再集計していた段階の10万件・同じ300件保存は2.999秒、論理readは
約6.445GBだった。最終版は履歴件数に依存しない通常保存になった。
authoritativeな容量集計のため、保存1件あたり2ページの更新は追加される。
GUIの512差分を2行にまとめるテスト、heartbeat省略、空flushの無書き込みを合わせて、
発生件数と1件あたりの費用をそれぞれ検証する。

決定的ゲートでは128/8,192件の履歴に96件保存したVM命令数が単一writerで
17,088/17,088、二つのwriterで20,128/20,128。WALは560/589フレームで、
従来の上限592フレームを維持した。

## 実行中 GUI の計測

```sh
uv run --no-project python scripts/measure-io.py --pid 12345 --duration 60
uv run --no-project python scripts/measure-io.py --pid 12345 --duration 300 --interval 5 --samples
# storage-writerのTIDを選び、アプリ自身の保存I/Oを切り分ける
uv run --no-project python scripts/measure-io.py --pid 12345 --tid 12399 --duration 60
```

対象 PID を `/proc` から読み取り、JSON を標準出力へ出す。既定でログファイルを
作らず、DB へメトリクスを保存しない。CPUは全threadを含み、子プロセスのCPUを含まない。
プロセスI/Oは全threadに加え、終了してwaitで回収された子プロセスの累計も含む。
まだ実行中の子プロセスは別PIDで調べる。子の終了時に過去のI/Oがまとめて加算されるため、
その区間の毎秒値をGUI保存の瞬間速度と解釈しない。
`--tid`は特定threadだけを測り、子プロセスや他threadのI/Oを含めない。
PID/TID が再利用された場合は集計を止める。途中終了時の summary は最後に取得できた
sample までであり、終了直前の未観測 I/O は含まれない。権限不足や欠損は `null` と
`unavailable_fields` / `io_error` に出る。

計測ツール自体の回帰テストは以下で実行できる。PID/TIDの取り違え、取得不能の
ゼロ扱い、子プロセス回収前後の帰属を検証する。

```sh
PYTHONDONTWRITEBYTECODE=1 uv run --no-project python -m unittest discover \
  -s scripts/tests -p test_measure_io.py -v
```

| 値 | 読み方 |
| --- | --- |
| `rchar` / `wchar` | read/write 系システムコールで扱ったバイト。ページキャッシュや端末・pipe も含む |
| `read_bytes` / `write_bytes` | カーネルがプロセスに帰属させる storage I/O。SSD 内部の書き込み増幅量ではない |
| `syscr` / `syscw` | read/write 系システムコール回数 |
| `cpu_percent_one_core` | 1 コアを 100% とする CPU。複数 thread で 100% を超え得る |

定義は [Linux procfs の公式資料](https://docs.kernel.org/filesystems/proc.html#proc-pid-io-display-the-io-accounting-fields)
と、子プロセス回収時の集計を行う[Linux kernelの実装](https://github.com/torvalds/linux/blob/v6.18/kernel/exit.c)
を参照。子が1MiBをfsyncして終了する実験でも、親の`wchar`と`write_bytes`にそれぞれ
1,048,576が加算された。130GiBの内訳を判断する際も、終了済みのビルド等を考慮する。
待機、長い streaming、ツール連続実行、複数ウィンドウを同じ時間ずつ測り、
CPU・各 I/O の毎秒値と保存キュー件数・待ち時間を合わせて確認する。待機で継続した
書き込みが出た場合は、保存元を特定して更新契機を修正する。

### 2026-10-02: Wayland の再描画待ちによる高 CPU

修正前の実行中 GUI（eframe 0.36.1）では、メイン thread が 1 コア基準で CPU
99.7%、毎秒約 55.2 万回の read 系 syscall を消費していた。同区間の
`read_bytes` / `write_bytes` は増加していない。5 秒間の userspace sampling では
`polling::Poller::wait_impl` が 75.07% を占め、Wayland のイベント待機経路に
集中していた。フォーカス外で悪化するという観察とも整合するが、累積 195 GiB の
書き込みをこの待機ループへ帰属させる根拠にはならない。

[eframe の修正 #8398](https://github.com/emilk/egui/pull/8398) は、再描画要求時に
`ControlFlow::Poll` へ切り替え、予定した描画がなくなると待機へ戻らない不具合を
修正している。Wayland の compositor から描画通知が来るまでの間にもループが
回り続けるため、非表示・非アクティブ時の描画抑制で顕在化し得る。
[0.36.2 の changelog](https://github.com/emilk/egui/blob/0.36.2/crates/eframe/CHANGELOG.md)
と配布ソースの `src/native/run.rs::check_redraw_requests` で、再描画時の `Poll`
を除き、必ず `WaitUntil` または `Wait` に戻す修正を確認した。
workspace の最低バージョンを 0.36.2 に上げ、lockfile も更新した。
evorch 側の 200 ms ごとの再描画予約は毎秒 5 回の更新契機であり、この待機なしの
連続ループとは分けて評価する。

依存更新や描画スケジュール変更時は、同じ Wayland 環境・設定・workload で以下を
確認する。上記は修正前の計測値であり、依存更新だけで改善後の実測値とはしない。

1. 更新した lockfile から起動したバイナリを使い、起動直後の読み込みが落ち着いて
   から前面・フォーカス外・別ウィンドウで覆った状態をそれぞれ 30〜60 秒測る。
2. `measure-io.py --tid` でメイン thread の CPU、`syscr`、storage I/O の差分を
   比較する。CPU が 1 コアを占有し、read 系 syscall が連続増加する再発を検出する。
3. フォーカスを戻し、入力・再描画・streaming 中の更新が応答することも確認する。
   CPU だけを下げて画面更新を止める変更は合格にしない。
4. 通常の offscreen 描画テストだけでは compositor の描画通知の遅延を再現できない。
   必要な profiler は対象 PID と数秒の採取時間を指定し、全 filesystem の探索や
   無期限の syscall ログ出力を避ける。

CI の offscreen ジョブは `scripts/check-gui-wayland-idle.sh` も実行する。
専用の headless Weston と、実際の Workbench を使う `native_qa_window` を起動する。
`--fake-seat` を提供する Weston では desktop shell と組み合わせて使う。
このオプションがない Ubuntu 24.04 の Weston 13.0.0 では、headless desktop shell が
初回描画時にクラッシュするため kiosk shell を使う。kiosk shell は最小化を実装せず、
最小化要求後もウィンドウを表示する。
初回描画後に専用 compositor だけを停止して描画通知を保留する。5 秒間の
メイン thread の read syscall を毎秒 5,000 回未満に制限し、compositor を再開した
後に UI が進むことも確認する。続けて最小化要求後の 8 秒間も同じ予算で検査する。
後半のシナリオ名は `minimize-requested` とし、最小化完了の通知は検証しない。
主シナリオは shell の最小化実装に依存しない描画通知の保留である。
CPU は参考値として保存し、負荷に左右される百分率の
合否閾値は設けない。プロセスの開始時刻、欠損カウンター、途中終了も検査する。

2026-10-02 に同じ QA コード・専用 Weston 16・ソフトウェア Vulkan で比較した。
旧版をリンクした positive control は同じゲートの syscall 予算で失敗した。

| eframe | 描画通知を保留した 5 秒間のメイン CPU | read syscall / 秒 | ゲート |
| --- | ---: | ---: | --- |
| 0.36.1 | 99.6% | 253,439 | 不合格 |
| 0.36.2 | 計測上 0% | 0 | 合格 |

0.36.2 は最小化要求後の 8 秒間も CPU・read syscall ともに計測上 0 で、
描画通知再開後の UI の進行を確認した。CPU の 0 は OS clock tick の計測精度内の
値であり、全 thread の実行やすべての workload の CPU がゼロという意味ではない。

Ubuntu 24.04 の Weston 13.0.0 + kiosk shell でも同じゲートを比較した。
描画通知保留中は 0.36.1 が CPU 97.4%・read syscall 毎秒 217,942 回で不合格、
0.36.2 が計測上 0%・0 回で合格だった。後半の最小化要求後は表示が続くため、
0.36.2 でも CPU 10.1%・read syscall 毎秒 198 回を計測した。
