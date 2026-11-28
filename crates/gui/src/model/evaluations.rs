//! The Evaluations panel's model: which evaluation groups exist, which one is
//! open, and what its members, trials, report and replay show.
//!
//! Everything on disk is read by [`EvaluationReader`] on a worker thread. The
//! UI thread only starts a read, drains finished ones through a channel and
//! draws what it holds, so a slow or stuck read never blocks a frame. A result
//! carries the generation of the selection it was started for, and one whose
//! selection has changed since is dropped.
//!
//! Time is an argument ([`EvaluationsModel::poll_once`] takes `now`), so the
//! polling schedule is the same code in the GUI and in tests.

mod markdown;
pub mod present;
mod replay;
