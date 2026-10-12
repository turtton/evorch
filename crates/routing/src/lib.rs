//! メッセージとタスクを適切な処理先へ振り分ける層です。

mod affinity;
mod chain;
pub mod compose;
mod credential;
pub mod env;
mod error;
pub mod factory;
mod failure;
mod health;
mod profile;
mod router;

pub use affinity::{RoutePin, SessionAffinity};
pub use chain::{Chain, ChainMember, FallbackCycle, FallbackStep};
pub use compose::{ComposeDeps, ComposedProvider, ComposedProviders, compose_providers};
pub use credential::CredentialRef;
pub use env::{EnvLookup, MapEnv, ProcessEnv};
pub use error::RoutingError;
pub use failure::{FailureKind, is_transient, retry_after};
pub use health::{Cooldown, ProviderHealth};
pub use profile::ProviderProfile;
pub use router::{ResolvedRoute, Router, pin};
