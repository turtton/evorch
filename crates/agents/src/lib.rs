//! ADR 0002 の capability boundary（権限分離）をランタイム検証可能なデータとして定義するクレート。
//!
//! ロール定義とケイパビリティ境界チェッカーのみを提供し、I/O も async も行わない。

pub mod capability;
pub mod role;

pub use capability::{CapabilityDecision, NetworkAccess, RoleCapabilities};
pub use role::Role;
