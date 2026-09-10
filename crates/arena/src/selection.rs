use crate::EvalTrace;
use std::cmp::Ordering;

pub const fn hard_gate(trace: &EvalTrace) -> bool {
    trace.failure.is_none()
}

fn tokens(trace: &EvalTrace) -> u128 {
    u128::from(trace.input_tokens) + u128::from(trace.output_tokens)
}

pub fn dominates(a: &EvalTrace, b: &EvalTrace) -> bool {
    tokens(a) <= tokens(b)
        && a.elapsed_ms <= b.elapsed_ms
        && (tokens(a) < tokens(b) || a.elapsed_ms < b.elapsed_ms)
}

pub fn pairwise_tiebreak(a: &EvalTrace, b: &EvalTrace) -> Ordering {
    (tokens(a), a.elapsed_ms, &a.config_id).cmp(&(tokens(b), b.elapsed_ms, &b.config_id))
}

pub fn select(traces: &[EvalTrace]) -> Vec<String> {
    let eligible: Vec<_> = traces.iter().filter(|t| hard_gate(t)).collect();
    let mut frontier: Vec<_> = eligible
        .iter()
        .copied()
        .filter(|b| !eligible.iter().any(|a| dominates(a, b)))
        .collect();
    frontier.sort_by(|a, b| pairwise_tiebreak(a, b));
    frontier.into_iter().map(|t| t.config_id.clone()).collect()
}
