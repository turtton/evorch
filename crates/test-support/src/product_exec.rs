//! Registration of the F4-P product launch fixture (module declared by the
//! integration PR I0).
//!
//! The fixture lives in `crates/evorch-core/shared-test-support/product_exec*`
//! and `tests/renderer_support/product_exec*`, not here: it needs
//! `core-instance`, `core-launcher`, the workbench client and the ownership
//! registry, which this crate must not depend on. What this crate owns is the
//! sealed artifact contract the fixture builds on: [`crate::product_artifact`]
//! (`EVORCH_PRODUCT_CORE_EXE`, `EVORCH_PRODUCT_MANIFEST`).
