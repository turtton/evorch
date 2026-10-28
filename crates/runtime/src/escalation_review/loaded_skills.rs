//! Skills a run has read, shown to the reviewer as the expected work procedure.
//!
//! The runtime records a body only after resolving it from its own registry, so
//! nothing the agent writes (a justification, a delegated prompt) can become
//! one. A reviewer treats the bodies like project rules: they describe the
//! expected workflow and never widen authorization.

use serde::{Deserialize, Serialize};

use super::{ReviewRunContext, UserRequest, bounded};
use crate::skill::{SkillScope, split_skill_attachments};

/// Longest skill body sent to the reviewer, in bytes.
pub(crate) const MAX_SKILL_BYTES: usize = 16 * 1024;
/// Most skills sent to the reviewer; the oldest are dropped first.
pub(crate) const MAX_LOADED_SKILLS: usize = 6;

/// Who chose to bring a skill into the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LoadedBy {
    /// The agent called `skill_load`.
    SkillLoad,
    /// A parent started this run with `load_skills`.
    DelegateLoadSkills,
    /// The user attached it to their own message with an `@skill` mention.
    /// Only the reviewer's view carries this; a run never records it.
    UserMention,
}

/// A skill body the runtime read for a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoadedSkill {
    pub(crate) name: String,
    pub(crate) scope: SkillScope,
    pub(crate) loaded_by: LoadedBy,
    pub(crate) text: String,
}

/// What a restored run needs to read its skills again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadedSkillRef {
    pub name: String,
    pub(crate) loaded_by: LoadedBy,
}

/// One skill as the reviewer sees it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct LoadedSkillView {
    name: String,
    scope: &'static str,
    trust: &'static str,
    loaded_by: &'static str,
    text: String,
}

impl LoadedSkillView {
    /// Whether the body says anything.
    pub(super) fn has_text(&self) -> bool {
        !self.text.trim().is_empty()
    }
}

impl ReviewRunContext {
    /// Records `skill` as the newest one, replacing an earlier load of the same
    /// name, and drops the oldest beyond [`MAX_LOADED_SKILLS`]. The evidence
    /// stamp does not move: skills are never authorization.
    pub(crate) fn record_loaded_skill(&mut self, mut skill: LoadedSkill) {
        skill.text = bounded(&skill.text, MAX_SKILL_BYTES);
        self.loaded_skills.retain(|known| known.name != skill.name);
        self.loaded_skills.push(skill);
        let excess = self.loaded_skills.len().saturating_sub(MAX_LOADED_SKILLS);
        self.loaded_skills.drain(..excess);
    }

    pub(crate) fn loaded_skill_refs(&self) -> Vec<LoadedSkillRef> {
        self.loaded_skills
            .iter()
            .map(|skill| LoadedSkillRef {
                name: skill.name.clone(),
                loaded_by: skill.loaded_by,
            })
            .collect()
    }

    /// The skills the reviewer may see: builtin and user ones always, project
    /// ones only while the project's instructions are approved.
    pub(crate) fn reviewable_skills(&self, project_approved: bool) -> Vec<LoadedSkillView> {
        self.loaded_skills
            .iter()
            .filter_map(|skill| {
                let trust = match skill.scope {
                    SkillScope::Builtin => "builtin",
                    SkillScope::User | SkillScope::UserAgents => "user",
                    SkillScope::Repo | SkillScope::RepoAgents if project_approved => {
                        "approved-project"
                    }
                    SkillScope::Repo | SkillScope::RepoAgents => return None,
                };
                Some(LoadedSkillView {
                    name: skill.name.clone(),
                    scope: skill.scope.as_str(),
                    trust,
                    loaded_by: match skill.loaded_by {
                        LoadedBy::SkillLoad => "skill_load",
                        LoadedBy::DelegateLoadSkills => "delegate_load_skills",
                        LoadedBy::UserMention => "user_mention",
                    },
                    text: skill.text.clone(),
                })
            })
            .collect()
    }
}

/// Moves the skill bodies a host expanded from `@skill` mentions out of the
/// user's words: each request keeps its typed text, and the bodies come back
/// as views the user attached.
pub(super) fn detach_attached_skills(requests: &mut [UserRequest]) -> Vec<LoadedSkillView> {
    let mut attached = Vec::new();
    for request in requests {
        // An answer to a question is typed by the user; no host expands mentions in it.
        if request.in_reply_to.is_some() {
            continue;
        }
        let (typed, attachments) = split_skill_attachments(&request.text);
        if attachments.is_empty() {
            continue;
        }
        attached.extend(attachments.into_iter().map(|attachment| LoadedSkillView {
            name: attachment.name.to_owned(),
            scope: "user_message",
            trust: "user-attached",
            loaded_by: "user_mention",
            text: bounded(&attachment.body, MAX_SKILL_BYTES),
        }));
        request.text = typed.to_owned();
    }
    attached
}

/// `recorded` followed by `attached`, a later load replacing an earlier one of
/// the same name, newest [`MAX_LOADED_SKILLS`] kept.
pub(super) fn merge_views(
    mut recorded: Vec<LoadedSkillView>,
    attached: Vec<LoadedSkillView>,
) -> Vec<LoadedSkillView> {
    for view in attached {
        recorded.retain(|known| known.name != view.name);
        recorded.push(view);
    }
    let excess = recorded.len().saturating_sub(MAX_LOADED_SKILLS);
    recorded.drain(..excess);
    recorded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, scope: SkillScope) -> LoadedSkill {
        LoadedSkill {
            name: name.into(),
            scope,
            loaded_by: LoadedBy::SkillLoad,
            text: format!("{name} steps"),
        }
    }

    #[test]
    fn newest_six_are_kept_and_a_reload_moves_to_the_end() {
        let mut run = ReviewRunContext::empty("run-1");
        for index in 0..8 {
            run.record_loaded_skill(skill(&format!("s{index}"), SkillScope::Builtin));
        }
        run.record_loaded_skill(skill("s3", SkillScope::Builtin));
        let names: Vec<_> = run.loaded_skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["s2", "s4", "s5", "s6", "s7", "s3"]);
    }

    #[test]
    fn a_body_is_bounded_to_sixteen_kib() {
        let mut run = ReviewRunContext::empty("run-1");
        let mut big = skill("big", SkillScope::Builtin);
        big.text = "あ".repeat(MAX_SKILL_BYTES);
        run.record_loaded_skill(big);
        let text = &run.loaded_skills[0].text;
        assert!(text.ends_with("…[truncated]"));
        assert!(text.len() <= MAX_SKILL_BYTES + "…[truncated]".len());
    }

    #[test]
    fn project_scopes_are_shown_only_when_the_project_is_approved() {
        let mut run = ReviewRunContext::empty("run-1");
        for (name, scope) in [
            ("builtin", SkillScope::Builtin),
            ("user", SkillScope::User),
            ("user-agents", SkillScope::UserAgents),
            ("repo", SkillScope::Repo),
            ("repo-agents", SkillScope::RepoAgents),
        ] {
            run.record_loaded_skill(skill(name, scope));
        }
        let shown = |approved| {
            run.reviewable_skills(approved)
                .into_iter()
                .map(|view| (view.name, view.trust))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            shown(false),
            [
                ("builtin".to_owned(), "builtin"),
                ("user".to_owned(), "user"),
                ("user-agents".to_owned(), "user"),
            ]
        );
        assert_eq!(shown(true).len(), 5);
        assert_eq!(shown(true)[3], ("repo".to_owned(), "approved-project"));
    }

    #[test]
    fn recording_does_not_move_the_evidence_stamp() {
        let mut run = ReviewRunContext::empty("run-1");
        let before = run.stamp();
        run.record_loaded_skill(skill("demo", SkillScope::Builtin));
        assert_eq!(run.stamp(), before);
    }
}
