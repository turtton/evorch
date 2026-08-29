# ADR 0019: runtime reload セマンティクス — 2層分離・validate-then-swap・明示 /reload 起点

## Status

Accepted（2026-08-29、herdr-opencode-loop skill 作成後の運用知見から発生した設計問いを grill 形式の議論で確定）

## Context

skill 追加・設定変更を harness 起動後に反映する方法について、「pi には自動 reload があるのでは」との問いが発生した。pi-mono ソース調査（earendil-works/pi、2026-08-29 時点）の結果:

- **pi に自動 file watcher は存在しない**。あるのは明示 `/reload`（slash command）と拡張向け `ctx.reload()` のみ
- かつて `--watch`（100–200ms debounce・全 resource path 監視）は検討されたが [Issue #645](https://github.com/badlogic/pi-mono/issues/645) で**明示的に却下**。理由: 「編集中の一時的な不完全状態を自動 reload すると構文エラーや不安定な runtime になる。reload のタイミングはユーザーが判断すべき」
- pi の reload は**全体再評価方式**（差分イベント適用ではない）: 全リソース再探索 + キャッシュ置換 + system prompt 再構築
- 設定 parse 失敗時は**直前の正常状態を維持**しエラーを記録。skill 単位のバリデーション失敗はその skill のみ skip して残りを継続
- lifecycle 通知: `session_shutdown{reason:"reload"}` → `session_start{reason:"reload"}` → `resources_discover{reason:"reload"}`
- OpenCode も watcher なし（config 変更は再起動運用。hot reload は Issue/PR 提案段階）

一方で evorch 側には既に土台がある: ADR 0014（マルチソース loader・version migration・schemars validation）、ADR 0010（typed event bus + DiagnosticBus・「失敗は静かにしない」・config も domain transform 対象）、ADR 0003（Stable Prefix の毎 turn 再生成しない invariant）、ADR 0008（threat model）、ADR 0012（計測）。

## Decision

### 1. 2層分離 — config snapshot と prompt 可視コンテンツを分ける

| 層 | 対象 | reload の影響 |
|---|---|---|
| 層1: runtime 状態 | マージ済み config snapshot（routing 閾値・panel layout・metrics 保持期間等） | **プロンプト・コンテキスト・キャッシュに一切触れない**。in-memory の immutable snapshot を atomic swap するだけ |
| 層2: prompt 可視コンテンツ | skill snapshot・role 定義・tool schema | Stable Prefix 変化 → prefix マッチのキャッシュはその時点で1回だけ断絶（回避不能） |

「system prompt を最新メッセージに添付する」方式は採用しない。先頭固定（ADR 0003）の方が会話が長くなるほど prefix キャッシュが効くのに対し、末尾添付は添付点以降が毎ターン新規 token 化され steady-state ヒット率が毎ターン悪化する。

### 2. reload engine は transactional（validate-then-swap）

- ADR 0014 のマージ管線を再実行: multi-source 読込 → `config.d/` deep merge → version migration → schemars validation
- 全段成功時のみ新しい immutable snapshot を atomic swap で適用
- **失敗時は last-good snapshot を維持**し、DiagnosticBus にエラーを通知（ADR 0010-5「失敗は静かにしない」と一致）
- skill 単位のバリデーション失敗はその skill のみ skip（diagnostic 記録）して残りはロード継続（pi 準拠）

### 3. prompt 可視 diff の適用ルール（層2）

- 適用タイミングは**次ターン境界**（生成中の差し替えはしない）
- Stable Prefix は先頭固定のまま維持し、**差分は末尾への小さな append**（system notice / event。「skills updated: X 追加・Y 変更」程度）として通知。新規 token は差分通知分だけ
- Stable Prefix 本体が変わった時だけ1回だけキャッシュ断絶。その際の cache miss 率変化は ADR 0012 の計測で可観測にする（実装時に実測して微調整する）

### 4. lifecycle event（typed event bus 上に定義）

- `config.reload_started` / `config.reload_applied` / `config.reload_failed`
- `config.reload_applied` には `prompt_impact: bool` と `diff: [...]` を載せ、層1のみの変更（大半の config 編集）が断絶ゼロであることを購読者（GUI pane・agent role）が判別可能にする
- `skills.updated` も同様に diff 付き

### 5. reload の起点 — v0.1 は明示 `/reload` のみ

- **watch と reload engine を分離**する。engine は明示コマンド / RPC / GUI 操作から常に利用可能（pi 準拠）
- **v0.1 スコープは明示 `/reload` のみ**（pi と同水準。v01-routing-profiles の config 層に載せる）
- **auto-watch は opt-in で v0.1 以降**。設計上の分岐点:
  - user 配下 `~/.config/evorch/config.d/` の watch は許容候補（validate-then-swap があれば中間状態でも last-good 維持されるため pi の却下理由は緩和される）
  - **project 配下 `./evorch.toml` の自動適用はしない**（ADR 0008 threat model との整合）。checkout した repo が無確認で harness 挙動を変えられる攻撃面になるため、明示 `/reload` または GUI 確認を必須とする

## Consequences

- v01-routing-profiles packet の acceptance criteria に「reload engine + 明示 `/reload` + 失敗時 last-good 維持」が乗る（packet 修正はその slice 着手時に実施）
- auto-watch の採用判断は v0.1 実装後の運用実績（明示 reload の頻度・ユーザー体験）を見てから別途判断
- reload 由来の cache miss 率変化は ADR 0012 metrics（tok/s・cache 関連）で継続観測する
