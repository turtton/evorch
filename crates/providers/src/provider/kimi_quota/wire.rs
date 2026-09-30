use super::{KimiQuotaWindow, QuotaError};
use serde_json::Value;

pub(super) fn parse(raw: &Value) -> Result<Vec<KimiQuotaWindow>, QuotaError> {
    let mut windows = Vec::new();
    // New membership plans expose named windows; their presence is authoritative.
    if let Some(usages) = raw.get("usages").and_then(Value::as_object) {
        for (key, label) in [
            ("limit_5h", "5h"),
            ("limit_7d", "wk"),
            ("limit_month_total", "month"),
            ("limit_month_code", "code month"),
        ] {
            if let Some(entry) = usages.get(key).filter(|entry| !entry.is_null()) {
                let ratio = number(&entry["used_ratio"])
                    .ok_or(QuotaError::Protocol("invalid Kimi quota ratio"))?;
                windows.push(window(label.into(), ratio * 100.0, entry)?);
            }
        }
    } else {
        // Older plans supply an overall weekly usage and rolling limit details.
        if let Some(limits) = raw.get("limits").and_then(Value::as_array) {
            for (index, item) in limits.iter().enumerate() {
                let detail = item.get("detail").unwrap_or(item);
                let window_data = item.get("window").unwrap_or(item);
                let duration = number(&window_data["duration"]);
                let unit = window_data["timeUnit"].as_str().unwrap_or("");
                let label = match (duration, unit) {
                    (Some(300.0), "TIME_UNIT_MINUTE") | (Some(5.0), "TIME_UNIT_HOUR") => {
                        "5h".into()
                    }
                    (Some(7.0), "TIME_UNIT_DAY") => "wk".into(),
                    _ => item["name"]
                        .as_str()
                        .map_or_else(|| format!("limit {}", index + 1), str::to_owned),
                };
                windows.push(legacy_window(label, detail)?);
            }
        }
        if let Some(usage) = raw.get("usage").filter(|value| value.is_object()) {
            windows.push(legacy_window("wk".into(), usage)?);
        }
    }
    if windows.is_empty() {
        return Err(QuotaError::Protocol("Kimi quota windows unavailable"));
    }
    Ok(windows)
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|value| value.is_finite())
}

fn legacy_window(label: String, entry: &Value) -> Result<KimiQuotaWindow, QuotaError> {
    let limit = number(&entry["limit"])
        .filter(|value| *value > 0.0)
        .ok_or(QuotaError::Protocol("invalid Kimi quota limit"))?;
    let used = number(&entry["used"])
        .or_else(|| number(&entry["remaining"]).map(|remaining| limit - remaining))
        .ok_or(QuotaError::Protocol("Kimi quota usage unavailable"))?;
    window(label, used / limit * 100.0, entry)
}

fn window(label: String, used: f64, entry: &Value) -> Result<KimiQuotaWindow, QuotaError> {
    if !used.is_finite() || used < 0.0 {
        return Err(QuotaError::Protocol("invalid Kimi quota usage"));
    }
    let resets_at = ["reset_time", "resetTime", "reset_at", "resetAt"]
        .into_iter()
        .find_map(|key| entry[key].as_str())
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse()
                .map_err(|_| QuotaError::Protocol("invalid Kimi quota reset"))
        })
        .transpose()?;
    Ok(KimiQuotaWindow {
        label,
        used_percent: used.clamp(0.0, 100.0),
        remaining_percent: (100.0 - used).clamp(0.0, 100.0),
        resets_at,
    })
}
