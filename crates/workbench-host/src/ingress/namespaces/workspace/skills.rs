//! `list_skills`: the skills a message in a project can mention.
//!
//! Resolved by the runtime exactly like the registry a sent message reads its
//! skill bodies from (project root, repository trust, user directories,
//! shadowing), without touching any run's registry or catalog. Only name and
//! description leave the core.
use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::namespaces::WorkspaceReply;
use workbench_protocol::namespaces::workspace::{
    MAX_SKILL_DESCRIPTION_BYTES, ProjectTarget, SkillSummary, validate_skills,
};
use workbench_protocol::queries::QueryReply;

use crate::ingress::CommandHost;

impl CommandHost {
    pub(super) fn list_skills(&self, target: &ProjectTarget) -> Result<QueryReply, WorkbenchError> {
        let root = self.project_root(&target.project)?;
        let skills: Vec<SkillSummary> = self
            .service
            .runtime()
            .list_skills_for_host(&root)
            .into_iter()
            .map(|skill| SkillSummary {
                name: skill.name,
                description: description(&skill.description),
            })
            .collect();
        // A name the wire cannot carry, or too many skills, is an error rather
        // than a list quietly missing entries.
        validate_skills(&skills)?;
        Ok(QueryReply::Workspace(WorkspaceReply::Skills(skills)))
    }
}

/// One line without anything that looks like a credential, cut at the bound.
fn description(text: &str) -> String {
    let line = text.replace(char::is_control, " ");
    let redacted = secret_guard::SecretRedactor::from_env().redact(&line).text;
    let mut end = redacted.len().min(MAX_SKILL_DESCRIPTION_BYTES);
    while !redacted.is_char_boundary(end) {
        end -= 1;
    }
    redacted[..end].to_owned()
}
