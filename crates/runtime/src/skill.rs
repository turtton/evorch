//! SKILL.md ローダ中核 (issue #53)。
//!
//! PR#48 の area module 規約に従い、サブモジュールは非公開とし、公開 API は
//! このモジュールから re-export する。frontmatter の分割・解析・agentskills
//! 仕様に基づく検証に加え、skill ディレクトリの発見とメタデータレジストリを
//! 提供する。

mod discovery;
mod frontmatter;
mod registry;
mod resource;

pub use discovery::{default_skill_dirs, discover_skills};
pub use frontmatter::{
    SkillFrontmatter, SkillValidationError, parse_and_validate, split_frontmatter,
};
pub use registry::{SkillDiagnostic, SkillEntry, SkillLoadError, SkillRegistry, SkillScope};
pub use resource::{SkillResourceError, read_skill_resource};
