# ADR 0034: ブラウザ基盤を用途で分ける（HTMLプレビューはCEF OSR、agentの閲覧は外部Chromium + CDP）

## Status

Accepted（2026-10-18）。利用者との設計相談で方針に合意した。[ADR 0029](0029-conversation-artifacts.md) が保留した「egui内でのwebview埋め込みと、会話内での対話的なHTML表示」を閉じる。ADR 0029 段階2の「HTMLはChromiumのスナップショットを撮り、操作はBrowser paneで行う」のうち、Browser paneの実体をこのADRで決める。→ 2026-10-22 に、利用者との相談で配布の方針を変更した。Nixの既定パッケージ（x86_64-linux）はCEFを含み、`gui`の`browser` featureは既定で有効にする。cargoの`cef` featureはopt-inのまま残す（下記「配布」）。→ 2026-10-23 に、利用者との相談で手動のBrowser windowを廃止し、Agent browserはwindowから会話ごとのdockタブに移した（[docs/browser.md](../../../docs/browser.md)）。

## Context

2026-10-17時点の部品は次のとおりである。

- opt-inの`browser` feature（`crates/gui/src/browser/`、[docs/browser.md](../../../docs/browser.md)）は、chromiumoxideで外部のChromiumを起動し、CDPのscreencastで届くJPEGをeguiのtextureに描く。操作はHTTP(S)へのNavigateとCSSセレクタのClickだけで、マウス・キー・IMEは転送しない。浮動windowとして開き、agentからは使えない。
- `present`したHTMLは、カードの「Open」でXDG Desktop Portal経由の外部アプリに渡すだけである（`crates/gui/src/model/artifact_opener.rs`）。
- agentが使えるWebツールは`web_search`と`web_fetch`だけである。
- tool結果は`ToolResultContent::Text`だけで、MCPの画像も`McpToolResult::text()`で捨てている。

求めるものは2つある。

- **A: agentのブラウザ操作。** navigate・click・type・scroll・screenshot・ページ構造の取得をtoolとして持たせ、結果（画像を含む）をmodelへ返す。
- **B: 利用者が`present`されたHTMLをアプリ内で操作できること。** 表示はoffline（ネットワーク遮断）で行う。

### 候補の比較（2026-10-17）

| 候補 | 判断 | 理由 |
|---|---|---|
| wry / WebKitGTK | 不採用 | `build_as_child`はX11専用で、Waylandのeframe windowには埋め込めない。CDPがなく、Aは独自のJS操作になる。依存が約0.9 GB増える |
| Servo | 保留 | AとBの両方を満たしうるが、毎月のAPI破壊、mozjsのビルド、GLとwgpuの同居、実ページ互換に課題がある |
| Blitz | 保留 | wgpu 30でeframeと相性がよいが、JSを実行できない |
| 既存のCDP screencastを拡張 | Aに採用 | 試作で検証（下記） |
| CEF OSR（tauri-apps/cef-rs） | Bに採用 | 試作で検証（下記） |

### 試作の結果

同じfixture（ネイティブのpopupとIMEの`controls.html`、遮断を検査する`leak.html`、agent操作シナリオの`app.html`）で、releaseビルドで自動計測した。計測環境はRyzen 9 7900、Chromium 154、niri（Wayland）。

| 項目 | CDP screencast | CEF OSR |
|---|---|---|
| `<select>`・date・colorのpopup | 映らない | textureに描画される |
| HiDPI | screencastはCSS pxの解像度しか出さない。静止後150 msで2xのscreenshotに差し替えても、動いている間はぼやける | 常に2xで描画される |
| fps | 60 | 60 |
| CPU（アニメーション中） | Chromiumで1コアの63〜73% | プロセスツリー全体で5%（2xのスクロールは49%） |
| クリックから表示まで | p50 12.3 ms | p50 16.5 ms（約1フレーム） |
| メモリ（ページ1枚、PSS） | 544 MiB | 335 MB |
| CDP経由のIME | 変換・確定は正しい。変換中のEnterがページに届くので、埋め込み側で止める必要がある | 変換・確定は正しい。確定のEnterはページに届かない |
| offline表示の遮断 | 外部への接続0件。WebRTCのUDPはprofileの`webrtc.ip_handling_policy`でしか止まらない | 外部へのTCP・UDPは0件。WebRTCはrendererでAPIを削除して止めた |
| 通常ブラウジングとの同居 | 1つのChromiumの中でbrowser contextを分けられる | 不可。CEF 154はrequest context単位のproxyとWebRTCの設定を無視する |
| agentシナリオ（AX snapshotとref） | 4/4成功。snapshotは約54〜421 token | 4/4成功。snapshotは約108〜336 token |
| クラッシュ | 別プロセスなのでGUIに影響しない | rendererをkillしてもGUIは残り、reload 31 msで復帰する。browserプロセスはGUIと同じプロセスで動く |
| 配布 | 外部のChromiumを使う | 約310〜375 MB（strip後）。cef-rsはCEFのversionと`=`で固定される |

CEFは`ozone-platform=headless`ならWayland、X11、ディスプレイなしのどれでも初期化できた。GPU経路は、dev shellのglibcと`/run/opengl-driver`のmesaが合わず評価できていない。計測はGPUを無効にして行った。

## Decision

### 用途でエンジンを分ける

- **B（HTMLプレビュー）はCEF OSRをeguiに埋め込む。**
  - CEFの描画（OnPaintのCPUバッファ）をwgpuのtextureに書き込み、eguiのpaneに表示する。マウス・ホイール・キー・IME・カーソル・popup・JSダイアログ・クリップボードはevorchが転送する。
  - クリップボードはページからホストへのコピーだけを渡す（2026-10-19）。headlessのCEFは自前のクリップボードを持つので、各ページのcontextにページのscriptより先に入れるhookが、ユーザー操作に伴うコピーを拾ってbrowserプロセスに送る。対象はcopy・cutイベント（`element.select()`後のCtrl+Cと`execCommand('copy')`を含む）と`navigator.clipboard.writeText`である。ページはホストのクリップボードを読めない。
  - 利用者が見ていないagent用のページでは、JSダイアログを誰にも出さない。alertは閉じ、confirmとpromptは取り消し、tool結果に記録する。
  - このCEFは`present`されたartifactの表示とagentによるartifactの確認だけに使い、通常のWeb閲覧には使わない。CEF 154ではcontext単位のネットワーク設定が効かないためである。
  - 遮断は多層で行う。
    - artifactは架空のoriginで、scheme handlerから配信する。
    - それ以外のrequestはすべて取り消す。
    - CSPを付ける。
    - プロセス全体のproxyを到達不能な値にする。
    - WebRTCのAPIは削除する。
  - 遮断を検査するテスト（`leak.html`と同等）を常設する。
- **A（agentの閲覧）は外部のChromiumをCDPで操作する。**
  - 既存の`browser` featureの実装（chromiumoxide）を土台にする。headlessで起動し、信頼できないWebページをGUIとは別のプロセスに閉じ込める。
  - 到達できる先は`web_fetch`と同じネットワーク方針（network guard）に従わせ、CDPのFetchドメインで強制する。
  - 利用者はscreencastのpaneでagentの閲覧を観察できる。このpaneは観察用で、ネイティブのpopupが映らない制約は受け入れる。
- **agent用toolの層は1つにし、CDPの送り先を差し替えられるようにする。**
  - ページ構造は`Accessibility.getFullAXTree`から作ったsnapshotに要素ごとの`ref`を付けて返す。操作は`ref`で指定する。座標による操作はopt-inにする。CSSセレクタをmodelに推測させない。
  - 送り先は外部Chromium（chromiumoxide）とCEFのプロセス内DevTools（`execute_dev_tools_method`）の2つとする。agentが自分の作ったHTMLを確かめる用途（ADR 0029 段階3）は、CEFを送り先にしてofflineで行う。

### 前提として先に行うこと

- tool結果に画像を加える。providerごとの差は[ADR 0020](0020-canonical-message-normalization.md)の正規化で吸収する。MCPの画像も捨てずに渡す。
- ブラウザのセッション管理を`crates/gui`から独立したcrateに切り出し、runtimeとtoolsから使えるようにする。

### 会話のカード

- HTMLのカードは静的なスナップショットを表示する。スナップショットはCEFで撮る。
- カードを開くと、アプリ内のCEF paneで対話的に操作できる。
- CEFを含まないビルドでは、これまでどおりPortalで外部アプリに渡す。
- 会話のスクロールの中に動くブラウザを複数並べることはしない。1つのブラウザごとにrendererプロセスが付き、メモリが膨らむためである。

### 配布

- CEFは当面cargo featureでopt-inにする。
  - 2026-10-22改訂: cargoの`cef` featureはopt-inのまま残す。既定で有効にすると、Nixの外のcargoビルド（CIの各jobと各開発環境）がCEF（展開後約1.8 GB）の取得を必要とするためである。
  - 代わりにNixの既定パッケージ（`packages.default`と`evorch-gui`）を、x86_64-linuxではCEF入りにする。CLIなどは`evorch`から取り、`evorch-gui`は`evorch-gui-cef`を起動する。CEFの配布物がない他のsystemでは、これまでの`evorch`のままとする。CEFを含まない版は`.#evorch`で引き続き取得できる。
  - `gui`の`browser` feature（Agent browserの表示。当初はBrowser windowも含んだが2026-10-23に廃止）は既定で有効にする。`runtime`と`tools`がすでに`browser` crateに依存しているので、依存は増えない。`cef` featureも`browser`を有効にする。
- Nixでは、cef-rsが固定するversionのCEF配布物を`fetchurl`でhashを固定して取得する。`libcef.so`をstripし、localeを絞る。helperバイナリをevorch-guiと同じディレクトリに置く。
- SUIDの`chrome-sandbox`は使わず、user namespaceのsandboxに頼る。
- GPUは既定で無効にし、GPU経路は後で評価する。

## Consequences

- 利用者は`present`されたHTMLモックを、本物のChromiumとしてアプリ内で操作できる。popup、HiDPI、IME、CPU負荷のどれも、screencast方式より良い。
- agentはrefを使った安定した操作でWebを閲覧でき、閲覧するページはGUIと別のプロセスに閉じ込められる。
- Chromiumが2系統になる。同梱するCEFと、外部のChromiumである。
- 配布物が約310〜375 MB増える。
- CEFの更新への追従が月に1回ほど、1回0.5〜1日かかる。cef-rsの`accelerated_osr`はwgpuのversionを固定するので、eguiの更新でCEF側の追従を待つことがある。
- CEFのbrowserプロセスはGUIと同じプロセスで動くので、libcefの異常終了はGUIごと落ちる。
- 工数の見込みは次のとおりである。
  - Bの製品化: 3〜4週
  - Aの製品化: 2〜3週
  - 共通の前提（tool結果の画像化とcrateの切り出し）: 1〜2週

## Open questions

- niri上のfcitx5で、CEFでの実際の日本語入力（変換中の表示、候補窓の位置、確定のEnter）が正しく動くか。利用者が実機で確かめる。
- GPU経路（dmabufの取り込み）を使うかどうか。dev shellのglibcとmesaの不整合を解消してから評価する。
- CEFをsidecarプロセスに分けて、GUIを巻き込まないようにするか。
