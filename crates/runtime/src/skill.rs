//! SKILL.md ローダ中核 (issue #53)。
//!
//! PR#48 の area module 規約に従い、サブモジュールは非公開とし、公開 API は
//! このモジュールから re-export する。frontmatter の分割・解析・agentskills
//! 仕様に基づく検証を提供する (discovery / registry は別タスク)。

mod frontmatter;

pub use frontmatter::{
    SkillFrontmatter, SkillValidationError, parse_and_validate, split_frontmatter,
};
