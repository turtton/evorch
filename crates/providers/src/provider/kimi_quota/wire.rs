use super::{KimiQuotaWindow, QuotaError};
use chrono::{DateTime, Utc};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WindowId {
    FiveHours,
    Week,
    MonthTotal,
    MonthCode,
    OtherLimit(usize),
    UnspecifiedUsage,
}

const NAMED_WINDOWS: [(WindowId, &str, &str); 4] = [
    (WindowId::FiveHours, "limit_5h", "5h"),
    (WindowId::Week, "limit_7d", "wk"),
    (WindowId::MonthTotal, "limit_month_total", "month"),
    (WindowId::MonthCode, "limit_month_code", "code month"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Named,
    LegacyLimit,
    LegacyUsage,
}

#[derive(Debug)]
enum Quality {
    Missing,
    // Keep the unclamped percentage so conflicting over-limit counts stay distinguishable.
    Valid(f64),
    Invalid(QuotaError),
}

#[derive(Debug)]
struct Candidate {
    id: WindowId,
    source: Source,
    label: String,
    quality: Quality,
    // Retain a valid reset even if the ratio is invalid, for unambiguous usage association.
    resets_at: Option<DateTime<Utc>>,
}

fn candidate(
    id: WindowId,
    source: Source,
    label: String,
    entry: Option<&Value>,
    read_usage: fn(&Value) -> Result<f64, QuotaError>,
) -> Candidate {
    let Some(entry) = entry.filter(|entry| !entry.is_null()) else {
        return Candidate {
            id,
            source,
            label,
            quality: Quality::Missing,
            resets_at: None,
        };
    };
    let reset = reset_time(entry);
    let usage = read_usage(entry).and_then(|used| {
        if !used.is_finite() || used < 0.0 {
            return Err(QuotaError::Protocol("invalid Kimi quota usage"));
        }
        reset.as_ref().map(|_| used).map_err(Clone::clone)
    });
    Candidate {
        id,
        source,
        label,
        quality: match usage {
            Ok(used) => Quality::Valid(used),
            Err(error) => Quality::Invalid(error),
        },
        resets_at: reset.ok().flatten(),
    }
}

fn extract_named(raw: &Value) -> Vec<Candidate> {
    NAMED_WINDOWS
        .into_iter()
        .map(|(id, key, label)| {
            candidate(
                id,
                Source::Named,
                label.into(),
                raw.get("usages").and_then(|usages| usages.get(key)),
                named_usage,
            )
        })
        .collect()
}

fn extract_legacy(raw: &Value) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    if let Some(limits) = raw.get("limits").and_then(Value::as_array) {
        for (index, item) in limits.iter().enumerate() {
            let id = period_id(item.get("window").unwrap_or(item))
                .unwrap_or(WindowId::OtherLimit(index));
            let label = match id {
                WindowId::FiveHours => "5h".into(),
                WindowId::Week => "wk".into(),
                _ => item["name"]
                    .as_str()
                    .map_or_else(|| format!("limit {}", index + 1), str::to_owned),
            };
            candidates.push(candidate(
                id,
                Source::LegacyLimit,
                label,
                Some(item.get("detail").unwrap_or(item)),
                legacy_usage,
            ));
        }
    }
    if let Some(usage) = raw.get("usage").filter(|usage| !usage.is_null()) {
        candidates.push(candidate(
            WindowId::UnspecifiedUsage,
            Source::LegacyUsage,
            "wk".into(),
            Some(usage),
            legacy_usage,
        ));
    }
    candidates
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|value| value.is_finite())
}

fn named_usage(entry: &Value) -> Result<f64, QuotaError> {
    // The current API uses 0–100 percentages, unchanged by the official client.
    // Fractional-ratio compatibility is intentionally no longer supported.
    number(&entry["used_ratio"]).ok_or(QuotaError::Protocol("invalid Kimi quota ratio"))
}

fn legacy_count(entry: &Value, key: &str) -> Result<Option<f64>, QuotaError> {
    entry
        .get(key)
        .filter(|value| !value.is_null())
        .map(|value| number(value).ok_or(QuotaError::Protocol("invalid Kimi quota usage")))
        .transpose()
}

fn legacy_usage(entry: &Value) -> Result<f64, QuotaError> {
    let limit = number(&entry["limit"])
        .filter(|value| *value > 0.0)
        .ok_or(QuotaError::Protocol("invalid Kimi quota limit"))?;
    let used = legacy_count(entry, "used")?;
    let remaining = legacy_count(entry, "remaining")?;
    if let (Some(used), Some(remaining)) = (used, remaining)
        && !approximately_equal(used + remaining, limit)
    {
        return Err(QuotaError::Protocol("inconsistent Kimi quota usage"));
    }
    let used = used
        .or_else(|| remaining.map(|remaining| limit - remaining))
        .filter(|used| used.is_finite() && *used >= 0.0)
        .ok_or(QuotaError::Protocol("Kimi quota usage unavailable"))?;
    Ok(used / limit * 100.0)
}

fn period_id(window: &Value) -> Option<WindowId> {
    match (number(&window["duration"]), window["timeUnit"].as_str()) {
        (Some(300.0), Some("TIME_UNIT_MINUTE")) | (Some(5.0), Some("TIME_UNIT_HOUR")) => {
            Some(WindowId::FiveHours)
        }
        (Some(7.0), Some("TIME_UNIT_DAY")) => Some(WindowId::Week),
        _ => None,
    }
}

fn reset_time(entry: &Value) -> Result<Option<DateTime<Utc>>, QuotaError> {
    ["reset_time", "resetTime", "reset_at", "resetAt"]
        .into_iter()
        .find_map(|key| entry[key].as_str())
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse()
                .map_err(|_| QuotaError::Protocol("invalid Kimi quota reset"))
        })
        .transpose()
}

fn same_reset(left: &Candidate, right: &Candidate) -> bool {
    matches!((left.resets_at, right.resets_at), (Some(left), Some(right))
        if left.timestamp() == right.timestamp())
}

fn approximately_equal(left: f64, right: f64) -> bool {
    left.is_finite()
        && right.is_finite()
        && (left - right).abs() <= 1e-9 * left.abs().max(right.abs()).max(1.0)
}

fn associate_legacy(named: &[Candidate], legacy: &mut Vec<Candidate>, mixed: bool) {
    legacy.retain_mut(|candidate| match (candidate.source, candidate.id) {
        (Source::LegacyUsage, WindowId::UnspecifiedUsage) => {
            // Without named windows, retain the legacy weekly display convention.
            // With named windows, an unknown period needs a unique, matching 7d reset.
            let matches_week = named
                .iter()
                .any(|named| named.id == WindowId::Week && same_reset(candidate, named));
            let ambiguous = named
                .iter()
                .any(|named| named.id != WindowId::Week && same_reset(candidate, named));
            if !mixed || (matches_week && !ambiguous) {
                candidate.id = WindowId::Week;
                true
            } else {
                false
            }
        }
        // Unknown legacy periods retain their old labels only on legacy-only responses.
        // A display name such as "wk" is never evidence of a standard window identity.
        (Source::LegacyLimit, WindowId::OtherLimit(_)) => !mixed,
        _ => true,
    });
}

enum LegacyChoice<'a> {
    Missing,
    Valid(&'a Candidate),
    Invalid(QuotaError),
    Conflict,
}

fn legacy_choice(id: WindowId, legacy: &[Candidate]) -> LegacyChoice<'_> {
    let mut selected = LegacyChoice::Missing;
    for candidate in legacy.iter().filter(|candidate| candidate.id == id) {
        match &candidate.quality {
            Quality::Valid(used) => {
                if let LegacyChoice::Valid(previous) = &selected {
                    let Quality::Valid(previous_used) = previous.quality else {
                        unreachable!("only valid legacy candidates are selected");
                    };
                    let resets_agree = same_reset(previous, candidate)
                        || (previous.resets_at.is_none() && candidate.resets_at.is_none());
                    if !approximately_equal(previous_used, *used) || !resets_agree {
                        return LegacyChoice::Conflict;
                    }
                } else {
                    selected = LegacyChoice::Valid(candidate);
                }
            }
            Quality::Invalid(error) if matches!(selected, LegacyChoice::Missing) => {
                selected = LegacyChoice::Invalid(error.clone());
            }
            _ => {}
        }
    }
    selected
}

fn select<'a>(
    named: Option<&'a Candidate>,
    id: WindowId,
    legacy: &'a [Candidate],
) -> Result<Option<&'a Candidate>, QuotaError> {
    // A nonzero, valid named window is authoritative, regardless of legacy quality.
    if let Some(candidate) = named
        && matches!(candidate.quality, Quality::Valid(used) if used > 0.0)
    {
        return Ok(Some(candidate));
    }
    match legacy_choice(id, legacy) {
        LegacyChoice::Conflict => Err(QuotaError::Protocol("conflicting Kimi quota windows")),
        LegacyChoice::Valid(legacy) => {
            if let Some(named) = named
                && matches!(named.quality, Quality::Valid(0.0))
            {
                // Correct a zero only with positive usage from the same reset period.
                return Ok(Some(
                    if matches!(legacy.quality, Quality::Valid(used) if used > 0.0)
                        && same_reset(named, legacy)
                    {
                        legacy
                    } else {
                        named
                    },
                ));
            }
            // Missing/invalid ratios may use an independently identified legacy window.
            Ok(Some(legacy))
        }
        legacy => match named.map(|candidate| &candidate.quality) {
            Some(Quality::Valid(_)) => Ok(named),
            Some(Quality::Invalid(error)) => Err(error.clone()),
            _ => match legacy {
                LegacyChoice::Invalid(error) => Err(error),
                _ => Ok(None),
            },
        },
    }
}

impl Candidate {
    fn to_window(&self) -> KimiQuotaWindow {
        let Quality::Valid(used) = self.quality else {
            unreachable!("only valid candidates can become quota windows");
        };
        KimiQuotaWindow {
            label: self.label.clone(),
            used_percent: used.clamp(0.0, 100.0),
            remaining_percent: (100.0 - used).clamp(0.0, 100.0),
            resets_at: self.resets_at,
        }
    }
}

pub(super) fn parse(raw: &Value) -> Result<Vec<KimiQuotaWindow>, QuotaError> {
    let named = extract_named(raw);
    let mut legacy = extract_legacy(raw);
    let mixed = named
        .iter()
        .any(|candidate| !matches!(candidate.quality, Quality::Missing));
    associate_legacy(&named, &mut legacy, mixed);

    // Preserve legacy ordering when all recognized named windows are missing/null.
    let ids: Vec<_> = if mixed {
        named.iter().map(|candidate| candidate.id).collect()
    } else {
        let mut ids = Vec::new();
        for candidate in &legacy {
            if !ids.contains(&candidate.id) {
                ids.push(candidate.id);
            }
        }
        ids
    };
    let windows = ids
        .into_iter()
        .map(|id| {
            select(
                named.iter().find(|candidate| candidate.id == id),
                id,
                &legacy,
            )
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .map(Candidate::to_window)
        .collect::<Vec<_>>();
    if windows.is_empty() {
        return Err(QuotaError::Protocol("Kimi quota windows unavailable"));
    }
    Ok(windows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn periods_are_identified_only_by_known_duration_and_unit_pairs() {
        for (duration, unit, expected) in [
            (json!(300), "TIME_UNIT_MINUTE", Some(WindowId::FiveHours)),
            (json!("5"), "TIME_UNIT_HOUR", Some(WindowId::FiveHours)),
            (json!(7), "TIME_UNIT_DAY", Some(WindowId::Week)),
            (json!(30), "TIME_UNIT_DAY", None),
            (json!(31), "TIME_UNIT_DAY", None),
            (json!(300), "TIME_UNIT_UNKNOWN", None),
        ] {
            assert_eq!(
                period_id(&json!({"duration": duration, "timeUnit": unit})),
                expected
            );
        }
    }

    #[test]
    fn reset_matching_requires_dates_in_the_same_utc_second() {
        let make = |reset: Value| {
            candidate(
                WindowId::Week,
                Source::Named,
                "wk".into(),
                Some(&json!({"used_ratio": 0, "reset_time": reset})),
                named_usage,
            )
        };
        let base = make(json!("2026-10-03T16:43:44Z"));
        for equivalent in [
            "2026-10-03T16:43:44.982282Z",
            "2026-10-04T01:43:44.123456789+09:00",
        ] {
            assert!(same_reset(&base, &make(json!(equivalent))));
        }
        for different in [json!("2026-10-03T16:43:45Z"), json!("invalid"), Value::Null] {
            assert!(!same_reset(&base, &make(different)));
        }
    }

    #[test]
    fn legacy_quality_checks_counts_before_clamping() {
        assert!(approximately_equal(
            legacy_usage(&json!({"limit": 0.3, "used": 0.1, "remaining": 0.2})).unwrap(),
            100.0 / 3.0
        ));
        assert_eq!(
            legacy_usage(&json!({"limit": 100, "used": 120, "remaining": -20})).unwrap(),
            120.0
        );
        for entry in [
            json!({"limit": 100, "used": 120, "remaining": 0}),
            json!({"limit": 100, "used": -1}),
            json!({"limit": 100, "remaining": 101}),
            json!({"limit": 100, "used": "NaN", "remaining": 50}),
            json!({"limit": "inf", "used": 1}),
            json!({"limit": 0, "used": 0}),
        ] {
            assert!(legacy_usage(&entry).is_err(), "{entry}");
        }
    }
}
