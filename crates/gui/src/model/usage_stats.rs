//! Usage ledger aggregation for the Usage tab.
//!
//! Request rows and rolled-up daily rows are normalised into [`UsageFact`]s,
//! then grouped by a [`UsageDimension`]. Costs are computed at aggregation
//! time so the viewer can switch between recorded and current prices.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::mpsc;

use config::types::provider::ModelPricing;
use storage::usage::{UsageDailyRow, UsageRequestRow, UsageStatus};

use super::telemetry::TokenUsage;
use super::telemetry::pricing::UsagePricing;

#[path = "usage_stats/day.rs"]
pub mod day;
pub use day::LocalDay;

#[path = "usage_stats/series.rs"]
pub mod series;

/// Period shown by the Usage tab, ending today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UsageRange {
    Today,
    #[default]
    Last7Days,
    Last30Days,
    ThisMonth,
    Last90Days,
    AllTime,
}

impl UsageRange {
    pub const ALL: [Self; 6] = [
        Self::Today,
        Self::Last7Days,
        Self::Last30Days,
        Self::ThisMonth,
        Self::Last90Days,
        Self::AllTime,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Today => "Today",
            Self::Last7Days => "Last 7 days",
            Self::Last30Days => "Last 30 days",
            Self::ThisMonth => "This month",
            Self::Last90Days => "Last 90 days",
            Self::AllTime => "All time",
        }
    }

    /// First included day, or `None` for no lower bound.
    pub fn first_day(self, today: LocalDay) -> Option<LocalDay> {
        match self {
            Self::Today => Some(today),
            Self::Last7Days => Some(today.add_days(-6)),
            Self::Last30Days => Some(today.add_days(-29)),
            Self::ThisMonth => Some(today.month_start()),
            Self::Last90Days => Some(today.add_days(-89)),
            Self::AllTime => None,
        }
    }
}

/// Which price a cost is computed from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CostMode {
    /// The price recorded with each request; unrecorded tokens use current prices.
    #[default]
    Recorded,
    /// Current prices for every token, comparable across price changes.
    Current,
}

impl CostMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Recorded => "Recorded price",
            Self::Current => "Current price",
        }
    }
}

/// Grouping key for tables and charts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UsageDimension {
    #[default]
    Day,
    Week,
    Month,
    Project,
    Thread,
    Provider,
    Model,
    Role,
    Purpose,
}

impl UsageDimension {
    pub const ALL: [Self; 9] = [
        Self::Day,
        Self::Week,
        Self::Month,
        Self::Project,
        Self::Thread,
        Self::Provider,
        Self::Model,
        Self::Role,
        Self::Purpose,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Day => "Day",
            Self::Week => "Week",
            Self::Month => "Month",
            Self::Project => "Project",
            Self::Thread => "Thread",
            Self::Provider => "Provider",
            Self::Model => "Model",
            Self::Role => "Role",
            Self::Purpose => "Purpose",
        }
    }

    pub const fn is_time(self) -> bool {
        matches!(self, Self::Day | Self::Week | Self::Month)
    }

    /// Raw grouping key; empty when the dimension is unknown for the fact.
    pub fn key(self, fact: &UsageFact) -> String {
        match self {
            Self::Day => fact.day.to_string(),
            Self::Week => fact.day.week_start().to_string(),
            Self::Month => fact.day.month_label(),
            Self::Project => fact.project_id.clone().unwrap_or_default(),
            Self::Thread => fact.thread_id.clone().unwrap_or_default(),
            Self::Provider => fact.provider_label(),
            Self::Model => fact.model.clone(),
            Self::Role => fact.role.clone().unwrap_or_default(),
            Self::Purpose => fact.purpose.clone().unwrap_or_default(),
        }
    }
}

/// One request, or one rolled-up daily bucket, normalised for aggregation.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageFact {
    pub day: LocalDay,
    /// Request time; `None` for rolled-up daily totals.
    pub at_ns: Option<i64>,
    pub provider: String,
    pub profile: Option<String>,
    pub model: String,
    pub project_id: Option<String>,
    pub thread_id: Option<String>,
    pub role: Option<String>,
    pub purpose: Option<String>,
    pub requests: u64,
    pub failed: u64,
    /// Input includes cache reads and writes.
    pub tokens: TokenUsage,
    pub reasoning: u64,
    /// Sum of costs recorded with the requests.
    pub recorded_cost: f64,
    /// Tokens of requests recorded without a price.
    pub unpriced: TokenUsage,
    pub ttft_sum_ms: u64,
    pub ttft_count: u64,
    pub duration_sum_ms: u64,
}

impl UsageFact {
    pub fn from_request(row: &UsageRequestRow) -> Option<Self> {
        let record = &row.record;
        let tokens = TokenUsage {
            input: record.input_tokens,
            output: record.output_tokens,
            cache_read: record.cache_read_tokens,
            cache_write: record.cache_write_tokens,
        };
        Some(Self {
            day: LocalDay::parse(&row.day)?,
            at_ns: Some(record.at_ns),
            provider: record.provider.clone(),
            profile: record.profile.clone(),
            model: record.model.clone(),
            project_id: row.project_id.clone(),
            thread_id: row.thread_id.clone(),
            role: record.role.clone(),
            purpose: record.purpose.clone(),
            requests: 1,
            failed: u64::from(record.status == UsageStatus::Failed),
            tokens,
            reasoning: record.reasoning_tokens.unwrap_or_default(),
            recorded_cost: record.cost_usd.unwrap_or_default(),
            unpriced: if record.cost_usd.is_some() {
                TokenUsage::default()
            } else {
                tokens
            },
            ttft_sum_ms: record.ttft_ms.unwrap_or_default(),
            ttft_count: u64::from(record.ttft_ms.is_some()),
            duration_sum_ms: record.duration_ms,
        })
    }

    pub fn from_daily(row: &UsageDailyRow) -> Option<Self> {
        Some(Self {
            day: LocalDay::parse(&row.day)?,
            at_ns: None,
            provider: row.provider.clone(),
            profile: row.profile.clone(),
            model: row.model.clone(),
            project_id: row.project_id.clone(),
            thread_id: None,
            role: row.role.clone(),
            purpose: row.purpose.clone(),
            requests: row.request_count,
            failed: row.failed_count,
            tokens: TokenUsage {
                input: row.input_tokens,
                output: row.output_tokens,
                cache_read: row.cache_read_tokens,
                cache_write: row.cache_write_tokens,
            },
            reasoning: row.reasoning_tokens,
            recorded_cost: row.cost_usd,
            unpriced: TokenUsage {
                input: row.unpriced_input_tokens,
                output: row.unpriced_output_tokens,
                cache_read: row.unpriced_cache_read_tokens,
                cache_write: row.unpriced_cache_write_tokens,
            },
            ttft_sum_ms: row.ttft_sum_ms,
            ttft_count: row.ttft_count,
            duration_sum_ms: row.duration_sum_ms,
        })
    }

    /// The routing profile when known, otherwise the provider label.
    pub fn provider_label(&self) -> String {
        self.profile
            .clone()
            .unwrap_or_else(|| self.provider.clone())
    }
}

/// Facts read for one range, plus today's day on the ledger clock.
#[derive(Debug, Clone, Default)]
pub struct UsageDataset {
    pub today: Option<LocalDay>,
    pub facts: Vec<UsageFact>,
}

impl UsageDataset {
    pub fn from_rows(
        today: LocalDay,
        requests: &[UsageRequestRow],
        daily: &[UsageDailyRow],
    ) -> Self {
        let mut facts: Vec<_> = daily
            .iter()
            .filter_map(UsageFact::from_daily)
            .chain(requests.iter().filter_map(UsageFact::from_request))
            .collect();
        facts.sort_by_key(|fact| (fact.day, fact.at_ns));
        Self {
            today: Some(today),
            facts,
        }
    }

    /// Reads `range` from the database without touching the writer. A missing
    /// database reads as empty rather than being created by a viewer.
    pub fn load(config: &storage::StorageConfig, range: UsageRange) -> Result<Self, String> {
        if !config.db_path.exists() {
            return Ok(Self::default());
        }
        let database = storage::Database::open(config).map_err(|error| error.to_string())?;
        let today = database
            .usage_local_today()
            .map_err(|error| error.to_string())?;
        let today_day =
            LocalDay::parse(&today).ok_or_else(|| format!("invalid local day {today}"))?;
        let from = range
            .first_day(today_day)
            .map_or_else(|| "0000-01-01".to_owned(), |day| day.to_string());
        let requests = database
            .usage_requests_in_days(&from, &today)
            .map_err(|error| error.to_string())?;
        let daily = database
            .usage_daily_between(&from, &today)
            .map_err(|error| error.to_string())?;
        Ok(Self::from_rows(today_day, &requests, &daily))
    }
}

/// Facts kept by the active filters. `None` keeps every value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageFilter {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub project: Option<String>,
    pub role: Option<String>,
}

impl UsageFilter {
    pub fn matches(&self, fact: &UsageFact) -> bool {
        let keep = |wanted: &Option<String>, actual: String| {
            wanted.as_ref().is_none_or(|wanted| *wanted == actual)
        };
        keep(&self.provider, UsageDimension::Provider.key(fact))
            && keep(&self.model, UsageDimension::Model.key(fact))
            && keep(&self.project, UsageDimension::Project.key(fact))
            && keep(&self.role, UsageDimension::Role.key(fact))
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Distinct values offered by each filter.
pub fn filter_options(facts: &[UsageFact], dimension: UsageDimension) -> Vec<String> {
    facts
        .iter()
        .map(|fact| dimension.key(fact))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Accumulated metrics for a group of facts.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct UsageTotals {
    pub requests: u64,
    pub failed: u64,
    pub tokens: TokenUsage,
    pub reasoning: u64,
    pub cost: f64,
    /// Some tokens had no known price, so `cost` is a lower bound.
    pub unpriced: bool,
    pub ttft_sum_ms: u64,
    pub ttft_count: u64,
    pub duration_sum_ms: u64,
}

impl UsageTotals {
    fn add(&mut self, fact: &UsageFact, cost: FactCost) {
        self.requests += fact.requests;
        self.failed += fact.failed;
        self.tokens.input += fact.tokens.input;
        self.tokens.output += fact.tokens.output;
        self.tokens.cache_read += fact.tokens.cache_read;
        self.tokens.cache_write += fact.tokens.cache_write;
        self.reasoning += fact.reasoning;
        self.cost += cost.usd;
        self.unpriced |= cost.unpriced;
        self.ttft_sum_ms += fact.ttft_sum_ms;
        self.ttft_count += fact.ttft_count;
        self.duration_sum_ms += fact.duration_sum_ms;
    }

    pub fn merge(&mut self, other: &Self) {
        self.requests += other.requests;
        self.failed += other.failed;
        self.tokens.input += other.tokens.input;
        self.tokens.output += other.tokens.output;
        self.tokens.cache_read += other.tokens.cache_read;
        self.tokens.cache_write += other.tokens.cache_write;
        self.reasoning += other.reasoning;
        self.cost += other.cost;
        self.unpriced |= other.unpriced;
        self.ttft_sum_ms += other.ttft_sum_ms;
        self.ttft_count += other.ttft_count;
        self.duration_sum_ms += other.duration_sum_ms;
    }

    /// Input tokens that were neither read from nor written to the cache.
    pub fn uncached_input(&self) -> u64 {
        self.tokens
            .input
            .saturating_sub(self.tokens.cache_read)
            .saturating_sub(self.tokens.cache_write)
    }

    pub fn total_tokens(&self) -> u64 {
        self.tokens.input + self.tokens.output
    }

    /// Share of input served from cache, in percent.
    pub fn cache_hit_rate(&self) -> Option<f64> {
        (self.tokens.input > 0).then(|| self.tokens.cache_hit_rate())
    }

    pub fn success_rate(&self) -> Option<f64> {
        (self.requests > 0)
            .then(|| (self.requests - self.failed) as f64 / self.requests as f64 * 100.0)
    }

    pub fn average_ttft_ms(&self) -> Option<u64> {
        (self.ttft_count > 0).then(|| self.ttft_sum_ms / self.ttft_count)
    }

    /// Blended dollars per million total tokens.
    pub fn cost_per_million(&self) -> Option<f64> {
        let total = self.total_tokens();
        (total > 0 && self.cost > 0.0).then(|| self.cost / total as f64 * 1_000_000.0)
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct FactCost {
    usd: f64,
    unpriced: bool,
}

/// Prices facts, memoising each model's price lookup for one aggregation pass.
pub struct CostCalculator<'a> {
    pricing: &'a UsagePricing,
    mode: CostMode,
    cache: HashMap<(String, Option<String>, String), Option<ModelPricing>>,
}

impl<'a> CostCalculator<'a> {
    pub fn new(pricing: &'a UsagePricing, mode: CostMode) -> Self {
        Self {
            pricing,
            mode,
            cache: HashMap::new(),
        }
    }

    fn cost(&mut self, fact: &UsageFact) -> FactCost {
        let (base, tokens) = match self.mode {
            CostMode::Recorded => (fact.recorded_cost, fact.unpriced),
            CostMode::Current => (0.0, fact.tokens),
        };
        if tokens == TokenUsage::default() {
            return FactCost {
                usd: base,
                unpriced: false,
            };
        }
        let key = (
            fact.provider.clone(),
            fact.profile.clone(),
            fact.model.clone(),
        );
        let pricing = *self.cache.entry(key).or_insert_with(|| {
            self.pricing
                .pricing(&fact.provider, fact.profile.as_deref(), &fact.model)
        });
        match tokens.estimated_cost(pricing) {
            Some(cost) => FactCost {
                usd: base + cost,
                unpriced: false,
            },
            None => FactCost {
                usd: base,
                unpriced: true,
            },
        }
    }
}

/// One group in a breakdown, with an optional per-model split.
#[derive(Debug, Clone, PartialEq)]
pub struct BreakdownRow {
    pub key: String,
    pub totals: UsageTotals,
    pub models: Vec<(String, UsageTotals)>,
}

/// Groups the filtered facts. Time dimensions sort newest first; others by cost.
pub fn breakdown(
    facts: &[UsageFact],
    filter: &UsageFilter,
    dimension: UsageDimension,
    costs: &mut CostCalculator<'_>,
) -> (Vec<BreakdownRow>, UsageTotals) {
    let mut groups: BTreeMap<String, (UsageTotals, BTreeMap<String, UsageTotals>)> =
        BTreeMap::new();
    let mut total = UsageTotals::default();
    for fact in facts.iter().filter(|fact| filter.matches(fact)) {
        let cost = costs.cost(fact);
        let (totals, models) = groups.entry(dimension.key(fact)).or_default();
        totals.add(fact, cost);
        models
            .entry(fact.model.clone())
            .or_default()
            .add(fact, cost);
        total.add(fact, cost);
    }
    let mut rows: Vec<_> = groups
        .into_iter()
        .map(|(key, (totals, models))| {
            let mut models: Vec<_> = models.into_iter().collect();
            models.sort_by(|left, right| right.1.cost.total_cmp(&left.1.cost));
            BreakdownRow {
                key,
                totals,
                models,
            }
        })
        .collect();
    if dimension.is_time() {
        rows.reverse();
    } else {
        rows.sort_by(|left, right| {
            right
                .totals
                .cost
                .total_cmp(&left.totals.cost)
                .then_with(|| right.totals.total_tokens().cmp(&left.totals.total_tokens()))
        });
    }
    (rows, total)
}

/// Background loads keyed by the range they were requested for.
#[derive(Default)]
pub struct UsageLoader {
    pending: Option<(UsageRange, mpsc::Receiver<Result<UsageDataset, String>>)>,
}

impl UsageLoader {
    pub fn is_loading(&self) -> bool {
        self.pending.is_some()
    }

    /// Starts a load on a worker thread, replacing any load still in flight.
    /// `repaint` is woken when the result is ready.
    pub fn start(
        &mut self,
        config: storage::StorageConfig,
        range: UsageRange,
        repaint: Option<egui::Context>,
    ) {
        let (sender, receiver) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("usage-stats-load".into())
            .spawn(move || {
                let _ = sender.send(UsageDataset::load(&config, range));
                if let Some(context) = repaint {
                    context.request_repaint();
                }
            });
        match spawned {
            Ok(_) => self.pending = Some((range, receiver)),
            Err(error) => tracing::warn!(%error, "failed to start usage stats load"),
        }
    }

    /// The finished load, if any.
    pub fn poll(&mut self) -> Option<(UsageRange, Result<UsageDataset, String>)> {
        let (range, receiver) = self.pending.as_ref()?;
        let range = *range;
        match receiver.try_recv() {
            Ok(result) => {
                self.pending = None;
                Some((range, result))
            }
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.pending = None;
                Some((range, Err("usage stats load stopped".into())))
            }
        }
    }
}

#[cfg(test)]
#[path = "usage_stats_tests.rs"]
mod tests;
