# UI/UX デザインパターン調査レポート

evorch-gui の再設計に向けたデザインリサーチの統合レポート。OpenCode TUI、
t3code、Orca の 3 参照アプリと、egui テーマ基盤、Tokyo Night カラースキームの
調査結果を、実装者が直接参照できる形に整理する。数値と引用はすべて調査資料
(2026-09-14 実施、2 wave 完了) に基づき、外部の主張には出典 URL を付す。

## 1. エグゼクティブサマリ

本調査の目的は、evorch-gui の GUI を再設計する際の権威ある参照文書を作ること
にある。対象はカラーシステム、タイポグラフィ、情報密度、状態表示、テーマ基盤
の全般で、特に Tokyo Night テーマの導入を具体的なトークンマッピングまで
落とし込むことを目標とした。

3 つの参照アプリから得られた結論は一致している。OpenCode TUI は「色と
レイアウトを設定として分離」し、primary/error/warning 等のセマンティック
ロールを全テーマで漏れなく明示する規律を持つ [3][4]。t3code は
canvas/surface/textMuted/border 等のセマンティックトークンで画面を構成し、
カスタムテーマを OKLCH の 2 色から全面生成する [5]。Orca は公式 Style Guide
で "monochrome and quiet"、"color is reserved for state" と明文化し、
色を選択・状態・Git・破壊的操作に限定している [6][7]。

evorch の現状診断では、デザインの構造基盤は健全である。色・余白・角丸・
フォントが `crates/gui/src/theme/tokens.rs` に集約され、egui_dock のスタイルも
`theme/dock.rs` に一元化されている。一方で、現行 dark palette は t3code dark
palette と実質同一 (CANVAS #0a0a0a、SURFACE #111111、TEXT #f5f5f5、
TEXT_MUTED #818181、BORDER #191919、ACCENT #346bf1、SIDEBAR #000000) であり、
独自の視覚言語を持たないことが一次ソースの直接照合で確認された。

弱い点は 4 つある。ThemePreset が Graphite / HighContrast の 2 種のみで全
パレット preset 化の構造を持たないこと、`reload_theme` への UI 呼び出し
導線がないこと、テーマ選択の永続化がないこと、そして上述の独自性の欠如
である。

改善の方向性は、Tokyo Night (folke Night 変体) を新 preset として追加し、
OpenCode のロール割当規律にならって全トークンを preset 毎に明示する設計へ
移行することである。切替 UI は `Workbench settings` メニューへの追加が最小
変更で、永続化は `workspace-ui::UiSettings` への拡張が自然な接続点である。
詳細は第 6 章のマッピング表と第 7 章のロードマップを参照。

## 2. 現状診断: evorch のデザインシステム

### 2.1 健全な点

- トークン集約: 色・余白・角丸・フォントサイズがすべて
  `crates/gui/src/theme/tokens.rs` の `pub const` に集約されている
  (tokens.rs:5-72)。テーマ差替は原則このファイルで完結する。
- 4px グリッド: SP_1 から SP_4 (4/8/12/16) が定義され、
  `spacing_consts_are_multiples_of_four` テスト (tokens.rs:162-178) で
  ROW_COMPACT 36、ROW_DENSE 28、TAB_HEIGHT 24、TOPBAR 52 等の 4 の倍数
  性を機械的に保証している。光学補正の半値例外 (DOT_SIZE 10、TAB_GAP 2)
  はコメントで根拠が明記されている (tokens.rs:41-45)。
- dock スタイル集約: egui_dock の全スタイル (tab_bar / tab / separator /
  overlay / buttons) が `theme/dock.rs` の `dock_style()` 1 関数に集約
  されている (dock.rs:6-96)。attention tab は `attention_tab_style()`
  (dock.rs:98-105) で個別化する構造。
- Visuals 全面 mapping: `visuals_for()` (style.rs:19-74) が panel_fill、
  window_fill、selection、widgets 5 状態 (noninteractive/inactive/hovered/
  active/open)、shadow までをトークンから設定しており、背景だけ変えて
  default 色を残す失敗型には陥っていない。
- headless テスト基盤: egui_kittest による振る舞いテスト、
  `size_dpi_matrix` (4 状態 x 4 サイズ x 3 DPI)、wgpu オフスクリーン
  レンダ証跡 (`scripts/gui-evidence-matrix.sh` で 24 PNG 一括) が整備
  されている。詳細は `docs/gui-verification.md` を参照。
- 責務分離: workspace-ui は視覚層を持たない純粋状態層であり、テーマ
  変更は gui crate 内で完結する。

### 2.2 弱い点

- ThemePreset が 2 種のみ: `ThemePreset` は `Graphite` / `HighContrast`
  のみ (style.rs:9-13) で、分岐も選択背景 1 色だけ (style.rs:31-34)。
  全パレットを preset 化する構造ではない。
- 切替 UI なし: `reload_theme(ctx, preset)` は存在する (state.rs:269-273)
  が、UI からの呼び出し導線がない。
- 永続化なし: `theme_preset` は `WorkbenchState` 内で `Graphite` 固定
  (state.rs:71、state.rs:142)。evorch.toml / crates/config に UI テーマの
  schema が存在しない。
- 独自視覚言語の欠如: 現行 palette は t3code dark palette と実質同一
  (第 1 章参照)。`DESIGN.md` も "no redesign or new visual dependencies"
  (crates/gui/DESIGN.md、1. Direction) と再設計を明示的に保留してきた。
- Light テーマ未定義: `install_preset` は Light preference にも dark 設計を
  そのまま登録している (style.rs:137-143)。意図的なフォールバックだが、
  真の Light テーマは存在しない。

## 3. 参照アプリのデザイン言語分析

### 3.1 OpenCode TUI

設計思想: 「色とレイアウトを設定として分離」「terminal-native を尊重」
「diff はレビュー対象」。現行実装は OpenTUI (Zig) + SolidJS で、旧
bubbletea 版は性能・機能不足で v1.0.0 で置換された [4]。

トークン体系: 33 個の組込 JSON テーマを持ち、
`~/.config/opencode/themes/*.json` で拡張可能。tokyonight も組込済み [3]。
セマンティックロールは primary/secondary/accent/error/warning/success/info、
text/textMuted、background/backgroundPanel/backgroundElement、
border/borderActive/borderSubtle に加え、diff 系 13、markdown 系 14、
syntax 系 9、thinkingOpacity と網羅的である [3][4]。

特徴的 UX パターン: compressed transcript (edit/bash のみ詳細表示)、
Ctrl-P command bar への全操作集約、session sidebar、responsive diff
(幅 100 カラム未満で unified 表示)、spinner + pending tool count の圧縮
状態表示 [4]。

evorch が取り入れるべき点: 全ロールを全テーマで明示する規律 (欠落を
default 依存にしない)、テーマの JSON/設定ファイル分離、状態表示への
「意味あるステップ名」の採用。

### 3.2 t3code

設計思想: 「agent harness control surface」。Entity は pingdotgg/t3code
(Electron + React 19 + Tailwind 4)、信頼度 95% で確定 [5]。

トークン体系: canvas/chrome/toolbar/surface/surfaceRaised/surfaceOverlay/
text/textMuted/border/input/focus/accent/messageSurface/codeBackground/
sidebar*/terminal* のセマンティックトークン。dark palette は canvas
#0a0a0a、surface #111111、text #f5f5f5、muted #818181、border #191919、
accent #1b4ed8 (light) / #346bf1 (dark)、sidebar #000000 で、evorch の現行
palette はこれを踏襲している [5]。カスタムテーマは OKLCH で 2 色から全面
生成し、WCAG 二分探索でコントラストを確保する [5]。

特徴的 UX パターン: thread = 作業単位 (pinned/active/snoozed/settled)、
background thread (Cmd+Enter)、approval は会話内カード、progressive
disclosure、diff は turn/checkpoint 結合、PR までが thread の終点 [5]。

evorch が取り入れるべき点: thread 状態のライフサイクル表現 (evorch の
ThreadState に対応)、会話内 approval カード、surface 明度階層による深度
表現。評価として「calm and readable」とされる一方、diff の読みにくさが
批判されている点は反面教師とする [5]。

### 3.3 Orca

設計思想: 公式 Style Guide に "monochrome and quiet"、"neutral grays
carry the chrome"、"color is reserved for state" と明文化。色は選択・
状態・Git・破壊的操作のみに使い、shadow は 3 段階まで、独自 UI より
shadcn primitive を優先する [6][7]。Entity は stablyai/orca (Electron +
React + Tailwind 4 + shadcn + xterm + Monaco)、信頼度 95% [6]。

トークン体系: neutral gray が chrome を担い、semantic 色は状態専用。
状態 glyph は全画面で共有 (spinner / amber ? / green check / red dot /
gray dot) [6]。

特徴的 UX パターン: agent = 1 CLI agent x 1 terminal x 1 worktree、
Kanban Agent Dashboard (Needs You / Working / Done / Idle)、diff 行
コメントのバッチ送信 (1 round of thinking、1 revision pass)、Design
Mode [6]。

evorch が取り入れるべき点: 状態 glyph の全画面共有 (evorch の status
dot / phase pill に対応)、色の用途限定規律。issue #4395 で「機能は強いが
polish は Zed/T3 Code に劣る」と自己申告しており、panel hierarchy、tab
chrome、list density、focus rings、empty states、keyboard-first flows が
polish 要求項目として挙がっている点は、evorch が同じ罠を避けるための
チェックリストとして使える [8]。

## 4. 良いパターン集

各パターンは「パターン名 / 内容 / 根拠 / evorch への適用方針」の形式で
記す。

### 4.1 カラーシステム

- パターン名: セマンティックロールの全明示
  - 内容: primary/error/warning 等の全ロールを、全テーマで省略せず明示
    する。OpenCode の全テーマ (opencode/catppuccin/vesper) が全ロールを
    明示設定していた [3]。
  - 根拠: OpenCode tokyonight.json 全ロール明示 [3]。
  - 適用方針: ThemePreset を全トークンを持つ構造体化し、preset 毎に
    漏れなく定義する。部分分岐 (style.rs:31-34) を廃止する。
- パターン名: surface 明度階層による深度表現
  - 内容: shadow 過剰を避け、surface の明度階層で深度を表す。accent は
    少数限定、semantic 色と syntax 色を分離する [4][6]。
  - 根拠: Orca "shadow 3 段階まで" [6]、t3code の surface 階層 [5]。
  - 適用方針: CANVAS < SURFACE < SURFACE_RAISED < OVERLAY < INPUT の
    現行階層 (tokens.rs:5-13) を維持しつつ、Tokyo Night の階層色へ
    置き換える (第 6 章)。
- パターン名: 色は状態専用
  - 内容: chrome は neutral に任せ、色は選択・状態・Git・破壊的操作に
    限定する。
  - 根拠: Orca Style Guide "color is reserved for state" [7]。
  - 適用方針: ACCENT の使用箇所を監査し、装飾目的の色使いを状態色へ
    統合する。

### 4.2 タイポグラフィ

- パターン名: UI フォントと code フォントの分離
  - 内容: UI テキストとコードでフォントファミリを分ける。Zed は UI 16 /
    buffer 15 / agent response Inter を使い分ける [4]。
  - 根拠: Zed のフォント使い分け [4]、line-height 1.5、見出し
    semibold 推奨 [4]。
  - 適用方針: Proportional = Inter → NotoSansJP、Monospace =
    JetBrainsMono → NotoSansMonoCJK の fallback 順を `theme/fonts.rs`
    に採用する。現行の body 14 / small 12 / mono 12 (tokens.rs:65-67)
    は維持してよい。

### 4.3 情報密度と余白

- パターン名: progressive disclosure
  - 内容: 最終回答・作業過程・tool を別階層にし、詳細は要求時のみ
    展開する。
  - 根拠: t3code の progressive disclosure [5]、OpenCode の compressed
    transcript (edit/bash のみ詳細) [4]。
  - 適用方針: transcript の tool call を「実行中は展開、完了後は折畳」
    の既定にする。
- パターン名: 4px グリッドの機械的保証
  - 内容: 余白トークンを 4 の倍数に限定し、テストで強制する。
  - 根拠: evorch 自身の `spacing_consts_are_multiples_of_four`
    (tokens.rs:162-178) が先行実装として有効に機能している。
  - 適用方針: 現状維持。新トークン追加時も同テストに列挙する。

### 4.4 状態表示

- パターン名: 状態 glyph の全画面共有
  - 内容: spinner / amber ? / green check / red dot / gray dot の glyph
    体系を全画面で共有する [6]。
  - 根拠: Orca の状態 glyph 共有 [6]。
  - 適用方針: status_dot (widgets.rs:46-50)、phase pill、tab bullet の
    3 系統を 1 つの glyph/色規約に統一する。
- パターン名: 意味あるステップ名
  - 内容: spinner 単体ではなく「何をしているか」のステップ名を添える。
    OpenCode は spinner + pending tool count で圧縮表示する [4]。
  - 根拠: agent-UI 一般パターン「spinner ではなく意味あるステップ名」
    [4]。
  - 適用方針: `ui.spinner()` 単体の 3 箇所 (第 5 章参照) にフェーズ名
    を併記する。
- パターン名: 色だけで状態を表さない
  - 根拠: agent-UI 一般原則 [4]、evorch 自身の DESIGN.md「Status is
    expressed in text as well as color」(crates/gui/DESIGN.md)。
  - 適用方針: 現状維持 + glyph 統一で強化する。

### 4.5 チャット・ツールコール UX

- パターン名: tool call の状態機械表示
  - 内容: tool call を start/args/end/result の状態機械として扱い、
    実行中は展開、完了後は折畳む。
  - 根拠: agent-UI chat UX パターン [4]。
  - 適用方針: transcript_tool.rs の表示を状態機械ベースに整理する。
- パターン名: 条件付き auto-scroll
  - 内容: auto-scroll は最下部付近にいる場合のみ有効にする。
  - 根拠: agent-UI 一般パターン「auto-scroll は最下部付近のみ」 [4]。
  - 適用方針: 第 5 章の無条件 stick_to_bottom 指摘を参照。
- パターン名: 会話内 approval カード
  - 内容: 承認要求をモーダルではなく会話内カードとして表示する。
  - 根拠: t3code の approval カード [5]。
  - 適用方針: merge approval の会話 notice (crates/gui/docs/
    workbench-panels.md 参照) をカード化する将来案として保持する。

### 4.6 diff 表示

- パターン名: responsive diff
  - 内容: 幅 100 カラム未満では side-by-side から unified に切り替える。
  - 根拠: OpenCode の responsive diff [4]。
  - 適用方針: Diff パネルの幅に応じた unified/split 切替を検討する。
- パターン名: diff 専用ロールの分離
  - 内容: diff 用の色ロール (追加/削除/変更/背景) を semantic 色とは
    独立に定義する。OpenCode は diff 系 13 ロールを持つ [4]。
  - 根拠: OpenCode トークン体系 [3][4]。
  - 適用方針: Tokyo Night の git 色 (git.add #449dab、change #6183bb、
    delete #914c54) を diff 専用トークンとして採用する (第 6 章)。

### 4.7 テーマ基盤

- パターン名: semantic palette の Visuals 全面 mapping
  - 内容: 背景だけ変えて default 色を残さない。catppuccin-egui が 26 色
    の semantic palette を Visuals へ全面 mapping する参照実装 [10]。
  - 根拠: catppuccin/egui [10]、egui_styled / egui-stylesheet 等の複数
    実装の収束 [11][12][13][14]。
  - 適用方針: evorch の `visuals_for()` (style.rs:19-74) はすでにこの
    方式に近い。preset 構造体化で完全化する。
- パターン名: テーマの 4 層設計
  - 内容: tokens → global Style → component states → app primitives の
    4 層で設計する [11]。
  - 根拠: egui theming 調査 [11]。
  - 適用方針: 現行構造 (tokens.rs → style.rs → dock.rs/widgets.rs →
    panes/*) はすでに 4 層に対応している。層の越境 (pane 内での生色
    指定) を lint 的に監視する。

### 4.8 操作性

- パターン名: command bar への操作集約
  - 内容: 全操作を 1 つのコマンドバー (OpenCode は Ctrl-P) に集約する。
  - 根拠: OpenCode の Ctrl-P command bar [4]。
  - 適用方針: 現行キーバインドは 5 アクションのみ (Ctrl+1/2/3、
    Ctrl+S、Ctrl+Shift+R) で command palette は未実装。テーマ切替の
    KeyAction 追加 (第 7 章) を足がかりにする。
- パターン名: keyboard-first flows
  - 根拠: Orca issue #4395 の polish 要求項目 [8]。
  - 適用方針: 主要操作 (テーマ切替、pane 移動、承認) にキーボード
    導線を用意する。

## 5. アンチパターン集

形式は第 4 章と同じ。evorch コード内の該当可否を調査済みの範囲で明記
する。

- パターン名: default テーマの局所変更
  - 内容: Visuals::dark() の一部だけ書き換えて残りを default のまま
    にする [11]。
  - 根拠: egui theming anti-pattern [11]。
  - evorch 該当: 非該当。`visuals_for()` は全面 mapping している
    (style.rs:19-74)。
- パターン名: dark_mode フラグだけ
  - 内容: `visuals.dark_mode = true` のみでテーマ済みとみなす [11]。
  - 根拠: egui theming anti-pattern [11]。
  - evorch 該当: 非該当。dark_mode 設定 (style.rs:21) に加えて全色を
    明示している。
- パターン名: override_text_color 乱用
  - 内容: 箇所箇所でテキスト色を上書きし、トークン体系を破壊する [11]。
  - 根拠: egui theming anti-pattern [11]。
  - evorch 該当: 非該当。`markdown_render.rs:10` で `None` へのリセット
    を行うのみで、上書き用途の使用はない。
- パターン名: over-nested boxes
  - 内容: 全 UI を Frame で多重に囲み、視覚的ノイズと余白の不整合を
    生む [4][11]。
  - 根拠: agent-UI anti-pattern [4]、egui anti-pattern [11]。
  - evorch 該当: 軽度の注意対象。`card()` (widgets.rs:30-43) は
    surface_frame + 内側 Frame の 2 段構成。現状は機能しているが、
    新規 card 利用時はネスト段数を意識する。
- パターン名: badge soup
  - 内容: 1 行に 5 個以上の pill/badge を並べる [4]。
  - 根拠: agent-UI anti-pattern [4]。
  - evorch 該当: 非該当。phase_indicator は spinner + badge の 1 行
    1-2 個構成 (phase_indicator.rs:24、テスト phase_indicator.rs:101-116)。
- パターン名: color overuse
  - 内容: semantic 色の多用・重複で識別性を失わせる [4]。
  - 根拠: agent-UI anti-pattern [4]。
  - evorch 該当: 一部該当。`WAITING` は `INFO` のエイリアス
    (tokens.rs:28) で、テストも `assert_eq!(WAITING, INFO)` を仕様と
    して固定している (tokens.rs:130)。Waiting と Running/情報表示が
    同色になるため、Tokyo Night 導入時に色分離を検討する。
- パターン名: spinner だけの状態表示
  - 内容: 何が起きているかを示さず spinner のみを出す [4]。
  - 根拠: agent-UI 一般パターン「意味あるステップ名」 [4]。
  - evorch 該当: 該当。`model_metadata.rs:17`、`provider_models.rs:168`、
    `provider_codex_editor.rs:129` が `ui.spinner()` 単体。
    phase_indicator.rs:24 と transcript_tool.rs:55 は badge 併用の
    ため非該当寄り。
- パターン名: 無条件 auto-scroll
  - 内容: ユーザーがスクロール位置を離れても強制的に最下部へ戻す [4]。
  - 根拠: agent-UI anti-pattern [4]。
  - evorch 該当: 該当候補。`agent.rs:193` が `stick_to_bottom(true)`
    を無条件指定している。最下部付近判定の条件付き化を検討する。
- パターン名: token 毎 state 更新
  - 内容: ストリーミング token 毎に state 更新し、再描画コストを
    爆発させる [4]。
  - 根拠: agent-UI anti-pattern [4]。
  - evorch 該当: 現状非該当 (本調査の観測範囲では未確認)。transcript
    更新経路のバッファリング設計を、実装改善時に併せて確認する。

## 6. Tokyo Night テーマ仕様

### 6.1 folke Night 変体の全色

一次ソース: folke/tokyonight.nvim @cdc07ac、
extras/lua/tokyonight_night.lua (SHA-pinned) [1][2]。

| ロール | hex | 用途目安 |
|--------|-----|----------|
| bg | #1a1b26 | メイン背景 |
| bg_dark | #16161e | 最深度背景 (sidebar 等) |
| bg_highlight | #292e42 | ハイライト背景・border 相当 |
| bg_visual | #283457 | 選択背景 |
| fg | #c0caf5 | 本文 |
| fg_dark | #a9b1d6 | 二次テキスト |
| comment | #565f89 | コメント (補助用途のみ) |
| blue | #7aa2f7 | 主 accent |
| cyan | #7dcfff | 実行中・情報補助 |
| green | #9ece6a | 成功 |
| magenta | #bb9af7 | 強調補助 |
| red | #f7768e | エラー |
| orange | #ff9e64 | 警告・accent 補助 |
| yellow | #e0af68 | 注意 |
| teal | #1abc9c | git.add 系 |
| purple | #9d7cd8 | secondary |
| git.add | #449dab | diff 追加 |
| git.change | #6183bb | diff 変更 |
| git.delete | #914c54 | diff 削除 |

コントラスト計算値 (Night bg 基準): fg/bg 10.59:1 (WCAG AAA)、
blue/bg 6.79:1、comment/bg 2.76:1 (補助用途のみ) [1]。

### 6.2 OpenCode tokyonight.json のロール割当規律

取得源: anomalyco/opencode @228e9095 (dev)、
packages/tui/src/theme/assets/tokyonight.json [3]。dark は Tokyo Night
Moon 系の色使いで、Step1 #1a1b26 (Night bg と同値)、Step2 #1e2030、
Step3 #222436、Step9 blue #82aaff、purple #c099ff、orange #ff966c、
red #ff757f、green #c3e88d、cyan #86e1fc、yellow #ffc777、text Step12
#c8d3f5、textMuted Step11 #828bb8 を使用する [3]。

割当規律: primary/info = Step9 blue、secondary = purple、
accent/warning = orange、error = red、success = green、
background/Panel/Element = Step1-3、border 系 = Step6-8。diff/markdown/
syntax は全ロール明示で、省略依存しない [3]。

### 6.3 evorch トークンへの推奨マッピング

方針: Night 変体の色を基調とし、surface 階層が不足する部分は
OpenCode tokyonight.json の Step2/Step3 を採用する。全トークンを明示
し、default 依存を残さない (第 4.7 節)。

| evorch トークン | 現行値 | 推奨値 | 出典 |
|-----------------|--------|--------|------|
| CANVAS | #0a0a0a | #1a1b26 (bg) | [1] |
| SURFACE | #111111 | #1e2030 (Step2) | [3] |
| SURFACE_RAISED | #141414 | #222436 (Step3) | [3] |
| OVERLAY | #191919 | #222436 (Step3) | [3] |
| SIDEBAR | #000000 | #16161e (bg_dark) | [1] |
| TEXT | #f5f5f5 | #c0caf5 (fg) | [1] |
| TEXT_MUTED | #818181 | #737aa2 (dark5 系) | 下記注記 |
| BORDER | #191919 | #292e42 (bg_highlight) | [1] |
| INPUT | #1e1e1e | #292e42 (bg_highlight) | [1] |
| ACCENT | #346bf1 | #7aa2f7 (blue) | [1] |
| HOVER_ROW | #131313 | #222436 (Step3) | [3] |
| ACTIVE_ROW | #1a1b1b | #283457 (bg_visual) | [1] |
| ERROR / ERROR_FG | #fb414a / #ff6467 | #f7768e (red) | [1] |
| ERROR_SURFACE | #301214 | #914c54 (git.delete) | [1] |
| WARNING / WARNING_FG | #fe9a00 / #ffb900 | #ff9e64 (orange) | [1][3] |
| SUCCESS | #34d399 | #9ece6a (green) | [1] |
| INFO | #60a5fa | #7aa2f7 (blue) | [1][3] |
| RUNNING | #22d3ee | #7dcfff (cyan) | [1] |
| WAITING | INFO と同値 | #e0af68 (yellow) | 下記注記 |

注記 1 (TEXT_MUTED): comment #565f89 は bg に対し 2.76:1 で本文用途の
コントラストを満たさない。TEXT_MUTED には dark5 系の #737aa2 を使い、
comment #565f89 は装飾・プレースホルダ等の補助用途に限定する。

注記 2 (WAITING): 現行は INFO エイリアスで識別不能 (tokens.rs:28)。
Tokyo Night 導入を機に yellow #e0af68 への分離を推奨する。
`phase_indicator_tokens_are_distinct` テスト (tokens.rs:125-135) の
`assert_eq!(WAITING, INFO)` を同時に更新する必要がある。

注記 3 (INFO = ACCENT 同色): OpenCode の primary/info = blue 規律に
倣い INFO を blue とする [3]。ACCENT と同色になるが、OpenCode と同じ
割当であり意図的な統一である。

diff 専用トークン (新設): DIFF_ADD = #449dab、DIFF_CHANGE = #6183bb、
DIFF_DELETE = #914c54 (git 色) [1]。

## 7. 改善ロードマップ

調査結果から導く、実装者向けの優先順位リスト。上から順に依存関係が
ある。

1. ThemePreset の全パレット構造体化。現行の 2 分岐 (style.rs:31-34)
   を廃止し、preset 毎に全トークンを持つ構造体へ移行する。根拠は
   OpenCode の全ロール明示規律 (第 4.1 節) [3]。
2. TokyoNight preset の追加。第 6.3 節のマッピング表をそのまま
   実装する。`phase_indicator_tokens_are_distinct` テスト
   (tokens.rs:125-135) の WAITING/INFO 同値 assert を更新する。
3. テーマ切替 UI。`viewer.rs:18-31` の `Workbench settings` メニュー
   への追加が最小変更。モーダル化する場合は provider_settings の
   open/close API + Modal::new + surface_frame パターンを踏襲する。
   `reload_theme` (state.rs:269-273) をここへ接続する。
4. 永続化。`workspace-ui::UiSettings` に `theme_preset` を追加する
   (現状は state.rs:71、state.rs:142 で Graphite 固定)。evorch.toml
   / crates/config に UI テーマ schema は現存しない。
5. spinner 単体表示の改善。`model_metadata.rs:17`、
   `provider_models.rs:168`、`provider_codex_editor.rs:129` の 3 箇所
   にフェーズ名を併記する (第 5 章)。
6. WAITING/INFO の色分離。第 6.3 節の注記 2 に従う。
7. 条件付き auto-scroll。`agent.rs:193` の `stick_to_bottom(true)`
   に最下部付近判定を加える。
8. キーボード導線。テーマ切替 KeyAction の追加。変更面は settings.rs /
   keymap.rs / app/frame.rs dispatch / app/state.rs / theme/style.rs /
   viewer.rs / bin/evorch-gui.rs / 各テストの計 12 ファイル。
9. 状態 glyph 規約の統一。status dot / phase pill / tab bullet を
   1 規約へ整理する (第 4.4 節)。
10. QA 接続。`cargo run -p gui --bin headless_capture -- --demo --out
    x.png` (WGPU_BACKEND=vulkan、--size/--dpi/--open-settings あり) と
    `scripts/gui-evidence-matrix.sh` (24 PNG 一括)、
    `cargo test -p gui --test size_dpi_matrix` (4 状態 x 4 サイズ x
    3 DPI) を TokyoNight preset に拡張し、新旧テーマの証跡を比較
    可能にする。EVORCH_REQUIRE_ADAPTER=1 で adapter 必須化。
11. 将来案: diff 専用トークン (DIFF_ADD/CHANGE/DELETE) の新設と
    responsive diff、会話内 approval カード化、command palette。

## 8. 出典一覧

1. folke/tokyonight.nvim (Night 変体一次ソース、@cdc07ac):
   https://github.com/folke/tokyonight.nvim
2. tokyonight Night extras (SHA-pinned 色定義):
   https://github.com/folke/tokyonight.nvim/blob/main/extras/lua/tokyonight_night.lua
3. anomalyco/opencode @228e9095 (dev) tokyonight.json:
   https://github.com/anomalyco/opencode/blob/228e9095/packages/tui/src/theme/assets/tokyonight.json
4. OpenCode 公式ドキュメント:
   https://opencode.ai
5. pingdotgg/t3code (themePalette.ts / index.css、@9375c77):
   https://github.com/pingdotgg/t3code
6. stablyai/orca:
   https://github.com/stablyai/orca
7. Orca Style Guide (docs/STYLEGUIDE.md、@93c3702):
   https://github.com/stablyai/orca/blob/93c3702/docs/STYLEGUIDE.md
8. Orca issue #4395 (polish 要求項目):
   https://github.com/stablyai/orca/issues/4395
9. Orca 公式サイト:
   https://onorca.dev
10. catppuccin/egui (Visuals 全面 mapping 参照実装、@ffb92d2):
    https://github.com/catppuccin/egui
11. egui 本体:
    https://github.com/emilk/egui
12. egui_dock (Style 構造):
    https://github.com/Adanos020/egui_dock
13. egui-styled:
    https://crates.io/crates/egui-styled
14. egui-stylesheet:
    https://crates.io/crates/egui-stylesheet
15. egui-thematic (Tokyo Night preset 情報):
    https://crates.io/crates/egui-thematic
16. Zed (フォント使い分け・トークンカテゴリ化):
    https://zed.dev
17. Charm Lip Gloss (宣言的 style、graceful degradation):
    https://github.com/charmbracelet/lipgloss
