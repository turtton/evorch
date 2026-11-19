# renderer helper の実測（ADR 0045 段階 16a）

2026-11 時点。ADR 0045 §3 は、CEF/browser を「オンデマンドで起動される
renderer helper」としてコア外へ分離する。この文書は 16a-2（helper をコアへ
組み込み、Admission → SpawnLedger に接続する PR）の設計に必要な数値を集める。
2 つの計測を載せる。計測の窓と startup の手順は PR #673 の後で直した（§1）。
修正後の手順で測り直した baseline は §5.5。§3〜§5.4 の表の値は #673 の旧手順の
もので、§3 の startup だけ修正後の手順で置き換えた。

1. **spike（16a-1 の前段）**: 試作 harness による方式選択（Unix socket か共有
   メモリか）の計測。元データは main に無かったので、結果と判断の要点だけを
   §2 に残す。
2. **実バイナリの計測（この PR）**: 出荷する `evorch-renderer` を
   `RendererClient` 経由で測る。手順は `scripts/measure-renderer.sh`
   （§1）。結果は §3〜§5、16a-2 への提案は §6。

## 0. 計測の定義と出典

| 項目 | 出典 |
|---|---|
| display なしで CEF が初期化でき、sandbox と network 制限を緩めない | Planner 計画 §5.3「PR16a-1：実測」。必須の採否基準 |
| GUI 内 CEF との同時稼働（profile/cache を分離し、singleton・port・lock が衝突しない） | 同上。必須の採否基準。spike では off-screen の GUI harness で確認、**native GUI は 16b で測る**（§7） |
| 1280×720 のフレーム転送。1/10/30fps で p50/p95、CPU、RSS、copy 量、drop 数、queue 長 | 同上 |
| queue は上限付きでメモリが増え続けない／30fps で転送 p95 が 33ms 以内／socket が満たせば socket を優先 | 同上 |
| 実バイナリ・`RendererClient` 経由で、起動 latency、RSS/PSS、CEF プロセス数、転送 p50/p95、終了時間、多 tab、4K | 16a の計測範囲として Orchestrator が確認（question-ed089f47f757fd84d6305a1e8a939f64、案 A） |
| #610 が「未実施」とした項目: native GUI 共存、production core からの spawn、4K、複数 tab、長時間 soak、macOS/Windows | #610 の PR 本文 |

## 1. 再現手順

```sh
# dev shell 内で（CEF_PATH / LD_LIBRARY_PATH / LIBCLANG_PATH が要る）。
# sandbox からは .agents/skills/sandbox-build/scripts/with-dev-env.sh 経由。
scripts/measure-renderer.sh --out /path/to/results            # 全部（約 15 分）
scripts/measure-renderer.sh --quick startup transfer          # 短縮版（窓 6 秒）
scripts/measure-renderer.sh --check                           # ツール自身の fmt/clippy/test
```

- 出力は `<out>/{startup,tabs,stream,transfer,ipc}.jsonl`、`environment.json`、
  `build.log`。
- ビルド: `evorch-renderer` と `evorch-cef-helper` を checkout の cargo でビルド
  （dev profile）。計測ツール `scripts/measure-renderer/` は workspace に属さない
  単独 package（自前の `[workspace]` と `Cargo.lock`）で、`renderer-ipc` を path
  依存にし release でビルドする。root の `Cargo.toml`・CI・flake は変えない。
- **ツール自身の検証は `scripts/measure-renderer.sh --check`**（`cargo fmt
  --check`、`clippy -D warnings`、unit test）。単独 package なので lefthook
  （`cargo fmt --all` / `clippy --workspace`）と CI の workspace ジョブの対象外で、
  このコマンドを手で実行する。unit test は統計（percentile、先頭 frame の除外、
  欠落 seq）、画素の時刻 stamp、`/proc` の parse、proxy の転送、合成 backend の
  窓の数え方を見る。実 helper を起動する計測そのものは CI では動かさない。
- 計測ツールは `renderer_ipc::stdio_channels()` と `RendererClient::connect` で本物の
  helper を起動する。起動規約は `crates/evorch-renderer/tests/real_helper.rs`
  と同じ。helper は自分の process group を持ち、ツールは child subreaper になる。
  `DISPLAY`/`WAYLAND_DISPLAY`/`XDG_SESSION_TYPE` を消す。`TMPDIR=/proc/self/cwd`
  にして cwd を run ごとの短い dir にする（Chromium の SingletonSocket が AF_UNIX
  の `sun_path` 上限を超えると SIGTRAP になるため）。CEF の sandbox は有効のまま。
- 同期は `wait_frame`/`shutdown`/`waitid`/channel と `Condvar` の条件待ちだけ。
  sleep で完了を待たない。時間を使うのは次の 3 つで、いずれも負荷の形か
  サンプリング間隔であって同期ではない。
  - 合成 producer のフレーム間隔（`park_timeout` で fps を保つ。完了待ちは
    `Condvar`）。
  - slow consumer の保持時間（`hold_ms`）。
  - メモリ sampler の 2 秒間隔（専用 thread の `recv_timeout`。同時に停止信号）。
    sampler は frame の consumer と別 thread なので、`/proc` の走査が配送を
    止めない。
- scenario:
  - `startup`: **spawn → 最初のフレーム**を、spawn の直前に取った 1 つの
    `Instant` から最初のフレームを手にするまでの経過時間として測る（途中で
    `/proc` を読まない）。停止時間（`Shutdown` と process group への SIGKILL）と
    停止後の残留 process も測る。各 run は新しい helper process と新しい
    profile dir で始める（process は使い回さない）。timing 試行は spawn から最初の
    フレームまでの間に何も読まず、メモリは最初のフレームの後に 1 回だけ読む
    （その読み取りはどの latency 区間にも入らない。所要時間は
    `after_first_frame_read_ms`）。別の試行（`mem=1`）は、さらに `Hello` の後と
    `open_artifact` の後にもメモリを読む。この 2 回は spawn → 最初のフレームの
    区間の内側にあるので、その試行の spawn → 最初のフレームは読み取りの分だけ
    伸びる（`mode` に印を付ける）。読み取りの合計は
    `memory_read_before_first_frame_ms`、それを引いた値は
    `spawn_to_first_frame_excluding_memory_reads_ms`。`open_artifact_ms` は
    その呼び出し自身の区間で、どちらの試行でも読み取りを含まない。
  - `tabs`: tab を 1〜4 個開いたときの process 数・RSS・PSS・USS、5 個目の拒否、
    全 tab を閉じた後の残り。
  - `stream`: 実 helper。毎フレーム色が変わり、時刻を画素に埋め込む canvas
    （`fixture.rs`）を描き、`wait_frame` で受ける。配送 fps、到着間隔、
    ページ時刻から client までの age、drop、helper tree の CPU、メモリ。
  - `transfer`: `stream` と同じ負荷で、helper と client の frame channel の間に
    pass-through の proxy（`proxy.rs`）を挟む。実 helper の sender が出す
    1 フレームについて、最初の byte が読める時刻、全 byte を読み終えた時刻
    （`wire`）、client へ書き終えるまで（`forward`）、client が手にするまでを
    測る。**実 helper → client の転送 p50/p95 はここで出す**。ただし proxy は
    受信側で観測するので、これは「proxy が最初の byte を読んでから client が
    フレームを手にするまで」の 2 hop の区間である。helper が paint を始めた時刻、
    GUI が表示し終えた時刻は含まない（後者は 16b で測る）。
  - `ipc`: helper の代わりに同じ process 内の合成 backend が `FrameQueue::publish`
    でフレームを出す。CEF を除いた IPC（queue→送信→socket→client reader）だけの
    latency。先頭 8 byte の publish 時刻から測る。`fps=0` は上限まで出す飽和
    試験、`hold_ms` は遅い consumer。
- 窓: `stream`/`transfer`/`ipc` は先頭 10 フレーム（warm-up）を捨て、次に受けた
  1 フレーム（基準）の到着時刻を窓の開始とする。窓は `stats::Span` の半開区間
  `[開始, 開始 + 20 秒)` で、開始と終了は同じ `Instant`（単調時計）の基準に
  ある。
  - `wait_frame` は窓の残り時間を上限に待つので、期限を過ぎて待ち続けない。
  - 受信した直後にも到着時刻を窓と照らし、期限後に着いたフレームは数えずに
    捨てる（JSON の `late_frame_discarded`）。
  - 基準のフレームは数えず（`stats::Window`）、次のフレームから fps・drop・欠落に
    数える。
  - proxy の sample は、転送の開始と終了の両方が窓の内側にあるものだけを使う
    （`Sample::within`）。終了は `write_message` が返った直後に取った時刻で、読み取りと
    書き込みの間の処理（meta の decode、stamp、lock）も転送に含める。窓をまたぐ転送は
    入れない。client 側の「最初の byte → client」も、最初の byte の時刻が窓の内側に
    あるものだけを sample にする（窓の前に始まった転送は途中の観測）。
  - process の読み取り（helper tree の CPU とメモリ、client の CPU）は、窓の
    開始の直後と、期限の直後に行う。CPU% は 2 回の読み取りの間の時間
    （JSON の `cpu_reading_secs`）で割る。名目の 20 秒ではない。
  - 窓の内に 1 フレームも無ければ失敗にする。
  - 境界の直前・直後の扱いは `stats.rs` と `proxy.rs` の unit test で見る。
- 数値の読み方: RSS は共有ページ（libcef）を process ごとに数える。**PSS が
  足し算できる値**で、USS はその process 専用分。CPU% は 1 論理 CPU = 100%。
  role は `--type=` で分類。`zygote` は zygote とそれが fork した renderer/GPU
  process の合計（cmdline が zygote のままになる）。

### 環境

| | |
|---|---|
| CPU | AMD Ryzen 9 7900（12C/24T）、RAM 64GiB |
| OS | Linux 7.3.1-cachyos（NixOS）、display なし |
| CEF | cef-minimal 154.0.34 |
| toolchain | cargo（turtton fork）、rustc 1.99.0 |
| ビルド | helper: dev profile（CEF 本体は prebuilt、Rust 側は薄い）。計測ツール: release |
| 負荷 | 共有 workstation（#673 の全量実行の開始時 loadavg 2.8〜4.3。2026-11-18 の再測定は 5.0〜9.4）。専用機ではない |
| 試行 | 各 scenario 1 回（`stream`/`transfer`/`ipc` は 20 秒窓）。startup の timing は 8 回、kill 3 回、memory 3 回 |

数値は 1 回の測定で、誤差は載せていない。向きを見る材料として読むこと。

## 2. spike の結果（要点）

条件: dev profile、共有 workstation、CEF 154.0.34、Ryzen 9 7900。試作の
harness で、CEF のペイント観測点から受信側の owned Vec までを測った（clone・
queue 滞留・転送・受信 copy を含み、HTML 描画と GPU upload は含まない）。

- display なしで初期化・HTML 描画・終了が成立した。sandbox の無効化は不要。
  warm 初期化は p50 90.2ms（83.6–129.5ms）。長い `TMPDIR` は SingletonSocket が
  `sun_path` 上限を超えて SIGTRAP になる。回避は `TMPDIR=/proc/self/cwd`。
- 1280×720 の 1 フレームは 3,686,400B（3.5MiB）。30fps の名目 payload は 105MiB/s。
- 転送（30 秒×3 回）:

  | 方式 | fps | p50 / p95 ms |
  |---|---|---|
  | socket | 1 | 0.886 / 1.484 |
  | socket | 10 | 0.932 / 3.052 |
  | socket | 30 | 0.841 / 1.359 |
  | 共有メモリ | 30 | 0.597 / 0.831 |

  30fps で CEF 群 CPU は socket が 9.27%、受信側が 2.45%。CEF 群の peak RSS は
  779–796MiB。
- 30fps×120 秒で socket は 3,574 フレーム、drop 0、p95 5.9ms、max 21.5ms。
  CEF 群 RSS は初期 peak から減った。
- 受信側を 200ms 止めた場合は 874 生成 / 146 送信 / 728 drop で、queue は有界の
  まま。
- GUI 側の CEF（off-screen の egui harness）と別 profile の helper を同時に
  30 秒動かし、singleton/profile/cache の衝突も TCP listen port も無かった。
  native GUI は未実施。
- 結論: 暫定基準を満たすので **Unix socket + raw BGRA** を採る。共有メモリは
  0.2〜0.3ms 速いだけで基準を超える必要が無く、先行導入しない（#610 の判断）。
  この結論は spike の値だった。出荷する helper での確認は §5.2。

## 3. 起動と終了（`startup`）

修正後の手順（§1 の `startup`）で 2026-11-18 に測り直した値（最終の全量実行。旧手順の
#673 の値は下の注）。timing 8 回。各 run は新しい helper process で、spawn の直前から
最初のフレームまでを 1 つの `Instant` で測る。メモリの読み取りは含まない。

| 区間 | 値（ms） |
|---|---|
| `spawn` の呼び出し | 0.1〜0.2（`spawn_call_ms`） |
| spawn → `Hello` 応答（CEF 初期化を含む） | 109.5–214.5（中央値 114.9） |
| `open_artifact`（1280×720、30fps、1 tab 目） | 119.9–126.6（7 回）、190.3（1 回） |
| `set_streaming` → 最初のフレーム | 1.3–3.2 |
| **spawn → 最初のフレーム（1 つの `Instant`）** | **232.9–342.9（中央値 238.7）** |

- spawn → 最初のフレームは 6 回が 232.9–242.1ms、残り 2 回が 312.1 / 342.9ms。遅い
  2 回は、1 回が `Hello`（214.5ms。ほかは 109.5–116 台）、もう 1 回が
  `open_artifact`（190.3ms）の遅れに当たる（原因は未調査）。kill 試験の 3 回は
  235.6 / 237.0 / 241.0ms で、速い側の範囲。
- 旧手順（#673）の値は spawn → 最初のフレーム 226.8–294.3ms（中央値 234.5）、
  `Hello` 104.8–114.8ms、`open_artifact` 118〜120ms（5 回）と 184〜186ms
  （3 回）。範囲は同じだが、別の測定なので値は付け替えていない。
- 上の値は warm（libcef がページキャッシュにある状態）。ビルド直後の smoke 実行
  で、最初の 1 回だけ `Hello` まで 905ms かかったのを見た（再現手順には含めて
  いない。キャッシュを落とせないため）。cold では最初の起動が ~1 秒に伸びる
  ものとして扱う。
- 停止（shutdown 8 回）:
  - `RendererClient::shutdown` の復帰は 2.4–4.0ms（中央値 2.7）。これは `Shutdown` の
    ack に加え、2 本の channel の close と reader thread の終了まで待った時間で、
    ack だけの時間ではない（値の名前は `shutdown_returned_ms`）。
  - helper の終了は 25.6–39.4ms（中央値 27.6）、CEF の子孫 3 process の reap までで
    29.7–42.7ms（中央値 31.9）。
- process group への SIGKILL（3 回）は終了 7.7–9.2ms、子孫 5 process の reap まで
  7.9–9.4ms で、subreaper に戻る。どちらの停止でも、残留 process は 0。
- 起動直後（tab なし、`tabs` の 0 tab）: **8 process**（helper 1、utility 1、
  zygote 系 6）、**PSS 267MiB**（RSS 634–638MiB、USS 141〜142MiB）。
- 1 tab 開き、最初のフレームを受けた後: 10 process、PSS 359–362MiB（memory 試行）。
  timing 試行の最初のフレームの後の値は PSS 364–369MiB（8 回、RSS 941–961MiB）。
- PSS は同じコードの 1 つ前の全量実行（同日、PSS 241MiB / 331MiB 前後）より、同じ
  状態で約 26MiB 高い（起動直後 241→267MiB、1 tab 後 330→362MiB）。helper の
  RSS（160→186MiB）も上がっていて、再現手順が同じ build を使ったのに差が出た。
  PSS は共有ページ（libcef）を同時に動いている process と分け合うので、共有
  workstation の状態で動く可能性がある。原因は調べていない。この文書の PSS は、
  この実行の値で揃え、実行間で 30MiB 程度動くものとして読む。
- memory 試行（`mem=1`、3 回）: `Hello` の後に PSS 267MiB（8 process）、
  `open_artifact` の後に 363–366MiB、最初のフレームの後に 359–362MiB（10 process）。
  `Hello` の後と `open_artifact` の後の読み取りが spawn → 最初のフレームの内側
  にあるので、この試行の spawn → 最初のフレームは 245 / 271 / 336ms と
  timing 試行より長い（読み取りの合計 35–40ms を引くと 205 / 236 / 300ms）。この
  3 回の値は latency としては使わない。`open_artifact_ms`（その呼び出し自身の区間）は
  88 / 120 / 186ms。

## 4. tab と解像度（`tabs`）

§5.5 と同じ最終の全量実行（2026-11-18）の値。`MAX_TABS = 4`。5 個目は `Busy` で
拒否された（どちらの解像度でも）。

| viewport | tab 数 | process | PSS MiB | RSS MiB | USS MiB | `open_artifact` ms |
|---|---|---|---|---|---|---|
| 1280×720 | 0 | 8 | 267 | 638 | 142 | – |
| | 1 | 10 | 362 | 950 | 175 | 120 |
| | 2 | 11 | 395 | 1133 | 178 | 118 |
| | 3 | 12 | 428 | 1318 | 196 | 150 |
| | 4 | 13 | 457 | 1496 | 215 | 117 |
| | 全部閉じた後 | 8 | 300 | 690 | 165 | – |
| 3840×2160 | 0 | 8 | 267 | 634 | 142 | – |
| | 1 | 10 | 528 | 1227 | 224 | 269 |
| | 2 | 11 | 841 | 1791 | 407 | 258 |
| | 3 | 12 | 1091 | 2353 | 482 | 282 |
| | 4 | 13 | 1351 | 2937 | 557 | 314 |
| | 全部閉じた後 | 8 | **765** | – | – | – |

- 1 tab 目で +2 process（GPU/renderer）、以降は +1 process/tab。
- PSS は 2 tab 目以降で tab あたり約 30MiB/Mpix（720p は +29〜33MiB、4K は
  +250〜313MiB）。1 tab 目はそれに GPU/renderer の分が乗り、720p で +95MiB、4K で
  +261MiB。
- 4K の tab を閉じても PSS は戻りきらない（765MiB。起動直後は 267MiB）。720p でも
  300MiB 残り、起動直後より 33MiB 高い。
- 起動直後の PSS は、同日の 1 つ前の全量実行（240MiB）より 27MiB 高い（§3 の注）。
  この表の差分（tab あたりの増分）は 2 回でほぼ同じ（720p の 2〜4 tab 目は
  +29〜33MiB と +29〜33MiB）で、絶対値だけが動いた。

## 5. フレーム配送

### 5.1 実 helper（`stream`、20 秒窓）

（#673 の旧手順の値。修正後の手順で測り直した値は §5.5。）

age = 画素に埋めたページ時刻から client がフレームを手にするまで。**転送の
時間ではない**。ページの描画、CEF の paint、frame の copy、queue 滞留を含み、
さらにページの描画 timer と helper の paint の周期が独立なので、位相次第で
0〜1 フレーム間隔（30fps なら 0〜33ms）が乗る。同じ設定でも run によって 1.4ms
にも 35ms にもなる（`stream` の 720p×30fps で 1.4ms、`transfer` の同条件で
35.0ms）。転送そのものは §5.2。

| viewport | tab | 要求 fps | 配送 fps | 到着間隔 p50/p95 ms | age p50/p95 ms | drop（lost） | CPU% helper tree / client | PSS 窓の開始→peak MiB |
|---|---|---|---|---|---|---|---|---|
| 1280×720 | 1 | 1 | 1.0 | 1000 / 1000 | 1.9 / 2.3 | 0 | 0.4 / 1.0 | 328 → 331 |
| | 1 | 10 | 10.0 | 100 / 100.4 | 101.5 / 101.8 | 0 | 3.2 / 1.5 | 331 → 338 |
| | 1 | 30 | 30.0 | 33.3 / 33.7 | 1.4 / 1.8 | 0 | 6.7 / 2.4 | 327 → 335 |
| | 1 | 60 | 60.1 | 16.7 / 16.9 | 17.7 / 17.9 | 0 | 13.2 / 3.6 | 330 → 339 |
| | 4 | 10 | 40.0 | 15.9 / 53.7 | 1.7 / 101.4 | 0 | 11.0 / 2.9 | 433 → 449 |
| | 4 | 30 | 120.0 | 1.7 / 16.0 | 2.2 / 35.0 | 2（lost 2） | 36.0 / 7.1 | 422 → 440 |
| 3840×2160 | 1 | 30 | 30.0（950MiB/s） | 33.3 / 34.1 | 82.7 / 83.4 | 0 | 74.8 / 13.0 | 560 → 569 |
| | 4 | 30 | **83.8**（要求 120） | 11.8 / 13.8 | 144 / 152 | 3（lost 4） | 230.3 / 35.4 | 1403 → 1436 |

- 1280×720 では、60fps まで要求どおりに届き、drop は 0。到着間隔の p95 は
  30fps で 33.7ms。
- 1fps では、ページの描画自体が 1 秒間隔になる。1fps で開く `open_artifact` は
  **3.0 秒**かかった（最初の描画が 1fps の上限に待たされる。10fps で 0.42 秒、
  30fps で 0.12 秒）。
- 4K×1 tab×30fps は配送できる。helper tree の CPU は 1 論理 CPU の約 75%
  （software 描画）。age 83ms の大半は 4K 描画と copy で、転送は §5.2 のとおり
  8.6ms（p95）以下。
- 4K×4 tab×30fps は配送が飽和する（83.8fps、helper 230%）。IPC の上限ではなく
  CEF の描画側が頭打ち（§5.3 の合成試験は同じ負荷を 120fps で流せる）。
- 複数 tab のとき、client の frame slot は 1 つなので、tab 間でフレームが
  上書きされる。4 tab×720p×30fps で 2/2,400（0.08%）が落ちた（以前の試行では
  0.8%）。
- メモリは 20 秒の間、増え続けなかった（PSS の peak が開始値より 8〜16MiB 高い
  程度。4K×4 は +33MiB）。client 側 RSS は 10MiB（4K で 67MiB）で一定。
- 停止は全ケースで `shutdown` の復帰まで 0.2–0.7ms、残留 0。

### 5.2 実 helper → client の転送（`transfer`、20 秒窓）

`stream` と同じ実 helper と負荷だが、frame channel に proxy を挟む。proxy は
helper の socket から 1 メッセージを読み、client の socket へそのまま書き、
client の ack も helper へ戻す。helper の「ack まで 1 フレームだけ送信中」という
規則は変わらない。

- `wire`: 最初の byte が読めてから、メッセージ全体を読み終えるまで。helper の
  実際の sender が 1 フレームを socket へ書き出す時間。
- `forward`: proxy が client へ書き終えるまで。
- 最初の byte → client: 最初の byte が読めてから、client の `wait_frame` が
  フレームを返すまで。`wire` + `forward` + client の reader の受信。socket の
  hop を 2 回通るので、helper と client が直結なら概ね `wire` に近い値になる。
  **これは受信側で観測した 2 hop の区間である。** helper の paint の開始、
  GUI が表示し終えるまでの latency とは別の量で、それらは含まない。

（#673 の旧手順の値。修正後の手順で測り直した値は §5.5。）

| viewport | tab | 要求 fps | 配送 fps | フレーム数 | wire p50/p95/p99 ms | forward p50/p95 ms | 最初の byte → client p50/p95/p99 ms | drop（lost） |
|---|---|---|---|---|---|---|---|---|
| 1280×720 | 1 | 10 | 10.0 | 201 | 0.53 / 0.60 / 0.79 | 0.44 / 0.53 | 1.00 / 1.13 / 1.32 | 0 |
| | 1 | 30 | 30.0 | 601 | 0.49 / 0.60 / 0.74 | 0.45 / 0.56 | **0.96 / 1.16 / 1.38** | 0 |
| | 1 | 60 | 60.1 | 1202 | 0.44 / 0.53 / 0.69 | 0.40 / 0.50 | 0.86 / 1.04 / 1.27 | 0 |
| | 4 | 30 | 120.0 | 2404 | 0.49 / 0.70 / 0.80 | 0.44 / 0.64 | 0.98 / 1.31 / 1.51 | 2（lost 1） |
| 320×180 | 1 | 30 | 30.0 | 601 | 0.03 / 0.05 / 0.07 | 0.02 / 0.05 | 0.08 / 0.12 / 0.14 | 0 |
| 3840×2160 | 1 | 30 | 30.0 | 601 | 4.17 / 4.97 / 5.40 | 3.52 / 4.10 | **7.79 / 8.64 / 9.59** | 0 |
| | 4 | 30 | 76.9 | 1540 | 3.96 / 5.14 / 6.24 | 5.07 / 6.67 | 9.09 / 11.43 / 12.52 | 2（lost 3） |

- **基準（30fps で転送 p95 が 33ms 以内）は、出荷する helper でも満たす。**
  720p で p95 1.2ms、4K で 8.6ms（どちらも proxy を挟んだ 2 hop 分の上限）。
  4 tab×720p でも p95 1.3ms。
- 4K の 1 フレーム（33MiB）の `wire` は p50 4.2ms で、約 7.6GiB/s。
- proxy を挟む分、helper の ack の待ちが 1 hop 増える。4K×4 tab は `stream`
  （83.8fps）より少し低い 76.9fps になった。この行の fps は飽和側の値で、proxy の
  影響を含む。
- `page_to_first_byte`（ページの時刻 → helper が送り始める）は、720p×30fps で
  p50 34.1ms、4K×30fps で 45.1ms など、§5.1 の age とほぼ同じ。転送に入る前の
  時間（描画・paint・copy・queue 滞留と、上に書いた位相）で、helper 内部の
  OnPaint の時刻を取っていないため、これ以上は分けられない（§7）。

### 5.3 IPC だけ（`ipc`、合成 producer、同一 process）

publish → client が読める時点まで（CEF は含まない）。窓の先頭フレームは
数えない。

| viewport | tab | 要求 fps | 配送 fps | MiB/s | latency p50/p95/p99 ms | drop（lost） | process CPU% | process RSS peak MiB |
|---|---|---|---|---|---|---|---|---|
| 1280×720 | 1 | 1 | 1.0 | 3.5 | 0.73 / 0.78 / 0.86 | 0 | 0.2 | 17 |
| | 1 | 10 | 10.0 | 35 | 0.59 / 0.74 / 0.83 | 0 | 0.9 | 17 |
| | 1 | 30 | 30.0 | 105 | **0.50 / 0.73 / 0.94** | 0 | 2.6 | 17 |
| | 1 | 60 | 60.0 | 211 | 0.46 / 0.64 / 0.76 | 0 | 4.6 | 17 |
| | 4 | 30 | 120.0 | 422 | 1.2 / 1.6 / 1.8 | 0 | 12.8 | 39 |
| | 1 | 飽和 | 3,234 | **11,369** | 0.38 / 0.53 / 0.74 | 1（lost 259,868） | 277 | 26 |
| 3840×2160 | 1 | 30 | 30.0 | 949 | **5.6 / 6.1 / 6.4** | 0 | 26 | 130 |
| | 4 | 30 | 120.0 | 3,796 | 9.3 / 11.4 / 12.0 | 0 | 127 | 320 |
| | 1 | 飽和 | 234 | 7,417 | 7.6 / 9.2 / 9.9 | 0（lost 4,205） | 287 | 193 |

- 30fps の転送 p95 は 720p で 0.73ms、4K で 6.1ms。§5.2 の実 helper（2 hop の
  proxy 込みで 1.2ms / 8.6ms）と同じ桁で、IPC の下限がこの値。socket は 1 本で
  11GiB/s（720p）まで出る。
- 4K×4 tab×30fps（3.7GiB/s）も 120fps で流せる（p95 11.4ms）。実 helper の
  4K×4 tab の飽和（§5.1）は IPC ではなく CEF の描画が上限だという根拠。
- 飽和側の `lost`（queue が新しいフレームに置き換えた分）が大量に出る。queue が
  詰まらずに新しい方へ置き換わることを示す。

### 5.4 遅い consumer（`ipc`、`hold_ms=200`、60fps 要求）

| viewport | 配送 fps | queue による置き換え（lost / client drop） | process RSS（start → peak） |
|---|---|---|---|
| 1280×720 | 4.99 | 1,089 / 1,100 | 3.5 → 17MiB |
| 3840×2160 | 4.99 | 1,089 / 1,100 | 3.5 → 130MiB |

consumer が 200ms 止まっても、producer は止まらず、古いフレームが捨てられる。
メモリは頭打ち（4K で 130MiB は 33MiB のフレームバッファ数枚分）。基準の
「queue は上限付きでメモリが増え続けない」を満たす。

### 5.5 baseline の再測定（修正後の手順、2026-11-18）

§1 の窓（開始と終了を同じ単調時計に置き、残り時間で待ち、受信後に期限を再確認し、
proxy sample と `first byte → client` の sample を、転送の開始と終了（proxy は
`write_message` が返った直後の時刻）が窓の内にあるものだけにする）で、
`scripts/measure-renderer.sh` を全量（既定の 20 秒窓）で測り直した。環境は §1 と
同じ（Ryzen 9 7900、cef-minimal 154.0.34、dev profile の helper、display なし）。
共有 workstation で、各 scenario の開始時 loadavg（1 分値）は 5.0〜9.4（32 回）。
`late_frame_discarded` は、期限後に着いたフレームを数えずに捨てた場合だけ true に
なる。`stream`・`transfer` は全窓で false、`ipc` は飽和の 720p の 1 本だけが
true。CPU% の分母は `cpu_reading_secs`（19.997〜20.011 秒）。比較のための baseline は
直結の `stream` と、proxy を挟む `transfer` の 2 つ。この表は、1 つ前の全量実行
（proxy の終了時刻と 2 hop 区間の開始境界を直す前の窓）の値を置き換える。

**直結（`stream`）**

| viewport | tab | 要求 fps | 配送 fps | 到着間隔 p50/p95 ms | age p50/p95 ms | drop（lost） | CPU% helper tree / client | PSS 窓の開始→peak MiB |
|---|---|---|---|---|---|---|---|---|
| 1280×720 | 1 | 1 | 0.95 | 999.9 / 1001.2 | 1001.8 / 1002.6 | 0 | 0.5 / 1.3 | 363 → 366 |
| | 1 | 10 | 10.0 | 100.0 / 100.5 | 1.7 / 2.3 | 0 | 3.2 / 1.6 | 359 → 367 |
| | 1 | **30** | **30.0** | 33.3 / 33.8 | 34.6 / 35.0 | 0 | 9.1 / 2.6 | 362 → 371 |
| | 1 | 60 | 60.0 | 16.7 / 17.1 | 1.6 / 2.1 | 0 | 14.1 / 3.8 | 359 → 368 |
| | 4 | 10 | 40.0 | 18.3 / 47.3 | 1.8 / 101.5 | 0 | 11.8 / 3.1 | 465 → 482 |
| | 4 | **30** | **119.9** | 4.8 / 16.3 | 27.6 / 35.0 | 2（lost 2） | 37.8 / 7.5 | 468 → 487 |
| 3840×2160 | 1 | **30** | **29.95**（948MiB/s） | 33.3 / 34.4 | 49.7 / 50.9 | 0 | 76.3 / 13.1 | 560 → 569 |
| | 4 | 30 | 77.3（要求 120） | 12.9 / 14.2 | 154.2 / 158.4 | 2（lost 2） | 219.5 / 32.7 | 1451 → 1469 |

age は §5.1 のとおりページの描画 timer と helper の paint の位相を含み、同じ設定でも
run ごとに大きく動く（720p×30fps の 1 tab は、旧手順で 1.4ms、1 つ前の全量実行で
1.9ms、今回は 34.6ms。1fps は今回、1 フレーム間隔の位相に当たって 1,002ms）。age は
比較の対象にしない。

**proxy を挟む（`transfer`）**: 最初の byte → client は §5.2 のとおり「受信側で観測した
2 hop の区間」。

| viewport | tab | 要求 fps | 配送 fps | フレーム数 | wire p50/p95/p99 ms | forward p50/p95 ms | 最初の byte → client p50/p95/p99 ms | drop（lost） |
|---|---|---|---|---|---|---|---|---|
| 1280×720 | 1 | 10 | 10.0 | 200 | 0.51 / 0.65 / 0.70 | 0.45 / 0.62 | 1.00 / 1.26 / 1.63 | 0 |
| | 1 | **30** | 30.0 | 600 | 0.51 / 0.76 / 1.11 | 0.44 / 0.60 | **0.98 / 1.35 / 1.82** | 0 |
| | 1 | 60 | 60.0 | 1200 | 0.47 / 0.59 / 0.68 | 0.42 / 0.53 | 0.92 / 1.11 / 1.35 | 0 |
| | 4 | **30** | 119.95 | 2399 | 0.47 / 0.73 / 1.68 | 0.44 / 0.73 | 0.95 / 1.43 / 3.56 | 1（lost 1） |
| 320×180 | 1 | 30 | 30.0 | 600 | 0.04 / 0.05 / 0.07 | 0.02 / 0.05 | 0.09 / 0.12 / 0.14 | 0 |
| 3840×2160 | 1 | **30** | 29.95 | 599 | 4.15 / 5.03 / 5.52 | 3.58 / 4.12 | **7.77 / 8.78 / 9.38** | 0 |
| | 4 | 30 | 74.25 | 1485 | 3.93 / 5.67 / 7.59 | 5.12 / 6.82 | 9.15 / 12.08 / 13.93 | 2（lost 2） |

- 太字が 16a-2 の比較対象にする組（720p×30fps の 1 tab と 4 tab、4K×30fps の
  1 tab）。1 回ずつの測定で、誤差は載せていない。
- 旧手順（§5.2）の 2 hop 区間 p95 は 720p×1 tab で 1.16ms、4K×1 tab で 8.64ms、
  720p×4 tab で 1.31ms、4K×4 tab の fps は 76.9。今回は 1.35 / 8.78 / 1.43 / 74.25。
  p50 も 720p×1 tab 0.96→0.98ms、4K×1 tab 7.79→7.77ms で、旧手順と同じ範囲に
  戻った。修正前の窓（1 つ前の全量実行）で 4K×1 tab の p95 が 10.98ms まで上がった
  のは、窓をまたぐ転送の扱いの影響を含んでいた可能性が高い。ただし 2 回の間は負荷も
  違う（loadavg）ので、窓の修正だけが原因だとは言い切れない。どちらの手順でも基準
  （30fps で p95 33ms 以内）は満たす。
- 同じ全量実行で `ipc`、`tabs`、`startup` も測り直した。`startup` は §3、
  `tabs` は §4 に載せた。`ipc` は 720p×30fps で 30.0fps・drop 0（latency
  p50/p95/p99 0.61 / 0.82 / 0.92ms）、4K×30fps で 29.95fps・drop 0（5.6 / 6.2 /
  7.2ms）、4K×4 tab×30fps は 119.95fps・drop 0（9.1 / 11.3 / 12.0ms）で、§5.3 の
  値と同じ範囲に収まった。4K×4 tab で 101.4fps まで落ちた 1 つ前の全量実行は、
  この再測定では再現しなかった（原因は調べていない）。§5.3・§5.4 の表は旧手順の値の
  まま残してある（今回の飽和側は 720p が 3,237fps・lost 258,962、4K が 216fps・
  lost 3,311。`hold_ms=200` は 5.0fps・queue による置き換え 1,089、どちらの
  解像度でも）。

## 6. 16a-2 への提案

1. **helper は事前起動せず、必要になった時に起動する。** 起動は warm で spawn →
   最初のフレームまで p50 239ms（233–343ms。うち CEF 初期化 約 110〜115ms、
   `open_artifact` 120〜190ms）。一方、常駐は tab が無くても 8 process・
   PSS 267MiB を食う。起動を待たせる側のコストは artifact を開く 1 回に
   0.23〜0.35 秒で、常駐のコストより小さい。最後の tab を閉じたら idle
   timeout（例: 30〜60 秒）で停止する。cold のときは最初の `Hello` が約 1 秒に
   なりうる（1 回だけの観測）ので、`Hello` の待ちは 1 秒より十分長く取る。
   コアの起動時に先に温めるなら、Admission を通さずに常駐させないこと。
2. **helper は core に 1 つ、tab は共有する。** 2 tab 目以降の追加は PSS +30MiB
   （720p）だが、helper 1 個の基礎は 267MiB。tab ごとに helper を立てない。
   `MAX_TABS = 4` は妥当（5 個目は `Busy` で拒否されることを確認した）。
3. **Admission の見積もりは「基礎 + tab × 画素数」で出す。**
   基礎 PSS 約 267MiB、tab あたり約 30MiB/Mpix（最初の tab は GPU/renderer の分で
   さらに +60MiB 程度）。720p×4 で約 460MiB、4K×4 で約 1.35GiB（RSS は 2.9GiB）。
   予約は PSS 基準で、1 tab 720p なら 400MiB、720p×4 なら 512MiB、4K を含むなら
   1.5GiB を目安にする。
4. **SpawnLedger の process 数は 1 helper あたり 16 を上限に。** 実測は tab 0 で 8、
   4 tab で 13（1 tab 目 +2、以降 +1/tab）。
5. **停止は `Shutdown` → 期限で process group への SIGKILL。** orderly は
   `shutdown` の復帰 2〜4ms、helper の終了 26〜39ms、子孫 reap まで 30〜43ms。
   SIGKILL は 8〜9ms。いずれも group の全員を reap できた（コアは subreaper に
   なるか、group 単位で待つこと）。期限は 1 秒もあれば十分。
6. **frame rate と解像度に予算を置く。** 既定は 30fps。helper の CPU は
   配送する画素レート 1Mpix/s あたり 1 論理 CPU の 0.31〜0.34%（720p×30fps で
   9.1%、720p×4 tab×30fps で 38%、4K×30fps で 76%）で、画素レートにほぼ
   比例する。集計で約 250Mpix/s（4K×1 tab×30fps、または 720p×9 tab 相当。
   helper が 1 論理 CPU の約 75〜80%）を上限にする。これを超える 4K×4 tab×30fps
   （約 1,000Mpix/s）は配送が飽和した（77.3fps、helper 220%）。背景 tab は
   10fps に下げる。
7. **フレームを開く fps は 10 以上にする。** 1fps で `open_artifact` を開くと 3〜4 秒
   かかる（旧手順 3.0 秒、今回 4.0 秒）。低い rate が欲しいなら、10fps 以上で開き client 側で間引くか、
   frame rate を変える request を足す（protocol の拡張）。
8. **client の frame slot を tab ごとにする（16a-2 か 16b）。** 今は 1 slot なので
   複数 tab でフレームが tab 間で上書きされる（720p×4 tab×30fps で 0.08〜0.8%
   落ちる）。前景 tab だけを stream するなら不要。
9. **大きい tab を閉じたら helper を再起動する。** 4K の tab を閉じても PSS は
   765MiB 残る（起動直後は 267MiB）。最後の tab が閉じたら、4K を使った helper は
   停止して次回に起動し直す（再起動は 0.23〜0.3 秒）。常駐させる helper には
   1 回あたりの画素数の上限を置いてもよい。
10. **転送は Unix socket + raw BGRA のまま確定。** 出荷する helper の転送（proxy 込み
    の 2 hop）は 30fps の p95 が 720p で 1.35ms、4K で 8.8ms と、基準 33ms の
    1/3 以下（720p は 1/24）。共有メモリ（spike で 0.2〜0.3ms 速いだけ）は不要。
    4K×30fps の 950MiB/s でも client の CPU は 13%。
11. 16a-2 の `core_renderer_cache` E2E で確認する項目: helper の予定再起動後の
    artifact 再 open、停止後の残留 process 0、lock FD と credential を helper が
    継承しないこと（#610 の申し送りのとおり）。

## 7. 未実施・未解決

- **native GUI との同時稼働は測っていない。16b の段階で測る。** spike の
  off-screen の GUI harness での共存は §2 のとおり（衝突なし）。ウィンドウを開く
  計測は別途確認が要る。
- production の core からの spawn、Admission/SpawnLedger 経由の起動は 16a-2。
  この文書の起動時間は、コアの手前（`RendererClient` の直接起動）の値。
- 実 helper の転送は proxy を挟んだ 2 hop の値で、直結の値ではない（`wire` が
  直結に近い下限）。受信側で観測した区間なので、paint の開始や GUI の表示完了までの
  latency は含まない。helper 内の OnPaint の時刻と、ページ描画から送信開始までの
  内訳は取っていない（age と `page_to_first_byte` は位相を含む）。
- `open_artifact` の 120ms 前後と 190ms 前後の二峰の原因は調べていない。
- 長時間 soak は 120 秒まで（spike）。20 秒窓の本計測では、メモリは増えなかった。
  数時間の soak は未実施。
- macOS/Windows は未実施。process 計測は Linux の `/proc` 前提。
- helper は dev profile。CEF 本体は prebuilt なので差は小さいはずだが、release の
  数値は取っていない。
- 計測は 1 回ずつで、共有 workstation の負荷の影響を受けた。配送 fps は再現性が
  高いが、CPU%、飽和側の p95、tab 間の drop は目安。
- cold start（ページキャッシュを落とした起動）は、キャッシュを落とせないため
  再現手順に含めていない。
- 計測ツールは単独 package のため lefthook と CI の対象外。手で
  `scripts/measure-renderer.sh --check` を実行する（§1）。

## 8. 16a-2 F3: data plane の fan-out（`fanout`、2026-11、同一 process の prototype）

F3 の設計（1 tab の latest frame を複数の data 接続へ配る）の前に、構造だけを
同じにした prototype で決めた。helper も CEF も使わない。helper 役の thread が
socket pair へ frame を書き（ack を待つ）、core 役の reader が 1 つの buffer に読んで
tab の latest として公開し、subscriber ごとの sender thread が自分の socket へ書き、
client 役の reader が全 byte を受ける。latency は core が最初の byte を観測した時刻から
client が frame の全 byte を持つまで（受け入れ基準の p95 ≤ 33ms と同じ区間）。
`share=1` は全 sender が 1 つの `Arc` の allocation を書く。`share=0` は sender ごとに
copy する。stalled は接続して読まない subscriber。4K（33,177,600 byte）、30fps、8 秒、
24 論理 CPU の共有 workstation、1 回ずつ。

| tab | subscriber | share | chunk | p50 ms | p95 ms | max ms | 欠落 frame | 最大の生存 frame memory | CPU（1 core 比） |
|---|---|---|---|---|---|---|---|---|---|
| 1 | 1 | 共有 | 64KiB | 9.3 | 10.8 | 12.8 | 0 | 63 MiB | 46% |
| 1 | 4 | 共有 | 64KiB | 12.5 | 15.0 | 16.6 | 0 | 63 MiB | 183% |
| 1 | 4 | copy | 64KiB | 24.4 | 26.8 | 29.9 | 0 | 190 MiB | 336% |
| 4 | 8 | 共有 | 64KiB | 28.3 | 32.1 | 42.1 | 0 | 253 MiB | 844% |
| 4 | 8 | copy | 64KiB | 112.9 | 140.5 | 172.0 | 577 | 633 MiB | 1396% |
| 4 | 8（+読まない 4） | 共有 | 64KiB | 26.8 | 29.4 | 34.6 | 0 | 380 MiB | 811% |
| 1 | 4 | 共有 | 1 write | 11.7 | 14.3 | 16.2 | 0 | 63 MiB | 167% |
| 1 | 4 | 共有 | 1MiB | 13.3 | 15.0 | 16.2 | 0 | 63 MiB | 191% |
| 4 | 8（720p、60fps） | 共有 | 64KiB | 3.1 | 3.6 | 4.4 | 0 | 28 MiB | 189% |

決めたこと:

- **1 つの allocation を全 subscriber で共有する（`Arc`）。** copy は 4K の 4 tab × 8
  subscriber で p95 140ms、frame の欠落、peak 633MiB となり、基準も memory の予算も
  満たさない。共有なら p95 32.1ms。
- **write の単位は 64KiB。** 1 write、64KiB、1MiB の差は誤差内（p95 14.3〜15.0ms）。
  1 write は flow control の細かさを失うので、64KiB を採る。
- **S ≤ 8 subscriber、T ≤ 4 tab を上限にする。値は上げない。** 4K の最悪構成の p95 は
  32.1ms で、33ms の基準に余裕がほとんどない（max は 42ms）。
- **読まない subscriber は frame を握ったままになる**（上の +4 行で peak が +127 MiB）。
  よって no-progress timeout（5 秒）で切り、その間に握れる量を予算で数える。
- 限界: 同一 process・thread の prototype で、実 helper、core の async 層、GUI の表示は
  含まない。実装後に同じ区間を実装の上で再測定し（F3 の最終 commit）、基準を確かめる。

## 9. 16a-2 F3: 製品実装の再測定（`renderer_data_plane_measure`、2026-11）

§8 の prototype ではなく、出荷する経路を測った。core は `HostBackend` と
`unix::connection` の実物で、client は `workbench_client::renderer::DataChannel` の実物。
helper は fake helper（`evorch-renderer-test-helper`）に 1 つだけ足した合成の producer で、
tab の viewport の frame を 30fps で、先頭 8 byte に wall clock（ns）を書いて publish
する。同じ時計を helper・core・client が共有するので、latency は「helper が stamp してから
client が全 byte を持つまで」になる。

- 実行: `cargo nextest run --release -p evorch-core --features renderer-test-helper
  --test renderer_data_plane_measure --run-ignored only --no-capture`（1 scenario 1
  process、`EVORCH_MEASURE_OUT=<file>` で JSON 行を追記）。全 test が `#[ignore]` で、
  CI の時間 assertion にはしない（plan §5「性能閾値を共有 CI の不安定な時間 assertion に
  しない」）。判定はこの文書の同一環境の証跡で行う。
- fake helper には、`synth-<N>fps` という名前の link 越しに起動されたときだけ合成 producer
  が動く（core は helper の環境を消すので、起動名が唯一の入口）。通常の試験の動作は変えない。
- 区間は plan の「core が helper 出力の最初の byte を観測してから」より前（helper の stamp）から
  始まる。helper → core の区間（publish の copy、frame channel への write、core の read）を
  含むので、受け入れ基準の区間より長い（厳しい側）。この区間は 4K の 1 reader の p50 12ms 前後
  のうち数 ms 分。core が最初の byte を観測した時刻は process の外から取れないので、引いた
  値は載せない。
- 窓は 8 秒、先頭 10 frame は統計から外す。環境は §1 の「環境」と同じ（Ryzen 9 7900、24 論理
  CPU、共有 workstation。実行中の loadavg は 1 分値で 7〜10。他の作業が常時動いている）。
  client は core と同じ process にいるので、CPU と RSS は core と全 client の合計。
- 製品の上限が先に効く構成がある。subscription は **core 全体で 8**、tab は 4、4K は 30fps 以上
  では 1 つの stream だけ（`budget.rs`）で、その subscription は 4 まで。よって §8 の「4 tab × 8 subscriber」は製品では作れず、
  720p は 4 tab × 2、1080p は 4 tab × 2、4K は 1 tab × 4 が最大（下記の制限）。4K の複数 tab は
  `Busy` で拒否される（これは設計どおり。F3 の前から同じ）。

**製品実装（chunk を共有 frame から vectored write、client は body を `Vec` へ直接 read）**

| viewport | tab × reader（+停止） | 配送 fps / reader | p50 ms | p95 ms | p99 ms | max ms | 最悪 reader の p95 ms | CPU（core 数） | RSS peak MiB |
|---|---|---|---|---|---|---|---|---|---|
| 1280×720 | 1 × 1 | 29.99 | 1.56 | 2.24 | 3.88 | 8.7 | 2.24 | 0.06 | 65 |
| | 4 × 1 | 29.99 | 4.04 | 6.66 | 8.28 | 9.6 | 7.11 | 0.33 | 86 |
| | 1 × 8 | 29.99 | 3.43 | 6.39 | 10.62 | 20.9 | 6.74 | 0.74 | 89 |
| | 4 × 2 | 29.99 | 3.74 | 4.99 | 5.70 | 7.8 | 5.37 | 0.54 | 101 |
| 1920×1080 | 4 × 2 | 29.99 | 8.68 | 11.19 | 13.12 | 19.0 | 12.25 | 1.09 | 158 |
| 3840×2160 | 1 × 1 | 30.00 | 11.72 | 13.84 | 14.94 | 16.5 | 13.84 | 0.36 | 149 |
| | 1 × 4 | 30.00 | 15.46 | 18.53 | 19.44 | 20.3 | 18.56 | 1.88 | 244 |
| | 1 × 4（+停止 1）※ | 29.99 | 15.23 | 17.22 | 18.42 | 20.3 | 17.33 | 1.78 | 275 |

※ この行（旧 scenario `k4_1tab_4reader_1stalled`）は subscription 5 つ（reader 4 + 停止 1）で、4K の上限 4 を入れる**前**の値。製品では 5 つ目が `Busy` になるので、この構成は作れない。現在の scenario は `k4_1tab_3reader_1stalled`（reader 3 + 停止 1 = 4 subscription）で、値は下の「9.1」の表にある。

1 回ずつの測定で誤差は載せていない。停止 reader は latency の判定から外してあり、4K の 4 reader
で停止 1 つを足しても通常 reader は劣化しない（p95 18.5 → 17.2ms。差は誤差内）。全 reader が
全 frame を受け、配送 fps は要求の 30.0 と同じ（欠落は 1 reader あたり 0.1 frame 未満）。

**4K × 8 reader は基準を安定して満たさず、製品で 4 に制限した**。8 reader の 4K（1 tab）を
繰り返し測ると、次のようになった（`renderer_data_plane_measure` の旧 scenario
`k4_1tab_8reader`。制限の後は作れないので scenario からは外した）。

| 実装 | p95 ms（実行順） |
|---|---|
| 変更前（chunk ごとに `Vec` へ copy して write、client は一度 `body` に読んでから copy） | 34.65（最悪 reader 35.20） |
| 現在（vectored write、client の直接 read） | 30.18、31.21、30.72、80.72、61.20、36.83、85.04、32.40、31.02、39.43、98.65、32.29 |

12 回のうち 33ms 以下は 6 回（30.2〜32.4）で、残りの 6 回（36.8〜98.7）は外れ値（他の
作業が動いている間に起きたと見ているが、確かめていない）。同じ負荷（loadavg 約 8）でも
外れる回と通る回があった。4K × 8 は 8 × 33MB × 30 = 約 8GB/s を UDS で送る（kernel の copy が
送受で 2 回）ので、この workstation では CPU とメモリ帯域の上限に近い。**基準（p95 ≤ 33ms）を
安定して満たしたとは言えない**ので、閾値は緩めず、構成を制限した（親の承認済みの判断）。

- **制限**: 1 tab の frame が 4K（`3840×2160`）かつ 30fps 以上のとき、その tab の frame
  subscription は **4**（`MAX_FOUR_K_FRAME_SUBSCRIPTIONS`）。一般の上限（core 全体で 8）は
  変えない。4K・30fps 以上は budget でも 1 つの stream しか持てない（排他）ので、実際の
  使い方（4K を 8 つの表示先が同時に見る）は想定外としてよい。
- **5 つ目の拒否**: `Busy`、`RetryAdvice::AfterResync`（subscription が 1 つ終われば空くので、
  状態を読み直して決め直せる）。拒否は既存の subscription、data 接続、stream に触れない。
- **途中で 4K になる場合**: 新しく追加する subscription だけを拒否し、すでにあるもの
  （最大 8）は保つ（切らない）。試験（`renderer_data_plane_uds`）で固定した: 4K で 4 つまで
  受け付け 5 つ目は `Busy`、1080p に下げると同じ tab が 8 つまで受け付け 9 つ目は一般の上限で
  `Busy`、その 8 つを保ったまま 4K に戻すと新規は `Busy`・既存の binding は有効。
- **その残りの p95**: 4 を超えた subscription が残った 4K の tab の p95 は、この制限では
  保証しない。8 reader の測定（上の表）が当てはまり、基準を外れうる（33ms 超が 12 回中
  6 回）。この状態は、既存の subscription を保つ方針の帰結で、tab を 4K へ resize した
  利用者が、すでに 5 つ以上の表示先を持っているときに限る。subscription の終了で
  4 以下に戻れば、次の表の値に戻る。
- **4K × 4 reader の p95**: 同じ構成を 12 回（各 8 秒、loadavg 5.5〜9.7）測って
  p95 は 18.8〜25.7ms（中央 19.9ms、1 回だけ 25.7、他は 21.5 以下）、p50 は 15.5〜19.3ms、
  最悪 reader の p95 は 19.1〜26.1ms、CPU は 1.85〜2.38 core。12 回すべて 33ms 以下。
  最初の 1 回（上の表）は 18.5ms。

比較した変更（plan §5「不合格時は chunk、vectored write、再利用、単一購読経路を比較する」）:

- **vectored write と client の直接 read（採用）**: header と共有 frame の slice を 1 回の
  `write_vectored` で送り、chunk ごとの `Vec` の確保と copy をなくした。client は body を
  `read_buf` で呼び出し側の `Vec` に直接読み、`resize` の zero 埋めと中間 buffer からの copy を
  なくした。4K × 8 の p95 は 34.65 → 30.2〜32.4ms（quiet な回。外れ値は残った）。
- **送信側の `SO_SNDBUF` を 4MiB にする（不採用）**: 5 回とも p95 33.0〜44.1ms で CPU は 7.8 → 11.2
  core に増え、悪化した。戻した。
- **chunk の大きさ**: §8 の prototype で 1 write・64KiB・1MiB の差が誤差内だったので、
  protocol の値（64KiB）は変えない。
- **単一購読経路**: 4K の frame を 1 本の経路で配るのは、subscription ごとの接続という
  F3 の設計の外になるので比べていない。

**ベースラインとの比較**（§5.5）。720p×30fps の 1 tab は、直結の配送 30.0fps に対し proxy の
「最初の byte → client」p95 が 1.35ms、製品は 2.24ms（helper の stamp から。上の区間の違いを
含む）。4K×30fps の 1 tab は、同 8.78ms に対し 13.84ms。どちらも基準の 33ms の半分以下で、
frame は欠落していない。4 tab（720p）は、baseline の 4 tab 合計 119.95fps に当たる
29.99fps / tab を保った。

**測れていないもの**

- 実 helper と CEF を通した性能。実 CEF の e2e（`renderer_owner_cef`）は動作の確認で、時間は測らない。
  helper → core の区間は §5.5 の実測（4K×1 tab で 2 hop 区間 p95 8.78ms）が最新。
- 実 CEF 越しの PSS・allocation（下の 9.1 は fake helper の値で、helper 側の PSS は合成 producer のもの）。
- 専用機での測定。共有 workstation のため、4K × 8 reader のばらつき（制限の理由）が load による
  ものか実装によるものかは切り分けられていない。
- macOS / Windows。この data plane は Linux の UDS だけ（既存の方針）。

### 9.1 PSS・allocation・heap（fake helper、2026-11、窓を揃えた harness）

§9 の表を測った harness から、集計の窓を直して測り直した。**すべての reader は、同じ
`[start, end)` の窓（start は streaming を有効にしてから warm-up の 12 frame 分の時間が
過ぎた時刻、end は経過時間を確定した時刻）に受け取った frame だけを、latency・frame 数・
allocation の分母に数える**。経過時間・CPU・allocation の差分も、同じ start から取る。
8 秒窓、30fps、24 論理 CPU の共有 workstation（実行中の loadavg は 11 前後）。counting global
allocator の数は core・全 client・harness が同じ process にいる合計で、helper は別 process なので
含まない。PSS は `/proc/<pid>/smaps_rollup` を core の process と helper の子 process で別々に読む。
stalled の scenario は上限 4 を守る `k4_1tab_3reader_1stalled`。

| scenario | frame 数 | p50 ms | p95 ms | p99 ms | max ms | fps/reader | CPU | RSS MiB | PSS core / helper MiB | allocs/frame | alloc B/frame | 窓内の 1MiB 以上の確保 | window 開始時の heap MiB | close 要求の 2 秒後の heap MiB |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| p720_1tab_1reader | 247 | 1.69 | 2.59 | 3.51 | 4.7 | 30.10 | 0.08 | 65 | 61 / 8 | 467 | 30921 | 0 | 13 | 9 |
| p720_4tab_1reader | 992 | 3.27 | 6.23 | 8.73 | 10.6 | 30.11 | 0.35 | 86 | 82 / 40 | 170 | 9172 | 0 | 34 | 9 |
| p720_1tab_8reader | 1976 | 3.63 | 5.76 | 8.24 | 17.7 | 30.02 | 0.70 | 90 | 86 / 8 | 119 | 5353 | 0 | 37 | 9 |
| p720_4tab_2reader | 1970 | 3.57 | 5.01 | 6.02 | 8.1 | 30.02 | 0.53 | 100 | 97 / 33 | 120 | 5458 | 0 | 48 | 9 |
| p1080_4tab_2reader | 1984 | 10.11 | 13.93 | 15.69 | 20.0 | 30.07 | 1.40 | 158 | 155 / 88 | 190 | 6588 | 0 | 105 | 18 |
| k4_1tab_1reader | 247 | 12.61 | 17.27 | 18.71 | 19.1 | 30.04 | 0.41 | 149 | 145 / 64 | 926 | 38798 | 0 | 97 | 65 |
| k4_1tab_4reader | 992 | 21.45 | 26.32 | 27.99 | 31.2 | 29.99 | 2.61 | 244 | 241 / 64 | 618 | 16208 | 0 | 192 | 65 |
| k4_1tab_3reader_1stalled | 738 | 15.01 | 17.20 | 18.48 | 18.9 | 29.94 | 1.27 | 244 | 241 / 64 | 656 | 18882 | 0 | 192 | 65 |

- **p95 はすべて 33ms 以下**（最大は `k4_1tab_4reader` の 26.32ms。この run は loadavg 約 11 で、§9 の 12 回
  の p95 18.8〜25.7ms より高い側に出た）。§9 の表は窓の数え方が違う（先頭 10 frame を数で外し、接続が
  終わるまで数えた）ので、二つの値は同じ測定の再現ではなく、同じ基準に対する別の証跡として読む。
  `k4_1tab_1reader` の p95 は run ごとに 14〜25.7ms の幅があった（§9.1 の前の 5 回: 14.15〜17.76ms）。
- **steady state で frame ごとの大きい確保はない**: 窓内の 1MiB 以上の確保は全 scenario で 0 回、
  live heap の窓内の増分（peak）も 0 だった。frame の buffer は warm-up の間に pool が持ち終わり、
  窓の間は再利用されている。window 開始時の heap（4K の 4 reader で 192MiB）は、その pool と
  各 reader が持つ frame の合計。allocs/frame の多くは header・chunk の小さい確保で、reader が
  増えるほど 1 frame あたりは減る（frame の共有）。
- **close 後の heap**: 全 client の接続を閉じ、`CloseRendererSession` を**要求**して 2 秒待った後の heap は、
  720p で約 9MiB、1080p で約 18MiB、4K で約 65MiB（4K の frame 約 33MB の 2 枚ぶんに近い）。
  close は要求しただけで、**完了したかどうかも、何がこの heap を保持しているかも確かめていない**。
  観測した事実は、reader の数（1〜4）を変えても 4K では同じ約 65MiB で、reader ごとの保持は
  見えないこと、だけ。原因の特定と、close の完了後に 0 近くへ戻るかの確認は未実施。
- PSS は core の process が RSS とほぼ同じ（client も同じ process）。helper の PSS は fake helper の合成
  producer の値で、4K で約 64MiB、tab 数にほぼ比例する。
- 元の JSONL（この表と §9 の追加の繰り返しの run）は PR #693 のコメントに添付した。
- 実 CEF の helper の PSS と、close の完了後の heap は測っていない。
