//! A host message with the skills its `@name` mentions attached.
//!
//! The host sends what the user typed together with the names of the skills
//! they mentioned. The runtime reads those bodies once, appends them to the
//! prompt the agent sees, and keeps the three apart from then on: the typed
//! words (review evidence, receipts, goals), the prompt (what the agent is
//! sent), and the attachments (which skill the user brought, from where).

use std::path::Path;

use crate::skill::{SkillRegistry, append_skill_attachment};
use crate::{AgentRuntime, RuntimeError};

/// Longest skill body the runtime attaches to one message, in bytes.
pub(crate) const MAX_ATTACHMENT_BYTES: usize = 256 * 1024;

/// What a host sent, split by origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostPrompt {
    /// The user's own words.
    pub(crate) typed: String,
    /// What the agent receives: `typed` followed by one block per attachment.
    pub(crate) prompt: String,
    /// The skills the runtime read for the user's mentions.
    pub(crate) attachments: Vec<storage::EvidenceAttachment>,
}

impl HostPrompt {
    /// A message without attached skills.
    pub(crate) fn plain(text: String) -> Self {
        Self {
            prompt: text.clone(),
            typed: text,
            attachments: Vec::new(),
        }
    }

    /// Appends the body of each mentioned skill to `typed`, once per name in
    /// first-seen order.
    ///
    /// # Errors
    /// [`RuntimeError::SkillAttachment`] naming the first skill that is
    /// unavailable or whose body cannot be read or stored. Nothing is attached
    /// then, so the caller can refuse the message whole.
    pub(crate) fn expand(
        typed: String,
        mentions: &[String],
        registry: Option<&SkillRegistry>,
    ) -> Result<Self, RuntimeError> {
        let mut expanded = Self::plain(typed);
        for name in mentions {
            if expanded.attachments.iter().any(|known| known.name == *name) {
                continue;
            }
            let fail = |reason: &str| RuntimeError::SkillAttachment {
                name: name.clone(),
                reason: reason.into(),
            };
            let registry = registry.ok_or_else(|| fail("no skills are available"))?;
            let entry = registry
                .get(name)
                .ok_or_else(|| fail("the skill is not available here"))?;
            let body = registry
                .load_body(name)
                .map_err(|error| fail(&error.to_string()))?;
            let body = body.trim_end();
            if body.len() > MAX_ATTACHMENT_BYTES {
                return Err(fail("the skill body is too large to attach"));
            }
            append_skill_attachment(&mut expanded.prompt, name, body);
            expanded.attachments.push(storage::EvidenceAttachment {
                name: name.clone(),
                scope: entry.scope.as_str().into(),
                body: body.into(),
            });
        }
        Ok(expanded)
    }
}

impl AgentRuntime {
    /// Expands `mentions` for a message in `project_root` (or the active
    /// project). With no mentions nothing is read.
    pub(crate) fn expand_host_prompt(
        &self,
        typed: String,
        mentions: &[String],
        project_root: Option<&Path>,
    ) -> Result<HostPrompt, RuntimeError> {
        if mentions.is_empty() {
            return Ok(HostPrompt::plain(typed));
        }
        let active = self.shared.active_project_root();
        let project = project_root
            .map(Path::to_path_buf)
            .or(active)
            .map(|root| self.shared.project(&root));
        let registry = match self.shared.skill_source.get() {
            Some(source) => Some(source.registry_for(
                project.as_ref().map(|project| project.root.as_path()),
                project.as_ref().is_none_or(|project| project.repo_skills),
            )),
            None => self.shared.skills.get().cloned(),
        };
        HostPrompt::expand(typed, mentions, registry.as_deref())
    }
}
