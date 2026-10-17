# 隔離した通信採取

採取が必要な面だけ読む。native CLI の起動と mock の振る舞いは対象 version で確認する。
古い一時採取スクリプトにある絶対パス・認証・保存方式を、そのまま再利用しない。

## 起動前の隔離

repo 外にアクセス制限した一時ディレクトリを用意し、mock server、専用 CA/cert、
synthetic workspace、`CLAUDE_CONFIG_DIR`、出力先をそこへ置く。必要な raw 資料の
保存理由と破棄・保持範囲を決める。環境変数は allowlist で構成し、dummy OAuth token
を明示する。通常 home、credential/config、keyring socket を見せない。

外向き通信の遮断と loopback 接続を OS/container 側で担保する。Linux では network
namespace と bwrap を使える。macOS/Windows では同等の隔離環境を用意し、用意できなければ
採取を未実施として静的解析までに留める。proxy 設定だけで安全とみなさない。

Linux で capture server と CLI を同じ namespace 内に置く場合の launcher の形:

```sh
bwrap --unshare-net --unshare-pid --ro-bind / / \
  --tmpfs /home --tmpfs /root --tmpfs /run/user \
  --tmpfs /tmp --tmpfs /var/tmp \
  --bind "$analysis_scratch" "$analysis_scratch" \
  --proc /proc --dev /dev --die-with-parent \
  --chdir "$analysis_scratch" \
  env -i PATH="$analysis_tool_path" LANG=C.UTF-8 \
  "$analysis_node_binary" "$analysis_scratch/capture.mjs"
```

PID namespace と専用 `/proc` を分離して host process の環境・root への参照を防ぎ、
host の一時ファイルも隠して scratch だけを再公開する。
これは環境に合わせて作る launcher の例であり、`capture.mjs` は対象版の mock と採取器を
用意する。実 home の位置と executable の依存が mount 設定に合うこと、namespace 内に
loopback 以外の interface/外向き route がないことを起動時に検査する。
capture は response/stream/process の終了イベントで完了させ、外側の watchdog 期限を
超えた場合は測定不能とする。期限切れを成功や再開なしの証拠にしない。

## Official host の TLS mock

1. `api.anthropic.com` を SAN に持つ専用 cert を作り、HTTPS listener を loopback に限定する。専用 CA を `NODE_EXTRA_CA_CERTS` で追加し、TLS/hostname 検証を無効化しない。
2. CONNECT proxy は `api.anthropic.com:443` だけを受理し、DNS に渡さず固定 loopback listener へ転送する。他の宛先を拒否し、CONNECT に OAuth Authorization が漏れていないことを確認する。
3. CLI child へ dummy OAuth、隔離 config、synthetic workspace、proxy と専用 CA だけを渡す。native wrapper が必要とする sandbox は維持する。
4. mock は model endpoint のほか、対象版が必要とする settings/policy/model/token-count 等へ synthetic response を返す。未知の外部通信へ実接続を許可しない。

別 host の `ANTHROPIC_BASE_URL` だけで採取すると、official-host 分岐を通らない可能性がある。
一方 `--bare`、print/SDK mode、`--safe-mode`、`--setting-sources` の効果も版ごとに違う。
以前の CLI flag だけで onboarding/trust/managed settings や外部通信が無効になるとは
みなさず、隔離 config と実際の request で確認する。

## Capture matrix

全ケースを無条件に起動せず、今回の差分に必要な面を選ぶ。可能なら同じ process の
自然な tool loop を使い、request class と操作から要求を識別する。

| ケース | synthetic 操作と確認する点 |
| --- | --- |
| interactive main | onboarding/trust 済みの隔離 UI に既知の短い入力。main request を auxiliary title/settings と分ける |
| tool continuation | mock が native schema に合うダミー Read 等を返し、tool result 後の要求と prompt/previous response IDs を見る |
| first child | main に Agent tool use を返し、child が送る自身の ID と parent header の有無を見る |
| nested child | child に Agent tool use を返し、親 subagent と root の扱いを区別する |
| compaction | 対象版で確認した操作で誘発し、child 内も含め class override と保存される ancestry を見る |
| auxiliary / print / SDK | 対象 mode を明示して別ケースとして採る。interactive main の代わりにはしない |

mock response の tool 名・引数は、その版の request に含まれる schema を一時解析して選ぶ。
SSE は `message_start`、block start/delta/stop、`message_delta`、`message_stop` を正しく終え、
tool input の fragment を JSON へ復元できるようにする。tool ID・response IDs を一貫させる。
固定の「何件目」だけでは auxiliary を誤認するので、class・tool input・connection の対応も見る。
必要な終端まで届かなければ、そのケースは未完了と記録する。

## 最小限の証跡

header は **raw wire の casing/order** と要求種別を残す。値を JSON object に入れて
並べ直した出力は wire 順序の根拠にならない。IDs は存在・形式・等値関係を記録し、
値自体や token を公開しない。body はメモリ内で最終バイト checksum と構造を調べるのを
優先し、必要な一時保存は repo 外へ限定する。bundled prompt/tool 本文、rawHeaders、
CLI stdout/stderr の全量を fixture にコピーしない。

fixture に残すのは、検証済みの正規化 TLS bytes、header 名/順序、独立 synthetic vector、
version/hash/OS/操作/抽出方法などの provenance。Rust の出力から native fixture を作らない。
各 request の TLS connection ID/port と採取した ClientHello を対応づける。
最初の settings GET の Hello を main Messages の Hello と取り違えない。

## TLS のオフライン比較

採取器は client 方向の TCP chunk を TLS record に再構成し、複数 record にまたがる
Handshake も組み立てる。最初の TCP chunk/record だけを ClientHello とみなさない。
1 接続の未暗号化 ClientHello を含む TLS records を、完全な平文 Handshake の境界まで
repo 外のファイルに抽出し、次の helper で調べる。HTTP body/capture JSON や、TLS 1.2 の
CCS 後の暗号化 Handshake を含む全接続 stream は入力対象外である。復号処理は行わない。

```sh
analysis_skill=.agents/skills/claude-code-compatibility
node "$analysis_skill/scripts/inspect-client-hello.mjs" "$analysis_native_tls"
node "$analysis_skill/scripts/inspect-client-hello.mjs" \
  "$analysis_native_tls" "$analysis_evorch_tls"
node --test "$analysis_skill/scripts/inspect-client-hello.test.mjs"
```

Node 20 以降の built-in modules だけで動作し、通信・CLI 起動・認証読込み・fixture 書込みは行わない。
比較は normalized ClientHello と record 境界の両方を示す。exit 0 は解析/比較成功、1 は差分、
2 は使い方または不完全/不正な入力。TCP 分割は同じ内容へ復元され、TLS record の分割差は残る。

実行環境が子プロセスの test 出力を取得できず、`node --test` がファイル単位の結果だけを
表示する場合、`node "$analysis_skill/scripts/inspect-client-hello.test.mjs"` を直接実行して
各ケースの結果を確認できる。CI の `node --test` は失敗 exit code も検出する。

通常は random、legacy session ID、key-share の鍵値だけを正規化する。
cipher/group/version/ALPN、vector の長さ・個数、extension 順序、未知 extension と record 構造を
維持する。新しい extension を「変動するようだ」という理由で丸ごと除外しない。

## Pooling と TLS resumption

同じ TCP 接続の再利用と、新 TCP 接続での TLS session 再開を別々に測る。
再開の比較が必要な場合は、同じ HTTPS server/cert/ticket keys を維持し、同じ client で
完全に response を drain してから `Connection: close` による正常終了と新接続を観測する。
server の TLS version/cipher/ALPN と `isSessionReused()` を connection ごとに記録する。

先に、再開対応の control client が同じ server で実際に再開できることを確かめる。
PSK extension の存在だけで成功としない。production と PoC の TLS 設定、client の寿命、
session-cache 設定が同じかを照合してから、PoC の結果を採用する。

PSK ticket/age/binder の値を除いた構造を比較すると決めた場合だけ、helper に
`--normalize-psk` を付ける。各 vector の長さ・個数や extension の位置は保持される。
これを fresh-handshake 比較の既定除外に混ぜない。

| 結果 | 言えること |
| --- | --- |
| control 成功、双方再開、構造一致 | この観測条件で再開動作と構造が一致 |
| control 成功、一方のみ再開／構造差 | 再現した差分。production との条件を照合して修正を検討 |
| control 成功、双方非再開 | この条件では非再開。再開した場合の構造は未証明 |
| control 失敗、採取不足、connection 対応不能 | 測定不能。一致とは扱わない |

異なる TLS version、ticket 方針、失敗経路、OS、native version に結論を広げない。
実差分が出た場合だけ、対象 native と同じ Bun/runtime 等の追加 PoC で原因を切り分ける。
