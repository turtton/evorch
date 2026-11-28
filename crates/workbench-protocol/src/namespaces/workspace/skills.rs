//! `list_skills`: the skills a message in a project can mention with `@name`.
//!
//! A bounded single reply, never a page: a list cut at an arbitrary place
//! would offer a client an incomplete set without saying so. Only the name and
//! the description travel. The body, the bundled resources and where the skill
//! lives stay in the core, which reads the body when the message is sent.
use crate::{
    limits::MAX_NAME_BYTES,
    validation::{ValidationError, check_all, check_bytes, check_count, check_line},
};
use serde::{Deserialize, Serialize};

/// Most skills one `list_skills` reply carries.
pub const MAX_SKILL_SUMMARIES: usize = 256;
/// Longest skill description, in bytes.
pub const MAX_SKILL_DESCRIPTION_BYTES: usize = 1024;

/// One skill a message can mention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillSummary {
    /// What `send_chat.skills` quotes.
    pub name: String,
    pub description: String,
}

impl SkillSummary {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_line("name", &self.name, MAX_NAME_BYTES, false)?;
        check_bytes(
            "description",
            &self.description,
            MAX_SKILL_DESCRIPTION_BYTES,
            true,
        )
    }
}

/// Validates the skills of one reply.
pub fn validate_skills(skills: &[SkillSummary]) -> Result<(), ValidationError> {
    check_count("skills", skills.len(), MAX_SKILL_SUMMARIES)?;
    check_all(skills, SkillSummary::validate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::{ErrorCode, WorkbenchError};

    fn skill(name: &str, description: &str) -> SkillSummary {
        SkillSummary {
            name: name.into(),
            description: description.into(),
        }
    }

    fn code(error: ValidationError) -> ErrorCode {
        WorkbenchError::from(error).code
    }

    #[test]
    fn a_full_list_of_longest_entries_is_valid() {
        let skills = vec![
            skill(
                &"n".repeat(MAX_NAME_BYTES),
                &"d".repeat(MAX_SKILL_DESCRIPTION_BYTES)
            );
            MAX_SKILL_SUMMARIES
        ];
        validate_skills(&skills).unwrap();
    }

    #[test]
    fn too_many_skills_is_a_limit_not_a_partial_list() {
        let skills = vec![skill("a", ""); MAX_SKILL_SUMMARIES + 1];
        assert_eq!(
            code(validate_skills(&skills).unwrap_err()),
            ErrorCode::LimitExceeded
        );
    }

    #[test]
    fn an_unusable_name_or_description_is_refused() {
        let long = "d".repeat(MAX_SKILL_DESCRIPTION_BYTES + 1);
        assert_eq!(
            code(validate_skills(&[skill("a", &long)]).unwrap_err()),
            ErrorCode::LimitExceeded
        );
        assert_eq!(
            code(validate_skills(&[skill("", "x")]).unwrap_err()),
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            code(validate_skills(&[skill("a\nb", "x")]).unwrap_err()),
            ErrorCode::InvalidRequest
        );
    }
}
