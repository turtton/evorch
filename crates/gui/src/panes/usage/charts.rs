//! Shared chart parts for the Usage tab: palette, sparkline, legend and grid.
//!
//! The parts live in [`crate::panes::usage_widgets`]; this keeps the ledger's token layers.

use crate::panes::usage_widgets::{CACHE_READ, CACHE_WRITE, Layer, OUTPUT, UNCACHED_INPUT};
pub use crate::panes::usage_widgets::{
    SERIES, Tile, day_plot, entity_color, heat_grid, other_color, section, segment, stacked_bars,
    stacked_day_bars, tiles,
};

use crate::model::usage_stats::UsageTotals;

/// A named token measure stacked in daily bars.
pub type TokenLayer = Layer<UsageTotals>;

/// Input split by cache use, then output. Cache charts use the first three.
pub const TOKEN_LAYERS: [TokenLayer; 4] = [
    (UNCACHED_INPUT, UsageTotals::uncached_input),
    (CACHE_WRITE, |totals| totals.tokens.cache_write),
    (CACHE_READ, |totals| totals.tokens.cache_read),
    (OUTPUT, |totals| totals.tokens.output),
];
