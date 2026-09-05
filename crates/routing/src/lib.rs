//! メッセージとタスクを適切な処理先へ振り分ける層です。

mod affinity;
pub mod compose;
mod credential;
pub mod env;
mod error;
pub mod factory;
mod failure;
mod profile;
mod router;

pub use affinity::SessionAffinity;
pub use compose::{
    ComposeDeps, ComposedProvider, ComposedProviders, compose_providers, default_logical_models,
};
pub use credential::CredentialRef;
pub use env::{EnvLookup, MapEnv, ProcessEnv};
pub use error::RoutingError;
pub use failure::FailureKind;
pub use profile::ProviderProfile;
pub use router::{ResolvedRoute, Router};
