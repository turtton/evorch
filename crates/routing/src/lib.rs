//! メッセージとタスクを適切な処理先へ振り分ける層です。

mod affinity;
mod credential;
mod error;
mod failure;
mod profile;
mod router;

pub use affinity::SessionAffinity;
pub use credential::CredentialRef;
pub use error::RoutingError;
pub use failure::FailureKind;
pub use profile::ProviderProfile;
pub use router::{ResolvedRoute, Router};
