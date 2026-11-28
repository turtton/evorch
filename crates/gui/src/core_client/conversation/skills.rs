//! Which skills a message names when it is sent.
//!
//! The composer never reads a skill: it sends the names of the skills the
//! message mentions with `@name`, and the core reads their bodies. A name counts
//! when the core's list for the project has it, whatever else the project holds
//! under that name. Without that list the message cannot be told apart from one
//! that mentions only files, so it waits for it, or goes without skills when the
//! core could not list them.
use workbench_protocol::limits::MAX_SKILLS;

use crate::composer_input::mentioned_names;
use crate::core_client::features::workspace::ProjectSkills;

/// What to do with a message the user sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SkillSend {
    /// Send, naming these skills (once each, in the order first written).
    Send(Vec<String>),
    /// The list is not here yet: keep the draft and send nothing.
    Wait,
    /// The core could not list the skills: send without any, and say so.
    Unlisted,
    /// More skills than one message may name: keep the draft and send nothing.
    TooMany,
}

/// `state` is the project's list as known on this connection and `fetching`
/// whether a request for it is on its way.
pub(super) fn resolve(text: &str, state: Option<&ProjectSkills>, fetching: bool) -> SkillSend {
    let written = mentioned_names(text);
    if written.is_empty() {
        return SkillSend::Send(Vec::new());
    }
    match state {
        Some(ProjectSkills::Ready(listed)) => {
            let named: Vec<String> = written
                .into_iter()
                .filter(|name| listed.iter().any(|skill| skill.name == *name))
                .map(str::to_owned)
                .collect();
            if named.len() > MAX_SKILLS {
                SkillSend::TooMany
            } else {
                SkillSend::Send(named)
            }
        }
        Some(ProjectSkills::Failed) if !fetching => SkillSend::Unlisted,
        _ => SkillSend::Wait,
    }
}

#[cfg(test)]
mod tests {
    use workbench_protocol::namespaces::workspace::SkillSummary;

    use super::*;

    fn ready(names: &[&str]) -> ProjectSkills {
        ProjectSkills::Ready(
            names
                .iter()
                .map(|name| SkillSummary {
                    name: (*name).into(),
                    description: String::new(),
                })
                .collect(),
        )
    }

    fn sent(text: &str, state: &ProjectSkills) -> Vec<String> {
        match resolve(text, Some(state), false) {
            SkillSend::Send(names) => names,
            other => panic!("{other:?}"),
        }
    }

    // Given: a message with no `@name`
    // Then: it is sent at once, whatever is known of the list
    #[test]
    fn a_message_without_mentions_never_waits() {
        for (state, fetching) in [
            (None, true),
            (None, false),
            (Some(ProjectSkills::Failed), true),
        ] {
            assert_eq!(
                resolve("no mention, e-mail a@b", state.as_ref(), fetching),
                SkillSend::Send(Vec::new())
            );
        }
    }

    // Given: a list with two skills
    // When: the message mentions them with marks, twice, and a file
    // Then: each listed skill is named once in the order written; the file is not
    #[test]
    fn listed_names_are_named_once_in_first_written_order() {
        let state = ready(&["review", "deploy"]);
        assert_eq!(
            sent(
                "@deploy, then @review! @src/lib.rs @deploy and @unknown",
                &state
            ),
            ["deploy", "review"]
        );
    }

    // Given: a listed skill
    // Then: a mention of it is a skill even beside a project path of that name; the
    //       composer never looks at the project's files
    #[test]
    fn a_listed_name_is_a_skill_whatever_the_project_holds() {
        assert_eq!(sent("see @docs", &ready(&["docs"])), ["docs"]);
    }

    // Given: no list yet, or a refresh of a failed one on its way
    // Then: the message waits
    #[test]
    fn a_mention_waits_for_a_list_that_is_not_here() {
        assert_eq!(resolve("@review", None, false), SkillSend::Wait);
        assert_eq!(resolve("@review", None, true), SkillSend::Wait);
        assert_eq!(
            resolve("@review", Some(&ProjectSkills::Failed), true),
            SkillSend::Wait
        );
    }

    // Given: a list the core could not give
    // Then: the message goes without skills
    #[test]
    fn a_failed_list_sends_without_skills() {
        assert_eq!(
            resolve("@review", Some(&ProjectSkills::Failed), false),
            SkillSend::Unlisted
        );
    }

    // Given: a refresh on its way while a list is held
    // Then: the held list is used
    #[test]
    fn a_refresh_in_flight_does_not_hold_back_a_held_list() {
        assert_eq!(
            resolve("@review", Some(&ready(&["review"])), true),
            SkillSend::Send(vec!["review".into()])
        );
    }

    // Given: 32 and 33 listed skills mentioned
    // Then: 32 are named, 33 are refused
    #[test]
    fn at_most_the_wire_limit_of_skills_is_named() {
        let names: Vec<String> = (0..=MAX_SKILLS).map(|i| format!("skill-{i}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let state = ready(&refs);
        let text = |count: usize| {
            names[..count]
                .iter()
                .map(|name| format!("@{name}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert_eq!(sent(&text(MAX_SKILLS), &state).len(), MAX_SKILLS);
        assert_eq!(
            resolve(&text(MAX_SKILLS + 1), Some(&state), false),
            SkillSend::TooMany
        );
    }
}
