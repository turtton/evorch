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
scripts/measure-renderer.sh --out /path/to/results            # 全部（約 10 分）
scripts/measure-renderer.sh --quick startup stream            # 短縮版
```

- 出力は `<out>/{startup,tabs,stream,ipc}.jsonl`、`environment.json`、`build.log`。
- ビルド: `evorch-renderer` と `evorch-cef-helper` を checkout の cargo でビルド
  （dev profile）。計測ツール `scripts/measure-renderer/` は workspace に属さない
  単独 package（自前の `[workspace]` と `Cargo.lock`）で、`renderer-ipc` を path
  依存にし release でビルドする。root の `Cargo.toml`・CI・flake は変えない。
- 計測ツールは `renderer_ipc::stdio_channels()` と `RendererClient::connect` で本物の
  helper を起動する。起動規約は `crates/evorch-renderer/tests/real_helper.rs`
  と同じ。helper は自分の process group を持ち、ツールは child subreaper になる。
  `DISPLAY`/`WAYLAND_DISPLAY`/`XDG_SESSION_TYPE` を消す。`TMPDIR=/proc/self/cwd`
  にして cwd を run ごとの短い dir にする（Chromium の SingletonSocket が AF_UNIX
  の `sun_path` 上限を超えると SIGTRAP になるため）。CEF の sandbox は有効のまま。
- 同期は `wait_frame`/`shutdown`/`waitid` の条件待ちだけ。sleep は使わない。
  合成 producer のフレーム間隔と、slow consumer の保持時間（`hold_ms`）だけは
  負荷の形を作る時間待ちで、同期ではない。
- scenario:
  - `startup`: spawn/Hello/`open_artifact`/最初のフレームの latency、停止時間
    （`Shutdown` と process group への SIGKILL）、停止後の残留 process。
  - `tabs`: tab を 1〜4 個開いたときの process 数・RSS・PSS・USS、5 個目の拒否、
    全 tab を閉じた後の残り。
  - `stream`: 実 helper。毎フレーム色が変わり、時刻を画素に埋め込む canvas
    （`fixture.rs`）を描き、`wait_frame` で受ける。配送 fps、到着間隔、
    ページ時刻から client までの age、drop、helper tree の CPU、メモリ。
  - `ipc`: helper の代わりに同じ process 内の合成 backend が `FrameQueue::publish`
    でフレームを出す。CEF を除いた IPC（queue→送信→socket→client reader）だけの
    latency。先頭 8 byte の publish 時刻から正確に測れる。`fps=0` は上限まで
    出す飽和試験、`hold_ms` は遅い consumer。
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
| 負荷 | 共有 workstation（開始時 loadavg 3〜14）。専用機ではない |
| 試行 | 各 scenario 1 回（stream/ipc は 20 秒窓、最初の 10 フレームは捨てる）。startup は 8 回 |

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

## 3. 起動と終了（`startup`、8 回）

| 区間 | 値（ms） |
|---|---|
| `spawn` の呼び出し | 0.16–0.21 |
| spawn → `Hello` 応答（CEF 初期化を含む） | 104–114（p50 約 110） |
| `open_artifact`（1280×720、30fps、1 tab 目） | 118（一度 151、一度 185） |
| `set_streaming` → 最初のフレーム | 1.2–2.5 |
| **spawn → 最初のフレーム** | **225–297（p50 約 230）** |

- 上の値は warm（libcef がページキャッシュにある状態）。ビルド直後の smoke 実行
  で、最初の 1 回だけ `Hello` まで 905ms かかったのを見た（再現手順には含めて
  いない。キャッシュを落とせないため）。cold では最初の起動が ~1 秒に伸びる
  ものとして扱う。
- 停止: `Shutdown` の ack は 2.1–2.7ms。helper の終了は 25–30ms（最初の 1 回だけ
  47ms）。CEF の子孫 3 process の reap までで 29–34ms。
- process group への SIGKILL は 8.6–9.5ms で、子孫 5 process が subreaper に戻る。
  どちらの停止でも、残留 process は 0。
- 起動直後（tab なし）: **8 process**（helper 1、utility 1、zygote 系 6）、
  **PSS 242MiB**（RSS 565MiB、USS 134MiB）。
- 1 tab 開いた後: 10 process、PSS 333MiB（RSS 813MiB）。

## 4. tab と解像度（`tabs`）

`MAX_TABS = 4`。5 個目は `Busy` で拒否された（どちらの解像度でも）。

| viewport | tab 数 | process | PSS MiB | RSS MiB | USS MiB | `open_artifact` ms |
|---|---|---|---|---|---|---|
| 1280×720 | 0 | 8 | 242 | 567 | 134 | – |
| | 1 | 10 | 333 | 816 | 180 | 118 |
| | 2 | 11 | 366 | 964 | 182 | 118 |
| | 3 | 12 | 395 | 1108 | 200 | 116 |
| | 4 | 13 | 424 | 1253 | 218 | 118 |
| | 全部閉じた後 | 8 | 266 | – | – | – |
| 3840×2160 | 0 | 8 | 243 | 569 | 135 | – |
| | 1 | 10 | 498 | 1097 | 230 | 264 |
| | 2 | 11 | 795 | 1624 | 379 | 249 |
| | 3 | 12 | 1029 | 2092 | 486 | 295 |
| | 4 | 13 | 1295 | 2623 | 592 | 310 |
| | 全部閉じた後 | 8 | **709** | – | – | – |

- 1 tab 目で +2 process（GPU/renderer）、以降は +1 process/tab。
- PSS は tab あたり約 30MiB/Mpix（720p ≈ +30MiB、4K ≈ +265MiB）に、最初の tab の
  固定分が少し乗る。
- 4K の tab を閉じても PSS は戻りきらない（709MiB。起動直後は 243MiB）。720p では
  266MiB でほぼ戻る。

## 5. フレーム配送

### 5.1 実 helper（`stream`、20 秒窓）

age = 画素に埋めたページ時刻から client がフレームを手にするまで。ページの
`requestAnimationFrame` から OnPaint までの待ちを含むので、転送だけではない。

| viewport | tab | 要求 fps | 配送 fps | 到着間隔 p50/p95 ms | age p50/p95 ms | drop | CPU% helper tree / client | PSS 窓内 peak MiB |
|---|---|---|---|---|---|---|---|---|
| 1280×720 | 1 | 1 | 1.0 | 1000 / 1000 | 1001 / 1002 | 0 | 0.4 / 0.8 | 337 |
| | 1 | 10 | 10.0 | 100 / 100.4 | 1.7 / 2.0 | 0 | 2.9 / 1.5 | 336 |
| | 1 | 30 | 30.0 | 33.3 / 33.7 | 1.5 / 1.8 | 0 | 7.4 / 2.4 | 337 |
| | 1 | 60 | 60.0 | 16.7 / 17.0 | 1.4 / 1.9 | 0 | 13.0 / 3.6 | 338 |
| | 4 | 10 | 40.0 | 17.9 / 49.6 | 1.7 / 2.1 | 0 | 10.9 / 3.0 | 448 |
| | 4 | 30 | 119.0（要求 120） | 11.4 / 16.9 | 2.2 / 2.9 | 20（lost 19） | 35.9 / 7.0 | 449 |
| 3840×2160 | 1 | 30 | 30.0（949MiB/s） | 33.3 / 34.3 | 48.9 / 49.9 | 0 | 73.0 / 13.4 | 538 |
| | 4 | 30 | **77.5**（要求 120） | 12.6 / 14.1 | 150 / 157 | 32（lost 30） | 220.3 / 34.4 | 1439 |

- 1280×720 では、60fps まで要求どおりに届き、drop は 0。age は p95 でも 2〜3ms。
  到着間隔の p95 は 30fps で 33.7ms。
- 1fps では、ページの描画自体が 1 秒間隔になり、age が 1001ms になる。また
  1fps で開く `open_artifact` は **4.0 秒**かかった（最初の描画が 1fps の上限に
  待たされる。10fps で 320ms、30fps で 118ms）。
- 4K×1 tab×30fps は配送できる。helper tree の CPU は 1 論理 CPU の約 73%
  （software 描画）。age 約 49ms の大半は 4K 描画と copy で、転送は §5.2 のとおり
  7ms 前後。
- 4K×4 tab×30fps は配送が飽和する（77.5fps、helper 220%、age 150ms）。
- 複数 tab のとき、client の frame slot は 1 つなので、tab 間でフレームが
  上書きされる。4 tab×720p×30fps で 20/2,400（0.8%）が落ちた。
- メモリは 20 秒の間、増え続けなかった（PSS の peak が開始値より 8〜16MiB 高い
  程度）。client 側 RSS は 10MiB（4K で 67MiB）で一定。
- 停止は全ケースで ack 後 22–27ms（4K×4 のみ reap まで 53ms）、残留 0。

### 5.2 IPC だけ（`ipc`、合成 producer、同一 process）

publish → client が読める時点まで（CEF は含まない）。

| viewport | tab | 要求 fps | 配送 fps | MiB/s | latency p50/p95/p99 ms | drop | process CPU% | process RSS peak MiB |
|---|---|---|---|---|---|---|---|---|
| 1280×720 | 1 | 1 | 1.05 | 3.7 | 0.71 / 1.23 / 1.33 | 0 | 0.2 | 17 |
| | 1 | 10 | 10.05 | 35 | 0.85 / 1.51 / 2.17 | 0 | 1.3 | 17 |
| | 1 | 30 | 30.05 | 106 | **0.83 / 1.21 / 2.4** | 0 | 3.8 | 17 |
| | 1 | 60 | 60.05 | 211 | 0.85 / 1.15 / 1.48 | 0 | 7.8 | 17 |
| | 4 | 30 | 119.9 | 421 | 1.7 / 9.5 / 14.0 | 1 | 19 | 38 |
| | 1 | 飽和 | 2,882 | **10,134** | 0.43 / 0.55 / 0.79 | 10 | 277 | 29 |
| 3840×2160 | 1 | 30 | 30.05 | 951 | **6.1 / 7.1 / 8.2** | 0 | 29 | 130 |
| | 4 | 30 | 100.8（要求 120） | 3,189 | 17.5 / 64.8 / 77.5 | 0 | 165 | 383 |
| | 1 | 飽和 | 212.6 | 6,728 | 8.8 / 10.8 / 12.8 | 0 | 278 | 193 |

- 30fps の転送 p95 は 720p で 1.2ms、4K で 7.1ms。**基準の 33ms 以内**を、4K でも
  大きく下回る。socket は 1 本で 10GiB/s（720p）まで出る。
- 4K×4 tab×30fps（3.2GiB/s）だけは p95 が 65ms になる。producer の memcpy が
  同じ process で CPU を取り合う合成試験の上限であり、現実の負荷ではない。
- 飽和側の `drop` は少ないが、`seq lost`（queue が新しいフレームに置き換えた
  分）が大量に出る。queue が詰まらずに新しい方へ置き換わることを示す。

### 5.3 遅い consumer（`ipc`、`hold_ms=200`、60fps 要求）

| viewport | 配送 fps | queue による置き換え（client drop） | process RSS（start → peak） |
|---|---|---|---|
| 1280×720 | 4.99 | 1,101 | 3 → 21MiB |
| 3840×2160 | 4.99 | 1,088 | 3 → 193MiB |

consumer が 200ms 止まっても、producer は止まらず、古いフレームが捨てられる。
メモリは頭打ち（4K で 193MiB は 33MiB のフレームバッファ数枚分）。基準の
「queue は上限付きでメモリが増え続けない」を満たす。

## 6. 16a-2 への提案

1. **helper は事前起動せず、必要になった時に起動する。** 起動は warm で spawn →
   最初のフレームまで約 230ms（うち CEF 初期化 110ms、`open_artifact` 118ms）。
   一方、常駐は tab が無くても 8 process・PSS 242MiB を食う。起動を待たせる側の
   コストは artifact を開く 1 回に 0.23 秒で、常駐のコストより小さい。最後の
   tab を閉じたら idle timeout（例: 30〜60 秒）で停止する。cold のときは最初の
   `Hello` が約 1 秒になりうるので、`Hello` の待ちは 1 秒より十分長く取る。
   コアの起動時に先に温めるなら、Admission を通さずに常駐させないこと。
2. **helper は core に 1 つ、tab は共有する。** 2 tab 目以降の追加は PSS +30MiB
   （720p）だが、helper 1 個の基礎は 242MiB。tab ごとに helper を立てない。
   `MAX_TABS = 4` は妥当（5 個目は `Busy` で拒否されることを確認した）。
3. **Admission の見積もりは「基礎 + tab × 画素数」で出す。**
   基礎 PSS 約 250MiB、tab あたり約 30MiB/Mpix（最初の tab は GPU/renderer の分で
   さらに +60MiB 程度）。720p×4 で約 430MiB、4K×4 で約 1.3GiB（RSS は 2.6GiB）。
   予約は PSS 基準で、1 tab 720p なら 350MiB、720p×4 なら 512MiB、4K を含むなら
   1.5GiB を目安にする。
4. **SpawnLedger の process 数は 1 helper あたり 16 を上限に。** 実測は tab 0 で 8、
   4 tab で 13（1 tab 目 +2、以降 +1/tab）。
5. **停止は `Shutdown` → 期限で process group への SIGKILL。** orderly は ack 2ms、
   終了 25–30ms、子孫 reap まで 30–50ms。SIGKILL は 9ms。いずれも group の全員を
   reap できた（コアは subreaper になるか、group 単位で待つこと）。期限は
   1 秒もあれば十分。
6. **frame rate と解像度に予算を置く。** 既定は 30fps。集計で
   約 250Mpix/s（4K×1 tab×30fps、または 720p×9 tab 相当）を上限にする。
   これを超える 4K×4 tab×30fps は配送が飽和した（77.5fps、helper 220%、age 150ms）。
   背景 tab は 10fps に下げる。helper の CPU は 720p×30fps で 1 論理 CPU の約 7%、
   4K×30fps で約 73%。
7. **フレームを開く fps は 10 以上にする。** 1fps で `open_artifact` を開くと 4 秒
   かかる。低い rate が欲しいなら、10fps 以上で開き client 側で間引くか、
   frame rate を変える request を足す（protocol の拡張）。
8. **client の frame slot を tab ごとにする（16a-2 か 16b）。** 今は 1 slot なので
   複数 tab でフレームが tab 間で上書きされる（720p×4 tab×30fps で 0.8% 落ちる）。
   前景 tab だけを stream するなら不要。
9. **大きい tab を閉じたら helper を再起動する。** 4K の tab を閉じても PSS は
   709MiB 残る（起動直後は 243MiB）。最後の tab が閉じたら、4K を使った helper は
   停止して次回に起動し直す（再起動は 0.23 秒）。常駐させる helper には 1 回あたり
   の画素数の上限を置いてもよい。
10. **転送は Unix socket + raw BGRA のまま確定。** 30fps の p95 は 720p で 1.2ms、
    4K で 7.1ms と、基準 33ms の 1/5 以下。共有メモリは不要。4K×30fps の
    949MiB/s でも client の CPU は 13%。
11. 16a-2 の `core_renderer_cache` E2E で確認する項目: helper の予定再起動後の
    artifact 再 open、停止後の残留 process 0、lock FD と credential を helper が
    継承しないこと（#610 の申し送りのとおり）。

## 7. 未実施・未解決

- **native GUI との同時稼働は測っていない。16b の段階で測る。** spike の
  off-screen の GUI harness での共存は §2 のとおり（衝突なし）。ウィンドウを開く
  計測は別途確認が要る。
- production の core からの spawn、Admission/SpawnLedger 経由の起動は 16a-2。
  この文書の起動時間は、コアの手前（`RendererClient` の直接起動）の値。
- 長時間 soak は 120 秒まで（spike）。20 秒窓の本計測では、メモリは増えなかった。
  数時間の soak は未実施。
- macOS/Windows は未実施。process 計測は Linux の `/proc` 前提。
- helper は dev profile。CEF 本体は prebuilt なので差は小さいはずだが、release の
  数値は取っていない。
- 計測は 1 回ずつで、共有 workstation の負荷（loadavg 3〜14）の影響を受けた。
  配送 fps と age は再現性が高いが、CPU%、飽和側の p95 は目安。
- cold start（ページキャッシュを落とした起動）は、キャッシュを落とせないため
  再現手順に含めていない。
- helper の frame 配送の p50/p95（`stream`）は、ページ時刻から測った age で、helper
  内の OnPaint 時刻を取っていない。転送だけの値は `ipc` の合成 producer で出した。
