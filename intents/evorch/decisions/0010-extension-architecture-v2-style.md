# ADR 0010: 拡張アーキテクチャ — OpenCode V2 型（小さな typed core + default plugins + domain transforms）

## Status

Accepted（2026-08-29、grill による全体構想レビューから確定）

## Context

オペレータの実体験: pi 上の oh-my-openagent（omo）の subagent 機能は拡張・調整が事実上不可能で、「どうしようもなかった」。2026-08-29 に pi-mono / OpenCode V2 / omo 実コードを調査し構造原因を特定した。

**pi/omo 側の失敗の構造的原因:**
- pi core は subagent を持たず、「別 session を main TUI に bind する API」が存在しない（pi Issue #830）。拡張は replica UI を作るしかない
- TUI side panel / 永続 layout の公式 API がなく（pi Issue #3769）、omo も固定形式の toast / tmux pane のみ。spawn 方式・状態追跡・通知・renderer・layout の差し替えポイントが存在しない
- 基盤 API の UI/lifecycle 不足を omo が内部 singleton（7種の Map/store）で補完した結果、拡張境界が消失。変更可能なのは model/role/prompt/tool のみ

**OpenCode V2 の検証結果:**
- 「core の大部分を plugin 化」は正確には「小さな typed core + default plugins」。core は state / schema / events / hook contracts を持ち、provider / auth / agent / command / skill / model discovery は built-in plugin として boot。内部実装も外部 plugin と同じ registry / transform API を通る
- domain ごとの typed transform（agent/catalog/command/tool/integration/skill）、immutable input + mutable output の順序付き hook、TUI 用の独立した plugin boundary
- 失敗例も教訓として特定: V1→V2 の破壊的変更（Issue #39345）、silent plugin failure（#41234, #44920）、hook 型と runtime call site の drift（#28066）

## Decision

1. **core は小さな typed kernel**: state / schema / typed error / event bus / minimal operations を所有。session runner、event bus、storage は core のまま
2. **built-in も外部も同じ API**: provider profile / agent role 定義 / tool 登録 / UiRegion 登録は全て typed transform / registry 経由。builtin provider も特別扱いしない
3. **subagent / session ライフサイクルは core の typed event bus として公開**（AgentEvent、§7 どおり）。拡張の「頑張りどころ」にしない。pi/omo で起きた「extension 実装に押し出された」問題の構造的対策
4. **UI は独立した拡張境界**: Workspace Model の UiRegionRegistry（panel / widget / renderer / layout region の登録）として定義。構想書 §23 Panel enum + §24 Semantic UI API が枠組み
5. **失敗は静かにしない**: plugin load / transform 失敗は DiagnosticBus に流し、明示エラーで表示
6. **type と runtime を別々に進化させない**: hook trait の実装有無を test で検証（call site drift の防止）
7. **サードパーティ動的プラグイン（WASM 等の隔離方式）は v0.3 以降**。v0.1 から「内部は plugin 構成」で作り、動的 loading は後付け可能にする。隔離方式は別途 ADR

## Consequences

- architecture.md の crate 構成に plugin registry / transform 層を追加
- tools-sandbox feature に「plugin も sandbox 対象（動的 loading 時）」の将来項目として追記
- Orchestrator / role 定義は AgentRoleRegistry として外部登録可能な形に設計
