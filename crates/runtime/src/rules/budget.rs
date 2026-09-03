//! プロジェクトルール注入のバイト予算計算。

use providers::Usage;

use super::types::RulesSettings;

const BYTES_PER_TOKEN: u64 = 4;

pub(crate) fn injection_budget_bytes(
    settings: &RulesSettings,
    last_usage: Option<&Usage>,
    estimated_new_bytes: u64,
) -> u64 {
    let used_tokens = last_usage.map_or(0, |usage| {
        usage.input_tokens.saturating_add(usage.output_tokens)
    });
    let estimated_new_tokens = estimated_new_bytes / BYTES_PER_TOKEN;
    let available_tokens = settings
        .context_window_tokens
        .saturating_sub(used_tokens)
        .saturating_sub(settings.response_headroom_tokens)
        .saturating_sub(estimated_new_tokens);
    available_tokens
        .saturating_mul(BYTES_PER_TOKEN)
        .min(settings.max_injection_bytes)
}

#[cfg(test)]
mod tests {
    use providers::Usage;

    use crate::rules::types::RulesSettings;

    use super::injection_budget_bytes;

    fn settings() -> RulesSettings {
        RulesSettings {
            context_window_tokens: 100,
            response_headroom_tokens: 10,
            max_injection_bytes: 1_000,
        }
    }

    // Given: 使用量なし / When: 予算計算 / Then: headroom と新規履歴推定を差し引いた値になる
    #[test]
    fn no_usage_uses_full_available_window() {
        assert_eq!(injection_budget_bytes(&settings(), None, 40), 320);
    }

    // Given: ウィンドウ近傍まで使用済み / When: 予算計算 / Then: 0 に飽和する
    #[test]
    fn usage_near_window_saturates_to_zero() {
        let usage = Usage {
            input_tokens: 90,
            output_tokens: 5,
            ..Usage::default()
        };

        assert_eq!(injection_budget_bytes(&settings(), Some(&usage), 0), 0);
    }

    // Given: 十分な空きと小さい最大注入量 / When: 予算計算 / Then: 最大値に clamp される
    #[test]
    fn budget_is_clamped_to_configured_maximum() {
        let settings = RulesSettings {
            max_injection_bytes: 64,
            ..settings()
        };

        assert_eq!(injection_budget_bytes(&settings, None, 0), 64);
    }

    // Given: 応答 headroom / When: 予算計算 / Then: トークン単位で差し引かれる
    #[test]
    fn response_headroom_is_subtracted() {
        let settings = RulesSettings {
            context_window_tokens: 20,
            response_headroom_tokens: 5,
            max_injection_bytes: 100,
        };

        assert_eq!(injection_budget_bytes(&settings, None, 0), 60);
    }
}
