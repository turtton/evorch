//! compaction 発火ポリシー（T7 で実装）。

use std::collections::BTreeMap;

use config::{CompactionConfig, SummarizerKind};

use crate::prompt::ModelFamily;

const DEFAULT_CONTEXT_WINDOW_TOKENS: u64 = 200_000;
const DEFAULT_THRESHOLD: f64 = 0.75;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SummarizerKindSel {
    Model,
    Structural,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CompactionSettings {
    pub enabled: bool,
    pub threshold: f64,
    pub context_window_tokens: u64,
    pub model_overrides: BTreeMap<String, u64>,
    pub keep_recent_tokens: u64,
    pub cooldown_turns: u32,
    pub max_summary_bytes: u64,
    pub summarizer: SummarizerKindSel,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold: DEFAULT_THRESHOLD,
            context_window_tokens: DEFAULT_CONTEXT_WINDOW_TOKENS,
            model_overrides: BTreeMap::new(),
            keep_recent_tokens: 20_000,
            cooldown_turns: 1,
            max_summary_bytes: 16_384,
            summarizer: SummarizerKindSel::Model,
        }
    }
}

impl From<&CompactionConfig> for CompactionSettings {
    fn from(config: &CompactionConfig) -> Self {
        let threshold = if config.threshold.is_nan() {
            DEFAULT_THRESHOLD
        } else {
            config.threshold.clamp(f64::MIN_POSITIVE, 1.0)
        };
        let context_window_tokens = match config.context_window_tokens {
            0 => DEFAULT_CONTEXT_WINDOW_TOKENS,
            window => window,
        };
        let summarizer = match config.summarizer {
            SummarizerKind::Model => SummarizerKindSel::Model,
            SummarizerKind::Structural => SummarizerKindSel::Structural,
        };

        let model_overrides = config
            .model_overrides
            .iter()
            // 0 の override は比率計算の 0 除算を招くため、baseline へ正規化する。
            .map(|(model, window)| {
                let window = match window {
                    0 => context_window_tokens,
                    window => *window,
                };
                (model.clone(), window)
            })
            .collect();

        Self {
            enabled: config.enabled,
            threshold,
            context_window_tokens,
            model_overrides,
            keep_recent_tokens: config.keep_recent_tokens,
            cooldown_turns: config.cooldown_turns,
            max_summary_bytes: config.max_summary_bytes,
            summarizer,
        }
    }
}

pub(crate) fn resolve_window(settings: &CompactionSettings, model_id: &str) -> u64 {
    settings
        .model_overrides
        .get(model_id)
        .copied()
        .unwrap_or(settings.context_window_tokens)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TriggerDecision {
    Trigger,
    BelowThreshold,
    Disabled,
    Cooldown,
    AlreadyThisBoundary,
    InFlight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GuardDecision {
    Disabled,
    InFlight,
    AlreadyThisBoundary,
    Cooldown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThresholdDecision {
    Trigger,
    BelowThreshold,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct CompactionLoopState {
    pub last_handled_gen: u64,
    pub turn_counter: u64,
    pub last_compaction_turn: Option<u64>,
    pub compacted_this_boundary: bool,
    pub checkpoint_seq: u32,
    pub in_flight: bool,
    pub last_estimated_tokens: u64,
}

/// 全理由共通の抑制条件を Disabled、InFlight、境界、Cooldown の順に判定する。
pub(crate) fn guard_decision(
    state: &CompactionLoopState,
    settings: &CompactionSettings,
) -> Option<GuardDecision> {
    if !settings.enabled {
        return Some(GuardDecision::Disabled);
    }
    if state.in_flight {
        return Some(GuardDecision::InFlight);
    }
    if state.compacted_this_boundary {
        return Some(GuardDecision::AlreadyThisBoundary);
    }
    if state.last_compaction_turn.is_some_and(|last_turn| {
        state.turn_counter.saturating_sub(last_turn) < u64::from(settings.cooldown_turns)
    }) {
        return Some(GuardDecision::Cooldown);
    }
    None
}

/// 共通抑制条件の後に自動発火の使用率を判定する。
pub(crate) fn should_trigger(
    state: &CompactionLoopState,
    settings: &CompactionSettings,
    estimated_tokens: u64,
    window_tokens: u64,
) -> TriggerDecision {
    if let Some(decision) = guard_decision(state, settings) {
        return match decision {
            GuardDecision::Disabled => TriggerDecision::Disabled,
            GuardDecision::InFlight => TriggerDecision::InFlight,
            GuardDecision::AlreadyThisBoundary => TriggerDecision::AlreadyThisBoundary,
            GuardDecision::Cooldown => TriggerDecision::Cooldown,
        };
    }

    match threshold_decision(settings, estimated_tokens, window_tokens) {
        ThresholdDecision::Trigger => TriggerDecision::Trigger,
        ThresholdDecision::BelowThreshold => TriggerDecision::BelowThreshold,
    }
}

pub(crate) fn threshold_decision(
    settings: &CompactionSettings,
    estimated_tokens: u64,
    window_tokens: u64,
) -> ThresholdDecision {
    let ratio = estimated_tokens as f64 / window_tokens as f64;
    if ratio >= settings.threshold {
        ThresholdDecision::Trigger
    } else {
        ThresholdDecision::BelowThreshold
    }
}

pub(crate) fn compaction_policy_text(settings: &CompactionSettings, family: ModelFamily) -> String {
    let cache_guidance = match family {
        ModelFamily::Claude => {
            "Preserve Claude prompt-cache prefix stability; do not break the stable prompt prefix unnecessarily."
        }
        ModelFamily::OpenAiReasoning
        | ModelFamily::Gpt5
        | ModelFamily::Gemini
        | ModelFamily::Kimi
        | ModelFamily::Unknown => {
            "Preserve provider cache reuse; do not break the stable prompt prefix unnecessarily."
        }
    };
    let threshold_percent = settings.threshold * 100.0;

    format!(
        "Compaction becomes eligible when context usage reaches {threshold_percent}% of the context window.\n\
{cache_guidance}\n\
call the `compact` tool at meaningful task boundaries when compaction is eligible.\n\
avoid consecutive compaction; cooldown {} turn(s) applies.",
        settings.cooldown_turns
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::ModelFamily;
    use config::{CompactionConfig, SummarizerKind};
    use std::collections::BTreeMap;

    fn state() -> CompactionLoopState {
        CompactionLoopState {
            last_handled_gen: 0,
            turn_counter: 0,
            last_compaction_turn: None,
            compacted_this_boundary: false,
            checkpoint_seq: 0,
            in_flight: false,
            last_estimated_tokens: 0,
        }
    }

    // Given: 75% 閾値と 1000 token window
    // When: 閾値直前・一致・直後の推定値を判定する
    // Then: 749 は未達、750 と 751 は発火する
    #[test]
    fn trigger_boundary_at_seventy_five_percent_is_inclusive() {
        let settings = CompactionSettings::default();
        let state = state();
        let cases = [
            (749, TriggerDecision::BelowThreshold),
            (750, TriggerDecision::Trigger),
            (751, TriggerDecision::Trigger),
        ];
        for (estimate, expected) in cases {
            assert_eq!(should_trigger(&state, &settings, estimate, 1_000), expected);
        }
    }

    // Given: 75% 閾値と既定の 200000 token window
    // When: 150000 token を判定する
    // Then: 比率一致を発火として扱う
    #[test]
    fn trigger_boundary_equality_scales_to_default_window() {
        assert_eq!(
            should_trigger(&state(), &CompactionSettings::default(), 150_000, 200_000),
            TriggerDecision::Trigger
        );
    }

    // Given: cooldown=1 で同じターンに圧縮済み
    // When: 同一ターンと次ターンを判定する
    // Then: 同一ターンだけ抑制し、次ターンは再び発火可能になる
    #[test]
    fn cooldown_blocks_current_turn_and_expires_on_next_turn() {
        let settings = CompactionSettings::default();
        let mut state = state();
        state.turn_counter = 8;
        state.last_compaction_turn = Some(8);
        assert_eq!(
            should_trigger(&state, &settings, 750, 1_000),
            TriggerDecision::Cooldown
        );
        state.turn_counter = 9;
        assert_eq!(
            should_trigger(&state, &settings, 750, 1_000),
            TriggerDecision::Trigger
        );
    }

    // Given: 個別の抑制条件が有効
    // When: 閾値超過を判定する
    // Then: 各条件に対応する決定を返す
    #[test]
    fn trigger_guards_return_their_specific_decisions() {
        let settings = CompactionSettings::default();
        let mut boundary_state = state();
        boundary_state.compacted_this_boundary = true;
        assert_eq!(
            should_trigger(&boundary_state, &settings, 750, 1_000),
            TriggerDecision::AlreadyThisBoundary
        );
        let mut in_flight_state = state();
        in_flight_state.in_flight = true;
        assert_eq!(
            should_trigger(&in_flight_state, &settings, 750, 1_000),
            TriggerDecision::InFlight
        );
        let disabled_settings = CompactionSettings {
            enabled: false,
            ..CompactionSettings::default()
        };
        assert_eq!(
            should_trigger(&state(), &disabled_settings, 750, 1_000),
            TriggerDecision::Disabled
        );
    }

    // Given: 全抑制条件が同時に成立
    // When: 発火可否を判定する
    // Then: Disabled, InFlight, AlreadyThisBoundary, Cooldown の順で先の条件が勝つ
    #[test]
    fn check_order_prefers_disabled_then_in_flight_then_boundary_then_cooldown() {
        let mut settings = CompactionSettings {
            enabled: false,
            ..CompactionSettings::default()
        };
        let mut state = state();
        state.in_flight = true;
        state.compacted_this_boundary = true;
        state.last_compaction_turn = Some(0);
        assert_eq!(
            should_trigger(&state, &settings, 750, 1_000),
            TriggerDecision::Disabled
        );
        settings.enabled = true;
        assert_eq!(
            should_trigger(&state, &settings, 750, 1_000),
            TriggerDecision::InFlight
        );
        state.in_flight = false;
        assert_eq!(
            should_trigger(&state, &settings, 750, 1_000),
            TriggerDecision::AlreadyThisBoundary
        );
        state.compacted_this_boundary = false;
        assert_eq!(
            should_trigger(&state, &settings, 750, 1_000),
            TriggerDecision::Cooldown
        );
    }

    // Given: baseline と完全一致するモデル override
    // When: 既知・未知モデルの window を解決する
    // Then: 既知モデルは override、未知モデルは baseline を使う
    #[test]
    fn resolve_window_prefers_exact_override_and_falls_back_to_baseline() {
        let settings = CompactionSettings {
            context_window_tokens: 128_000,
            model_overrides: BTreeMap::from([(String::from("claude-sonnet-4-5"), 180_000)]),
            ..CompactionSettings::default()
        };
        assert_eq!(resolve_window(&settings, "claude-sonnet-4-5"), 180_000);
        assert_eq!(resolve_window(&settings, "unknown-model"), 128_000);
    }

    // Given: window=0 の override を含む config
    // When: runtime settings へ変換する
    // Then: 0 の override は baseline window へ正規化され、非 0 は保持される
    #[test]
    fn settings_from_config_normalizes_zero_model_overrides_to_baseline() {
        let config = CompactionConfig {
            context_window_tokens: 128_000,
            model_overrides: BTreeMap::from([
                (String::from("model-zero"), 0),
                (String::from("model-kept"), 99_000),
            ]),
            ..CompactionConfig::default()
        };
        let settings = CompactionSettings::from(&config);
        assert_eq!(resolve_window(&settings, "model-zero"), 128_000);
        assert_eq!(resolve_window(&settings, "model-kept"), 99_000);
    }

    // Given: 全項目を既定値と異なる値にした config
    // When: runtime settings に変換する
    // Then: 全項目と summarizer 変種が保持される
    #[test]
    fn settings_from_config_maps_all_fields() {
        let config = CompactionConfig {
            enabled: false,
            threshold: 0.8,
            context_window_tokens: 128_000,
            model_overrides: BTreeMap::from([(String::from("model-a"), 99_000)]),
            keep_recent_tokens: 12_000,
            cooldown_turns: 3,
            max_summary_bytes: 8_192,
            summarizer: SummarizerKind::Structural,
        };
        let settings = CompactionSettings::from(&config);
        assert_eq!(
            settings,
            CompactionSettings {
                enabled: false,
                threshold: 0.8,
                context_window_tokens: 128_000,
                model_overrides: BTreeMap::from([(String::from("model-a"), 99_000)]),
                keep_recent_tokens: 12_000,
                cooldown_turns: 3,
                max_summary_bytes: 8_192,
                summarizer: SummarizerKindSel::Structural,
            }
        );
    }

    // Given: 範囲外 threshold と zero window
    // When: runtime settings に変換する
    // Then: threshold を有効範囲へ clamp し、window は既定値へ戻す
    #[test]
    fn settings_from_config_clamps_threshold_and_falls_back_from_zero_window() {
        let zero = CompactionConfig {
            threshold: 0.0,
            context_window_tokens: 0,
            ..CompactionConfig::default()
        };
        let high = CompactionConfig {
            threshold: 1.5,
            ..CompactionConfig::default()
        };
        let zero_settings = CompactionSettings::from(&zero);
        let high_settings = CompactionSettings::from(&high);
        assert!(zero_settings.threshold > 0.0);
        assert_eq!(zero_settings.context_window_tokens, 200_000);
        assert_eq!(high_settings.threshold, 1.0);
    }

    // Given: 75% threshold と全 ModelFamily 変種
    // When: compaction policy 文を生成する
    // Then: 必須4条項が全ファミリに含まれ、Claude は prefix cache を明示する
    #[test]
    fn policy_text_contains_required_clauses_for_every_model_family() {
        let settings = CompactionSettings::default();
        let families = [
            ModelFamily::Claude,
            ModelFamily::OpenAiReasoning,
            ModelFamily::Gpt5,
            ModelFamily::Gemini,
            ModelFamily::Kimi,
            ModelFamily::Unknown,
        ];
        for family in families {
            let text = compaction_policy_text(&settings, family);
            assert!(text.contains("75"), "family={family:?}");
            for clause in [
                "do not break the stable prompt prefix unnecessarily",
                "call the `compact` tool at meaningful task boundaries",
                "avoid consecutive compaction; cooldown 1 turn(s)",
            ] {
                assert!(text.contains(clause), "family={family:?}, clause={clause}");
            }
            assert!(text.lines().count() <= 12, "family={family:?}");
        }
        let claude = compaction_policy_text(&settings, ModelFamily::Claude);
        let generic = compaction_policy_text(&settings, ModelFamily::Gpt5);
        assert!(claude.contains("Claude prompt-cache prefix stability"));
        assert!(generic.contains("provider cache reuse"));
        assert_ne!(claude, generic);
    }
}
