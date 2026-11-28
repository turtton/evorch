//! Token usage statistics tab, opened from the footer.
//!
//! A shared toolbar selects the period, cost mode and filters; the view below
//! groups the usage ledger. Loads run on a worker thread so the render thread
//! never waits on SQLite.
