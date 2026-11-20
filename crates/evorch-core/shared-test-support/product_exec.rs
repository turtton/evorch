//! F4-P: start the product — the ordinary `evorch-core` entry, built without
//! features and handed over as an immutable artifact — in directories of its
//! own, observe it from outside, and take it back completely.
//!
//! What this fixture is: a way to run a *separately built* binary through its
//! real arguments, config and public wire. What it is not: evidence that two
//! products hand over to each other. A harness that execs into the product
//! (see `tests/f4_product_fixture.rs`) measures the product's side only.
//!
//! The product gains nothing for this: no flag, no environment variable, no
//! feature. `CARGO_MANIFEST_DIR` and `CARGO_BIN_EXE_*` are read at run time
//! only, never with a compile-time macro.
#![allow(dead_code)]

#[path = "product_exec/artifact.rs"]
pub mod artifact;
#[path = "product_exec/env.rs"]
pub mod env;
#[path = "product_exec/intervals.rs"]
pub mod intervals;
#[path = "product_exec/launch.rs"]
pub mod launch;
#[path = "product_exec/measure.rs"]
pub mod measure;
#[path = "product_exec/names.rs"]
pub mod names;
#[path = "product_exec/observe.rs"]
pub mod observe;
#[path = "product_exec/old_core.rs"]
pub mod old_core;
#[path = "product_exec/process.rs"]
pub mod process;
#[path = "product_exec/wire.rs"]
pub mod wire;
