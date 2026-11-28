//! Usage ledger aggregation for the Usage tab.
//!
//! Request rows and rolled-up daily rows are normalised into [`UsageFact`]s,
//! then grouped by a [`UsageDimension`]. Costs are computed at aggregation
//! time so the viewer can switch between recorded and current prices.

#[path = "usage_stats/series.rs"]
pub mod series;
