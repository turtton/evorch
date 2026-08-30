//! コマンドを隔離された環境で実行し、承認方針と資格情報を安全に扱う層です。

pub mod approval;
pub mod bwrap;
pub mod composition;
pub mod credential;
pub mod error;
pub mod exec;
#[cfg(feature = "keychain")]
pub mod keychain;
pub mod network;
pub mod policy;

pub use approval::{ApprovalGate, ApprovalOutcome};
pub use bwrap::{BwrapConfig, BwrapSandbox};
pub use composition::production_sandbox;
pub use credential::{CredentialStore, FileCredentialStore, Secret, open_default};
pub use error::{CredentialError, SandboxError};
pub use exec::{CommandSpec, DirectSandbox, Sandbox, WrappedCommand};
#[cfg(feature = "keychain")]
pub use keychain::KeyringCredentialStore;
pub use network::NetworkPolicy;
pub use policy::{Action, ApprovalMode, ApprovalPolicy, Capabilities, PolicyDecision, resolve};
