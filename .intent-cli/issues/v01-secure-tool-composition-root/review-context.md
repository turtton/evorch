# v01-secure-tool-composition-root Review Context

Review that this slice moves operation toward the documented intent without widening scope.

Flag findings if the implementation:

- widens scope beyond the issue contract;
- launches AI providers from `intent-cli`;
- mutates GitHub or parent state when the issue is read-only;
- skips required contract sections.

## Slice-specific review focus

- downstream caller が `DirectSandbox` を unit struct / `Default` / public constructor で policy 明示なしに作れなくなっているか。名前だけ変えた public escape hatch は不十分
- production composition root が `BwrapSandbox::detect` failure を typed error として返し、DirectSandbox や raw process execution に fallback しないか
- standard shell / git_diff の production wiring が安全な composition root を通るか。低レベル `Arc<dyn Sandbox>` injection が production の推奨 path として残っていないか
- orchestrator_demo が新 composition root を実際に使用し、DirectSandbox import/construct が消えているか
- compile-fail または同等の API contract test が「permissive construction 不能」を downstream 視点で証明しているか
- RecordingSandbox / DirectSandbox を使う tests が明示 opt-out path に限定され、test convenience のため production visibility を再び広げていないか
- sandbox `wrap` semantics、bwrap argv、approval flow、tool result behavior を不要に変更していないか
- gVisor / Landlock / macOS / Windows や新 policy engine を追加していないか。これらは明確な scope widening

## Facet context

<!-- BEGIN GENERATED FACET CONTEXT (G530) -->
### vocabulary
- (none overlapping this packet's intent_references)
### invariant
- (none overlapping this packet's intent_references)
### decider
- (none overlapping this packet's intent_references)
### acceptance-property
- (none overlapping this packet's intent_references)
<!-- END GENERATED FACET CONTEXT (G530) -->

注: `intent-cli intent facet-check` は lexical な補助に留まる。上記 Slice-specific review focus が fail-closed construction invariant の意味検証を担う。

## Knowledge Writeback Expectation (G461)

`closeout_learning.write_back_required` は `true`。closeout で `intents/evorch/features/tools-sandbox/overview.md` に以下が記録されているか確認する（この PR 内または追跡可能な follow-up packet）。

- production composition root の最終 API と必須入力（workspace root / policy）
- DirectSandbox を利用できる限定 opt-out API と、その production 非推奨/非既定の境界
- bwrap unavailable / detect failure 時の typed error surface と no-fallback invariant

記録が未実施の場合は、将来の呼び出し側が再び fail-open wiring を導入し得るため、知識 writeback 不足として review 所見に残す。
