pub use storage::eval::{Attribution, EvalTrace, FailureAttribution};
mod selection;
pub use selection::{dominates, hard_gate, pairwise_tiebreak, select};
mod report;
mod runner;
mod spec;
pub use report::{ArenaReport, Confirmation};
pub use runner::{Runner, run};
pub use spec::{ArenaConfig, ArenaError, ArenaSpec, TaskSpec};
