//! Prompt cache retention: how much of the previous request's measured cache
//! the next request read back. A request's first send can never hit, so this
//! stays near 100% for a healthy cache regardless of session length, unlike
//! the billed `cache_read / input` ratio.

use event_bus::{CACHE_RETENTION_WARNING_THRESHOLD, CacheBaselineMissing, CacheComparison};

/// Cache reuse of one completed request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestReuse {
    Compared {
        read: u64,
        previous: u64,
    },
    NoBaseline(CacheBaselineMissing),
    /// The provider client did not observe cache reuse for this request.
    Unobserved,
}

impl RequestReuse {
    pub(super) fn observed(cache_read_tokens: u64, comparison: &CacheComparison) -> Self {
        match comparison {
            CacheComparison::Compared {
                previous_cache_tokens,
                ..
            } => Self::Compared {
                read: cache_read_tokens,
                previous: *previous_cache_tokens,
            },
            CacheComparison::NoBaseline { reason } => Self::NoBaseline(*reason),
        }
    }
}

/// Cache reuse across completed requests of one run or thread.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheReuseSummary {
    pub latest: Option<RequestReuse>,
    /// Cache reads up to each compared request's previous cache. New prefixes
    /// cached on top of the previous one are not counted as extra retention.
    pub retained_tokens: u64,
    pub baseline_tokens: u64,
    pub compared_requests: u32,
    pub completed_requests: u32,
}

fn percent(numerator: u64, denominator: u64) -> Option<f64> {
    (denominator > 0).then(|| {
        let ratio = std::time::Duration::from_secs(numerator).as_secs_f64()
            / std::time::Duration::from_secs(denominator).as_secs_f64();
        (ratio * 100.0).min(100.0)
    })
}

fn is_low(rate: Option<f64>) -> bool {
    rate.is_some_and(|rate| rate < CACHE_RETENTION_WARNING_THRESHOLD * 100.0)
}

fn baseline_missing_label(reason: CacheBaselineMissing) -> &'static str {
    match reason {
        CacheBaselineMissing::NoPreviousRequest => "first request for this model",
        CacheBaselineMissing::Expired => "previous request is over 5 minutes old",
        CacheBaselineMissing::PrefixChanged => "prompt prefix changed, e.g. by compaction",
        CacheBaselineMissing::PreviousUncached => "previous request reported no cache",
        CacheBaselineMissing::InvalidUsage => "provider reported invalid usage",
    }
}

impl super::TelemetryRow {
    /// [`CacheReuseSummary::tooltip`] with this run's billed ratios.
    pub fn cache_tooltip(&self) -> String {
        let billed_latest = self
            .latest_context
            .as_ref()
            .map(|request| request.usage.cache_hit_rate());
        let billed_average = self
            .latest_context
            .is_some()
            .then(|| self.usage.cache_hit_rate());
        self.cache_reuse.tooltip(billed_latest, billed_average)
    }
}

impl CacheReuseSummary {
    pub(super) fn complete(&mut self, reuse: RequestReuse) {
        self.completed_requests = self.completed_requests.saturating_add(1);
        if let RequestReuse::Compared { read, previous } = reuse {
            self.retained_tokens = self.retained_tokens.saturating_add(read.min(previous));
            self.baseline_tokens = self.baseline_tokens.saturating_add(previous);
            self.compared_requests = self.compared_requests.saturating_add(1);
        }
        self.latest = Some(reuse);
    }

    /// Adds another run's totals. `latest` is left to the caller, which knows
    /// which run completed a request most recently.
    pub(super) fn add_totals(&mut self, other: &Self) {
        self.retained_tokens = self.retained_tokens.saturating_add(other.retained_tokens);
        self.baseline_tokens = self.baseline_tokens.saturating_add(other.baseline_tokens);
        self.compared_requests = self
            .compared_requests
            .saturating_add(other.compared_requests);
        self.completed_requests = self
            .completed_requests
            .saturating_add(other.completed_requests);
    }

    pub fn latest_retention(&self) -> Option<f64> {
        match self.latest? {
            RequestReuse::Compared { read, previous } => percent(read.min(previous), previous),
            RequestReuse::NoBaseline(_) | RequestReuse::Unobserved => None,
        }
    }

    pub fn average_retention(&self) -> Option<f64> {
        percent(self.retained_tokens, self.baseline_tokens)
    }

    pub fn latest_is_low(&self) -> bool {
        is_low(self.latest_retention())
    }

    pub fn average_is_low(&self) -> bool {
        is_low(self.average_retention())
    }

    /// `cache 99% (avg 97%)`: the latest request plus the average.
    pub fn label(&self) -> String {
        let current = self
            .latest_retention()
            .map_or_else(|| "cache —".into(), |rate| format!("cache {rate:.0}%"));
        match self.average_retention() {
            Some(average) => format!("{current} (avg {average:.0}%)"),
            None => current,
        }
    }

    /// `avg cache 97.0%`: the average alone, for compact per-run metrics.
    pub fn average_label(&self) -> String {
        self.average_retention().map_or_else(
            || "avg cache —".into(),
            |rate| format!("avg cache {rate:.1}%"),
        )
    }

    /// Explains the retention values and keeps the billed ratio available.
    pub fn tooltip(&self, billed_latest: Option<f64>, billed_average: Option<f64>) -> String {
        let mut lines = vec![
            "Prompt cache retention".to_string(),
            "Share of the previous request's cached prompt read back from cache.".to_string(),
        ];
        lines.push(match (self.latest, self.latest_retention()) {
            (_, Some(rate)) => format!("Latest: {rate:.0}%"),
            (Some(RequestReuse::NoBaseline(reason)), None) => {
                format!("Latest: — ({})", baseline_missing_label(reason))
            }
            (Some(RequestReuse::Unobserved), None) => {
                "Latest: — (cache reuse was not observed)".into()
            }
            (_, None) => "Latest: — (no completed request)".into(),
        });
        let compared = format!(
            "{} of {} requests compared",
            self.compared_requests, self.completed_requests
        );
        lines.push(match self.average_retention() {
            Some(rate) => format!("Average: {rate:.0}% ({compared})"),
            None => format!("Average: — ({compared})"),
        });
        let billed =
            |rate: Option<f64>| rate.map_or_else(|| "—".into(), |rate| format!("{rate:.0}%"));
        if billed_latest.is_some() || billed_average.is_some() {
            lines.push(format!(
                "Billed hit rate (cache read / input): latest {}, average {}",
                billed(billed_latest),
                billed(billed_average)
            ));
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compared(read: u64, previous: u64) -> RequestReuse {
        RequestReuse::Compared { read, previous }
    }

    #[test]
    fn cold_first_request_does_not_lower_the_average() {
        // Given: the 0% -> 50% -> 75% billed pattern of a short healthy run.
        let mut summary = CacheReuseSummary::default();
        summary.complete(RequestReuse::NoBaseline(
            CacheBaselineMissing::NoPreviousRequest,
        ));
        summary.complete(compared(1_000, 1_000));
        summary.complete(compared(3_000, 3_000));
        // Then: retention reports a healthy cache.
        assert_eq!(summary.label(), "cache 100% (avg 100%)");
        assert_eq!(summary.average_label(), "avg cache 100.0%");
        assert_eq!(
            (summary.compared_requests, summary.completed_requests),
            (2, 3)
        );
    }

    #[test]
    fn newly_cached_suffix_does_not_inflate_retention() {
        let mut summary = CacheReuseSummary::default();
        summary.complete(compared(1_500, 1_000));
        summary.complete(compared(500, 1_000));
        assert_eq!(summary.latest_retention(), Some(50.0));
        assert_eq!(summary.average_retention(), Some(75.0));
    }

    #[test]
    fn missing_baseline_shows_a_dash_and_explains_why() {
        let mut summary = CacheReuseSummary::default();
        summary.complete(compared(900, 1_000));
        summary.complete(RequestReuse::NoBaseline(
            CacheBaselineMissing::PrefixChanged,
        ));
        assert_eq!(summary.label(), "cache — (avg 90%)");
        let tooltip = summary.tooltip(Some(12.0), Some(40.0));
        assert!(tooltip.contains(baseline_missing_label(CacheBaselineMissing::PrefixChanged)));
        assert!(tooltip.contains("1 of 2 requests compared"));
        // The billed ratio stays available beside the retention.
        assert!(tooltip.contains("latest 12%, average 40%"));
    }

    #[test]
    fn retention_below_the_regression_threshold_is_low() {
        let mut summary = CacheReuseSummary::default();
        summary.complete(compared(1_000, 1_000));
        summary.complete(compared(400, 1_000));
        assert!(summary.latest_is_low());
        assert!(!summary.average_is_low());
        summary.complete(compared(0, 2_000));
        assert!(summary.average_is_low());
    }

    #[test]
    fn unobserved_runs_have_no_retention() {
        let mut summary = CacheReuseSummary::default();
        assert_eq!(summary.label(), "cache —");
        assert_eq!(summary.average_label(), "avg cache —");
        summary.complete(RequestReuse::Unobserved);
        assert_eq!(summary.latest_retention(), None);
        assert!(!summary.latest_is_low());
    }
}
