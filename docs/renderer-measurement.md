# renderer helper の実測（ADR 0045 段階 16a）

2026-11 時点。ADR 0045 §3 は、CEF/browser を「オンデマンドで起動される
renderer helper」としてコア外へ分離する。この文書は 16a-2（helper をコアへ
組み込み、Admission → SpawnLedger に接続する PR）の設計に必要な数値を集める。
2 つの計測を載せる。

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
    停止後の残留 process も測る。メモリは別の試行（`mem=1`）で最初のフレームの
    後にだけ読み、その試行の latency には「読み取りで伸びた」と印を付ける。
  - `tabs`: tab を 1〜4 個開いたときの process 数・RSS・PSS・USS、5 個目の拒否、
    全 tab を閉じた後の残り。
  - `stream`: 実 helper。毎フレーム色が変わり、時刻を画素に埋め込む canvas
    （`fixture.rs`）を描き、`wait_frame` で受ける。配送 fps、到着間隔、
    ページ時刻から client までの age、drop、helper tree の CPU、メモリ。
  - `transfer`: `stream` と同じ負荷で、helper と client の frame channel の間に
    pass-through の proxy（`proxy.rs`）を挟む。実 helper の sender が出す
    1 フレームについて、最初の byte が読める時刻、全 byte を読み終えた時刻
    （`wire`）、client へ書き終えるまで（`forward`）、client が手にするまでを
    測る。**実 helper → client の転送 p50/p95 はここで出す**。
  - `ipc`: helper の代わりに同じ process 内の合成 backend が `FrameQueue::publish`
    でフレームを出す。CEF を除いた IPC（queue→送信→socket→client reader）だけの
    latency。先頭 8 byte の publish 時刻から測る。`fps=0` は上限まで出す飽和
    試験、`hold_ms` は遅い consumer。
- 窓: `stream`/`transfer`/`ipc` は先頭 10 フレームを捨て、その後の 20 秒を測る。
  到着の seq を見る `stats::Window` は、窓の先頭の 1 フレーム（基準）を数えず、
  次のフレームからを fps・drop・欠落に数える。
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
| 負荷 | 共有 workstation（全量実行の開始時 loadavg 2.8〜4.3）。専用機ではない |
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

timing 8 回。spawn の直前から最初のフレームまでを 1 つの `Instant` で測る。
メモリの読み取りは含まない。

| 区間 | 値（ms） |
|---|---|
| `spawn` の呼び出し | 0.13–0.19 |
| spawn → `Hello` 応答（CEF 初期化を含む） | 104.8–114.8（p50 108.6） |
| `open_artifact`（1280×720、30fps、1 tab 目） | 118.0–120.3（5 回）、184.2–185.9（3 回） |
| `set_streaming` → 最初のフレーム | 1.0–2.4 |
| **spawn → 最初のフレーム（1 つの `Instant`）** | **226.8–294.3（p50 234.5）** |

- spawn → 最初のフレームは 5 回が 227–235ms、3 回が 291–294ms。差は
  `open_artifact` の 118ms と 185ms の二峰に一致する（原因は未調査）。kill 試験の
  3 回でも 230 / 297 / 261ms で同じ範囲。
- 上の値は warm（libcef がページキャッシュにある状態）。ビルド直後の smoke 実行
  で、最初の 1 回だけ `Hello` まで 905ms かかったのを見た（再現手順には含めて
  いない。キャッシュを落とせないため）。cold では最初の起動が ~1 秒に伸びる
  ものとして扱う。
- 停止（shutdown 8 回）:
  - `RendererClient::shutdown` の復帰は 2.0–2.6ms。これは `Shutdown` の ack に加え、
    2 本の channel の close と reader thread の終了まで待った時間で、ack だけの
    時間ではない（値の名前は `shutdown_returned_ms`）。
  - helper の終了は 25.9–28.3ms、CEF の子孫 3 process の reap までで 29.2–31.8ms。
- process group への SIGKILL（3 回）は終了 8.9–9.9ms、子孫 5 process の reap まで
  9.0–10.1ms で、subreaper に戻る。どちらの停止でも、残留 process は 0。
- 起動直後（tab なし）: **8 process**（helper 1、utility 1、zygote 系 6）、
  **PSS 239MiB**（RSS 555–558MiB、USS 133MiB）。
- 1 tab 開き、最初のフレームを受けた後: 10 process、PSS 327–331MiB
  （RSS 約 800MiB）。memory 試行（読み取りが latency を伸ばすので、spawn → 最初の
  フレームは 259–261ms と印を付けた別試行）の値。

## 4. tab と解像度（`tabs`）

`MAX_TABS = 4`。5 個目は `Busy` で拒否された（どちらの解像度でも）。

| viewport | tab 数 | process | PSS MiB | RSS MiB | USS MiB | `open_artifact` ms |
|---|---|---|---|---|---|---|
| 1280×720 | 0 | 8 | 240 | 556 | 133 | – |
| | 1 | 10 | 331 | 805 | 179 | 118 |
| | 2 | 11 | 364 | 954 | 181 | 118 |
| | 3 | 12 | 393 | 1099 | 199 | 116 |
| | 4 | 13 | 422 | 1244 | 217 | 117 |
| | 全部閉じた後 | 8 | 263 | 591 | 151 | – |
| 3840×2160 | 0 | 8 | 240 | 559 | 133 | – |
| | 1 | 10 | 496 | 1089 | 229 | 264 |
| | 2 | 11 | 793 | 1618 | 378 | 247 |
| | 3 | 12 | 1043 | 2117 | 485 | 290 |
| | 4 | 13 | 1322 | 2674 | 592 | 310 |
| | 全部閉じた後 | 8 | **738** | – | – | – |

- 1 tab 目で +2 process（GPU/renderer）、以降は +1 process/tab。
- PSS は tab あたり約 30MiB/Mpix（720p ≈ +30MiB、4K ≈ +250〜300MiB）に、最初の
  tab の固定分が少し乗る。
- 4K の tab を閉じても PSS は戻りきらない（738MiB。起動直後は 240MiB）。720p では
  263MiB でほぼ戻る。

## 5. フレーム配送

### 5.1 実 helper（`stream`、20 秒窓）

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

## 6. 16a-2 への提案

1. **helper は事前起動せず、必要になった時に起動する。** 起動は warm で spawn →
   最初のフレームまで p50 235ms（227–294ms。うち CEF 初期化 約 109ms、
   `open_artifact` 118〜185ms）。一方、常駐は tab が無くても 8 process・
   PSS 240MiB を食う。起動を待たせる側のコストは artifact を開く 1 回に
   0.23〜0.3 秒で、常駐のコストより小さい。最後の tab を閉じたら idle
   timeout（例: 30〜60 秒）で停止する。cold のときは最初の `Hello` が約 1 秒に
   なりうる（1 回だけの観測）ので、`Hello` の待ちは 1 秒より十分長く取る。
   コアの起動時に先に温めるなら、Admission を通さずに常駐させないこと。
2. **helper は core に 1 つ、tab は共有する。** 2 tab 目以降の追加は PSS +30MiB
   （720p）だが、helper 1 個の基礎は 240MiB。tab ごとに helper を立てない。
   `MAX_TABS = 4` は妥当（5 個目は `Busy` で拒否されることを確認した）。
3. **Admission の見積もりは「基礎 + tab × 画素数」で出す。**
   基礎 PSS 約 240MiB、tab あたり約 30MiB/Mpix（最初の tab は GPU/renderer の分で
   さらに +60MiB 程度）。720p×4 で約 420MiB、4K×4 で約 1.3GiB（RSS は 2.7GiB）。
   予約は PSS 基準で、1 tab 720p なら 350MiB、720p×4 なら 512MiB、4K を含むなら
   1.5GiB を目安にする。
4. **SpawnLedger の process 数は 1 helper あたり 16 を上限に。** 実測は tab 0 で 8、
   4 tab で 13（1 tab 目 +2、以降 +1/tab）。
5. **停止は `Shutdown` → 期限で process group への SIGKILL。** orderly は
   `shutdown` の復帰 2〜3ms、helper の終了 26〜28ms、子孫 reap まで 29〜32ms。
   SIGKILL は 9〜10ms。いずれも group の全員を reap できた（コアは subreaper に
   なるか、group 単位で待つこと）。期限は 1 秒もあれば十分。
6. **frame rate と解像度に予算を置く。** 既定は 30fps。helper の CPU は
   配送する画素レート 1Mpix/s あたり 1 論理 CPU の 0.25〜0.33%（720p×30fps で
   6.7%、720p×4 tab×30fps で 36%、4K×30fps で 75%）で、画素レートにほぼ
   比例する。集計で約 250Mpix/s（4K×1 tab×30fps、または 720p×9 tab 相当。
   helper が 1 論理 CPU の約 75〜80%）を上限にする。これを超える 4K×4 tab×30fps
   （約 1,000Mpix/s）は配送が飽和した（83.8fps、helper 230%）。背景 tab は
   10fps に下げる。
7. **フレームを開く fps は 10 以上にする。** 1fps で `open_artifact` を開くと 3 秒
   かかる。低い rate が欲しいなら、10fps 以上で開き client 側で間引くか、
   frame rate を変える request を足す（protocol の拡張）。
8. **client の frame slot を tab ごとにする（16a-2 か 16b）。** 今は 1 slot なので
   複数 tab でフレームが tab 間で上書きされる（720p×4 tab×30fps で 0.08〜0.8%
   落ちる）。前景 tab だけを stream するなら不要。
9. **大きい tab を閉じたら helper を再起動する。** 4K の tab を閉じても PSS は
   738MiB 残る（起動直後は 240MiB）。最後の tab が閉じたら、4K を使った helper は
   停止して次回に起動し直す（再起動は 0.23〜0.3 秒）。常駐させる helper には
   1 回あたりの画素数の上限を置いてもよい。
10. **転送は Unix socket + raw BGRA のまま確定。** 出荷する helper の転送（proxy 込み
    の 2 hop）は 30fps の p95 が 720p で 1.2ms、4K で 8.6ms と、基準 33ms の
    1/3 以下（720p は 1/25）。共有メモリ（spike で 0.2〜0.3ms 速いだけ）は不要。
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
  直結に近い下限）。helper 内の OnPaint の時刻と、ページ描画から送信開始までの
  内訳は取っていない（age と `page_to_first_byte` は位相を含む）。
- `open_artifact` の 118ms / 185ms の二峰の原因は調べていない。
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
