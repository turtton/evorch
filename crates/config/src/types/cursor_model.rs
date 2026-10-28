//! Cursor のモデル ID の分解と、Cursor 公式 docs 由来の静的なモデル表。
//!
//! Cursor は `claude-opus-5-5-high` や `gpt-5-high-fast` のように、推論強度・速度レーン・
//! thinking の別をモデル ID の接尾辞で表す。料金や context window を models.dev で引くには
//! 接尾辞を除いた基底 ID とモデルファミリーが必要になるため、ここで一元的に分解する。

use super::provider::{ModelPricing, PriceSchedule, PriceSet, PriceTier};

/// 速度レーンを表す接尾辞。Cursor のモデル ID 自体が別レーンを指す。
pub const FAST_LANE_SUFFIX: &str = "-fast";
/// thinking 変種を表す接尾辞。
pub const THINKING_SUFFIX: &str = "-thinking";
/// 推論強度として接尾辞に現れるトークン。
pub const EFFORT_SUFFIXES: [&str; 8] = [
    "none", "off", "minimal", "low", "medium", "high", "xhigh", "max",
];

/// 末尾の `-fast` を一つだけ分離する。基底が空になる場合は通常 ID として扱う。
pub fn split_fast_lane(id: &str) -> (&str, bool) {
    match id.strip_suffix(FAST_LANE_SUFFIX) {
        Some(base) if !base.is_empty() => (base, true),
        _ => (id, false),
    }
}

/// 末尾の推論強度トークンを一つだけ分離する。基底が空になる場合は通常 ID として扱う。
pub fn split_effort_suffix(id: &str) -> (&str, Option<&str>) {
    match id.rsplit_once('-') {
        Some((base, effort)) if !base.is_empty() && EFFORT_SUFFIXES.contains(&effort) => {
            (base, Some(effort))
        }
        _ => (id, None),
    }
}

/// モデル ID を接尾辞ごとに分解した結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorModelParts<'a> {
    /// 接尾辞をすべて除いた基底 ID。
    pub base: &'a str,
    pub effort: Option<&'a str>,
    pub fast: bool,
    pub thinking: bool,
}

/// 末尾から `-fast` / `-thinking` / 推論強度を剥がして基底 ID を得る。
pub fn split_cursor_model_id(id: &str) -> CursorModelParts<'_> {
    let mut parts = CursorModelParts {
        base: id,
        effort: None,
        fast: false,
        thinking: false,
    };
    loop {
        let (rest, fast) = split_fast_lane(parts.base);
        if fast && !parts.fast {
            parts.base = rest;
            parts.fast = true;
            continue;
        }
        if !parts.thinking
            && let Some(rest) = parts
                .base
                .strip_suffix(THINKING_SUFFIX)
                .filter(|rest| !rest.is_empty())
        {
            parts.base = rest;
            parts.thinking = true;
            continue;
        }
        let (rest, effort) = split_effort_suffix(parts.base);
        if parts.effort.is_none() && effort.is_some() {
            parts.base = rest;
            parts.effort = effort;
            continue;
        }
        return parts;
    }
}

/// 料金表の検索に使う ID を、具体的なものから順に返す。過剰な剥がしで別モデルを
/// 引かないよう、元の ID と中間段階も候補に含める。
pub fn cursor_lookup_ids(id: &str) -> Vec<&str> {
    let mut ids = vec![id];
    let mut current = id;
    loop {
        let (rest, fast) = split_fast_lane(current);
        let stripped = if fast {
            rest
        } else if let Some(rest) = current
            .strip_suffix(THINKING_SUFFIX)
            .filter(|rest| !rest.is_empty())
        {
            rest
        } else {
            split_effort_suffix(current).0
        };
        if stripped == current {
            return ids;
        }
        ids.push(stripped);
        current = stripped;
    }
}

/// 別表記で載っている同じモデルの ID 候補。カタログ照合専用で、送信する ID には使わない。
///
/// 版番号の区切りは提供元で `5-5` と `5.5` に揺れ、Google は `-preview` を付けた ID で
/// 載せることが多い。元の ID は含めない。
pub fn cursor_id_aliases(id: &str) -> Vec<String> {
    let mut aliases = Vec::new();
    for (from, to) in [('-', '.'), ('.', '-')] {
        let swapped = swap_version_separator(id, from, to);
        if swapped != id {
            aliases.push(swapped);
        }
    }
    if id.starts_with("gemini-") && !id.ends_with("-preview") {
        let preview = format!("{id}-preview");
        aliases.extend(
            std::iter::once(preview.clone()).chain(
                aliases
                    .clone()
                    .into_iter()
                    .map(|alias| format!("{alias}-preview")),
            ),
        );
    }
    aliases
}

/// 短い数字（版番号）に挟まれた `from` だけを `to` に置き換える。日付の `5-20251101` のような
/// 長い数字列との境目は版番号ではないので触らない。
fn swap_version_separator(id: &str, from: char, to: char) -> String {
    let chars: Vec<char> = id.chars().collect();
    let digits =
        |range: &mut dyn Iterator<Item = &char>| range.take_while(|c| c.is_ascii_digit()).count();
    chars
        .iter()
        .enumerate()
        .map(|(index, &c)| {
            let before = digits(&mut chars[..index].iter().rev());
            let after = digits(&mut chars[index + 1..].iter());
            if c == from && (1..=2).contains(&before) && (1..=2).contains(&after) {
                to
            } else {
                c
            }
        })
        .collect()
}

/// 基底 ID のファミリーから、同じモデルを API 価格で引く models.dev の provider。
/// ファミリーを推定できなければ空。
pub fn cursor_api_providers(base: &str) -> &'static [&'static str] {
    let base = base.to_ascii_lowercase();
    let openai_reasoning = ["o1", "o3", "o4"]
        .iter()
        .any(|prefix| base == *prefix || base.starts_with(&format!("{prefix}-")));
    if base.starts_with("claude-") {
        &["anthropic"]
    } else if base.starts_with("gpt-") || openai_reasoning {
        &["openai"]
    } else if base.starts_with("gemini-") {
        &["google"]
    } else if base.starts_with("grok-") {
        &["xai"]
    } else {
        &[]
    }
}

/// Cursor 公式 docs の一行。
struct DocsRow {
    id: &'static str,
    context_window: u64,
    /// docs が料金を載せるモデルだけ。Cursor 自社モデルが該当する。
    prices: Option<DocsPrices>,
}

struct DocsPrices {
    standard: Tiered,
    fast: Option<Tiered>,
}

/// `[input, cache_read, output]` の組と、入力が閾値を超えた要求の料金。
struct Tiered {
    base: Rates,
    above: Option<(u64, Rates)>,
}

/// USD / 100 万 tokens。docs が `-` とする cache write は未設定。
type Rates = (f64, f64, f64);

const fn flat(input: f64, cache_read: f64, output: f64) -> Tiered {
    Tiered {
        base: (input, cache_read, output),
        above: None,
    }
}

/// 出典: <https://cursor.com/docs/models-and-pricing> の "Cursor models" /
/// "Model pricing" の表と、各モデルの <https://cursor.com/docs/models/> ページ
/// (2026-10 時点)。context window は各ページの "Context window"（既定値）。
/// 第三者モデルは docs が API 定価と明記しており、料金は models.dev に委ねるため
/// context window だけを持つ。
const DOCS: &[DocsRow] = &[
    DocsRow {
        id: "composer-2.5",
        context_window: 200_000,
        prices: Some(DocsPrices {
            standard: flat(0.5, 0.2, 2.5),
            fast: Some(flat(3.0, 0.5, 15.0)),
        }),
    },
    DocsRow {
        id: "grok-4.7",
        context_window: 256_000,
        // "Grok 4.7 500k" 行: 入力が 256k を超える要求。
        prices: Some(DocsPrices {
            standard: Tiered {
                base: (2.0, 0.5, 6.0),
                above: Some((256_000, (4.0, 1.0, 12.0))),
            },
            fast: Some(Tiered {
                base: (4.0, 1.0, 12.0),
                above: Some((256_000, (6.0, 1.5, 18.0))),
            }),
        }),
    },
    DocsRow {
        id: "grok-4.6",
        context_window: 256_000,
        prices: Some(DocsPrices {
            standard: flat(2.0, 0.5, 6.0),
            fast: Some(flat(4.0, 1.0, 12.0)),
        }),
    },
    DocsRow {
        id: "grok-4.5",
        context_window: 256_000,
        prices: Some(DocsPrices {
            standard: flat(2.0, 0.5, 6.0),
            fast: Some(flat(4.0, 1.0, 18.0)),
        }),
    },
    DocsRow {
        id: "claude-fable-5-1",
        context_window: 300_000,
        prices: None,
    },
    DocsRow {
        id: "claude-haiku-5-5",
        context_window: 300_000,
        prices: None,
    },
    DocsRow {
        id: "claude-opus-5-5",
        context_window: 300_000,
        prices: None,
    },
    DocsRow {
        id: "claude-sonnet-5-5",
        context_window: 200_000,
        prices: None,
    },
    DocsRow {
        id: "gemini-3.1-pro",
        context_window: 200_000,
        prices: None,
    },
    DocsRow {
        id: "gemini-3.8-flash",
        context_window: 200_000,
        prices: None,
    },
    DocsRow {
        id: "gemini-4-argon",
        context_window: 200_000,
        prices: None,
    },
    DocsRow {
        id: "gpt-5.6-luna",
        context_window: 272_000,
        prices: None,
    },
    DocsRow {
        id: "gpt-5.6-sol",
        context_window: 272_000,
        prices: None,
    },
    DocsRow {
        id: "gpt-5.6-terra",
        context_window: 272_000,
        prices: None,
    },
    DocsRow {
        id: "muse-wonder-1.0",
        context_window: 300_000,
        prices: None,
    },
];

fn docs_row(id: &str) -> Option<&'static DocsRow> {
    let base = split_cursor_model_id(id).base;
    DOCS.iter().find(|row| row.id == base).or_else(|| {
        let aliases = cursor_id_aliases(base);
        DOCS.iter()
            .find(|row| aliases.iter().any(|alias| alias == row.id))
    })
}

/// Cursor docs が示す context window。docs に無いモデルは `None`。
pub fn cursor_docs_context_window(id: &str) -> Option<u64> {
    docs_row(id).map(|row| row.context_window)
}

fn rates(rates: Rates) -> ModelPricing {
    ModelPricing {
        input: Some(rates.0),
        output: Some(rates.2),
        cache_read: Some(rates.1),
        cache_write: None,
    }
}

fn price_set(tiered: &Tiered) -> PriceSet {
    PriceSet {
        base: rates(tiered.base),
        tiers: tiered
            .above
            .iter()
            .map(|(above_input_tokens, above)| {
                let pricing = rates(*above);
                PriceTier {
                    above_input_tokens: *above_input_tokens,
                    input: pricing.input,
                    output: pricing.output,
                    cache_read: pricing.cache_read,
                    cache_write: pricing.cache_write,
                }
            })
            .collect(),
    }
}

/// Cursor docs が料金を載せるモデルの料金表。`-fast` の ID は docs が明示する Fast レーンの
/// 料金を標準料金として返す。Fast の料金が無いモデルや docs に無いモデルは `None`。
pub fn cursor_docs_pricing(id: &str) -> Option<PriceSchedule> {
    let prices = docs_row(id)?.prices.as_ref()?;
    let lane = if split_cursor_model_id(id).fast {
        prices.fast.as_ref()?
    } else {
        &prices.standard
    };
    Some(PriceSchedule {
        standard: price_set(lane),
        fast: None,
    })
}

#[cfg(test)]
mod tests;
