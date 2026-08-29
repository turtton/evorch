//! メッセージとタスクを適切な処理先へ振り分ける層です。

mod credential;
mod error;
mod failure;
mod profile;

pub use credential::CredentialRef;
pub use error::RoutingError;
pub use failure::FailureKind;
pub use profile::ProviderProfile;
