pub use storage::eval::{Attribution, EvalTrace, FailureAttribution};
pub use storage::eval_variant::{
    ArenaVariant, EvalExecution, EvalStep, PromptVersion, RoleRoute, Topology,
};
mod comparison;
mod execution;
pub use comparison::VariantComparison;
mod selection;
pub use selection::{dominates, hard_gate, pairwise_tiebreak, select};
mod report;
mod runner;
mod spec;
pub use report::{ArenaReport, Confirmation};
pub use runner::{Runner, run};
pub use spec::{ArenaConfig, ArenaError, ArenaSpec, TaskSpec};
