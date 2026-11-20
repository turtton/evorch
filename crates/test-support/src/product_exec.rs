//! Registration-only module of the F4-P product launch fixture.
//!
//! The integration PR I0 registers this module so that F4-P can fill it in
//! without touching `lib.rs` or the manifest. It deliberately holds no
//! fixture yet: the sealed artifact contract it will build on is
//! [`crate::product_artifact`].
