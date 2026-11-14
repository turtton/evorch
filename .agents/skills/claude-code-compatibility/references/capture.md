# 隔離した通信採取

採取が必要な面だけ読む。native CLI の起動と mock の振る舞いは対象 version で確認する。
古い一時採取スクリプトにある絶対パス・認証・保存方式を、そのまま再利用しない。

## 起動前の隔離

repo 外にアクセス制限した一時ディレクトリを用意し、mock server、専用 CA/cert、
synthetic workspace、`CLAUDE_CONFIG_DIR`、出力先をそこへ置く。必要な raw 資料の
保存理由と破棄・保持範囲を決める。環境変数は allowlist で構成し、dummy OAuth token
を明示する。通常 home、credential/config、keyring socket を見せない。

外向き通信の遮断と loopback 接続を OS/container 側で担保する。macOS/Windows では同等の
隔離環境を用意し、用意できなければ採取を未実施として静的解析までに留める。proxy 設定だけで安全と
みなさない。

### Linux: 最小 launcher と positive control

ホストに必要な道具（bubblewrap、iproute2、Node、OpenSSL 等）が無くても、特権や global install
なしで Nix の一時ツール（`nix run` / `nix shell`）から揃えられる。sudo、システム設定の変更、
通常 profile への追加はしない。rate limit 等で取得できなければ、そこで採取を未実施にして理由を残す。

launcher は **レビュー可能な短さ**にし、内容 hash を証跡に残す。`--ro-bind / /` で host 全体を
見せるより、Nix store の read-only 公開と今回専用の scratch だけを公開する形を優先する
（2.1.322 の採取で動作を確認した形。`$native` はレビュー済み binary、`$node` は `readlink -f` した実体）:

```sh
bwrap --unshare-user --unshare-net --unshare-pid --unshare-ipc --unshare-uts \
  --die-with-parent --new-session \
  --ro-bind /nix/store /nix/store --proc /proc --dev /dev \
  --tmpfs /tmp --dir /home --dir /root --dir /run --dir /etc \
  --bind "$analysis_scratch" "$analysis_scratch" \
  --ro-bind "$native" /native/claude \
  --chdir "$analysis_scratch/work" --clearenv \
  --setenv PATH "$analysis_tool_path" --setenv HOME "$analysis_scratch/home" \
  --setenv LANG C.UTF-8 --setenv TMPDIR "$analysis_scratch/private" \
  -- "$analysis_node_binary" "$analysis_scratch/harness.mjs" "$@"
```

user/network/PID namespace により、非特権でも host process の環境・root・他の scratch への
参照と外向き経路が消える。host の `/home`、`/root`、`~/.claude`、`~/.config`、credential、
socket、repo、他作業の資料は mount しない。環境は `--clearenv` と allowlist で渡す。

**positive control を CLI/mock の起動より先に実行し、不合格なら throw して起動しない。**
同じ namespace 内の harness の先頭で毎回次を検査し、結果を証跡へ保存する。

| Control | 合格条件 |
| --- | --- |
| interface | `lo` のみで UP |
| IPv4/IPv6 route | ともに空 |
| loopback listener への TCP | 成功 |
| 文書化用アドレス（例 `192.0.2.1:443`、`[2001:db8::1]:443`）への直接接続 | `ENETUNREACH` |

namespace 内に `lo` 以外が見えるか route がある場合は、採取を中止する。proxy の許可リストや
loopback 接続の成功だけでは外向き通信の遮断を示せない。capture は response/stream/process の
終了イベントで完了させ、外側の watchdog 期限を超えた場合は測定不能とする。期限切れを成功や
再開なしの証拠にしない。

### Nix で入手した native の起動

`npm` の wrapper を使わず、取得した platform package の native binary を直接実行する。Nix store
の glibc の動的 loader を明示すると、host の `/lib64` が無くても動く:
`"$glibc/lib/ld-linux-x86-64.so.2" /native/claude ...`（loader は store の実体を確認して指定。
過去の store path を転記しない）。wrapper を外すと失われる sandbox/環境を意識し、起動して得た
version と binary SHA-256 を一緒に記録する。registry の tarball integrity は **tarball** の digest で
あり、展開した native binary の SHA-256 とは別物。pin には両方を区別して書く。

### interactive の PTY 採取

interactive の main request は PTY で採る。手入力の自動化（起動後に文字列を書き込んで Enter）は、
terminal protocol や入力受付のタイミングで**送信が成立しないことがある**（2.1.322 の最初の 2 試行は
Messages 0 件）。初期 prompt を **CLI の引数**で渡し、PTY は画面とプロセスを維持する役目に絞る
ほうが再現しやすい。送信が成立したかは UI の見た目でなく、mock が受けた Messages で確認する。
成功した採取でも、SSE の完了とプロセスの意図的な終了（UI を閉じる操作）を、watchdog の期限切れと
区別して記録する。

### synthetic settings と feature flags

settings/policy/model 等への synthetic 応答（`{}` など）は、実アカウントの remote feature flags、
plan、managed policy を再現しない。UI に managed settings の parse failure が出ることもある。
採取した request に beta や field が無くても、それは「この mock 条件で送らなかった」にとどまり、
実アカウントが決して送らないことを示さない。送信条件は静的解析で別に確かめ、未測定として残す。

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
