//! システムプロンプト組立 (issue #49)。
//!
//! PR#48 の area module 規約に従い、サブモジュールは非公開とし、公開 API は
//! このモジュールから re-export する。組立はすべて純粋関数で構成され、
//! 同一入力に対してバイト単位で同一の出力を返す (AC3)。

mod assembly;
mod catalog;
mod composition;
mod family;
mod intent_gate;
mod key_triggers;

pub use assembly::{SystemPromptInput, assemble_system_prompt};
pub use catalog::{SystemPromptCatalog, SystemPromptCatalogBuilder, SystemPromptCatalogError};
pub use composition::{CatalogBuildInput, PromptCompositionError, build_catalog};
pub use family::{ModelFamily, classify};
pub use key_triggers::{
    AvailableAgent, AvailableSkill, TriggerSource, default_role_triggers, render_key_triggers,
    triggers_from_availability,
};
