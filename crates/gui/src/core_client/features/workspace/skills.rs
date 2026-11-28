//! The skills the core lists for each project (`list_skills`), for the composer's
//! `@` completion and for the skills a message names when it is sent.
//!
//! One list per project, asked for when the project is selected and again when a
//! mention starts, so a skill added meanwhile appears without a watcher. A list
//! never outlives the connection: a reset forgets all of them, and an answer to
//! a request of an earlier connection is dropped by generation. A list already
//! held stays usable while its refresh is on the way.
use super::{FeatureReply, Inflight, Outgoing, WorkspaceFeature};
use workbench_protocol::{
    ids::ProjectId,
    namespaces::{
        WorkspaceQuery, WorkspaceReply,
        workspace::{ProjectTarget, SkillSummary},
    },
    queries::{Query, QueryReply},
};

/// What the core answered about a project's skills.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectSkills {
    Ready(Vec<SkillSummary>),
    /// The core could not list them (an older core, a limit, a read failure).
    Failed,
}

impl WorkspaceFeature {
    /// What is known of `project`'s skills on this connection.
    pub fn skills(&self, project: &str) -> Option<&ProjectSkills> {
        self.skills.get(project)
    }

    /// Whether a request for `project`'s skills is on its way.
    pub fn skills_fetching(&self, project: &str) -> bool {
        self.inflight.values().any(|inflight| {
            matches!(inflight, Inflight::Skills(p, g)
                if p == project && *g == self.skills_generation)
        })
    }

    /// Asks for `project`'s skills unless they are known or on their way.
    pub(super) fn want_skills(&mut self, project: &ProjectId) {
        if !self.skills.contains_key(project.as_str()) {
            self.refresh_skills(project);
        }
    }

    /// Asks for `project`'s skills again unless a request is on its way. What is
    /// held stays in use until the answer replaces it.
    /// A list already held is still used for sending while its refresh is on the way:
    /// a send waits only on a connection that has no list yet (see `conversation::skills`).
    pub fn refresh_skills(&mut self, project: &ProjectId) {
        if self.skills_fetching(project.as_str()) {
            return;
        }
        let tag = self.tag(Inflight::Skills(
            project.to_string(),
            self.skills_generation,
        ));
        self.outbox.push(Outgoing::Query(
            tag,
            Query::Workspace(WorkspaceQuery::ListSkills(ProjectTarget {
                project: project.clone(),
            })),
        ));
    }

    /// Forgets every list: a new connection starts from nothing, and answers to
    /// requests sent before are dropped.
    pub(super) fn forget_skills(&mut self) {
        self.skills.clear();
        self.skills_generation += 1;
    }

    /// Takes the answer to a skills request. An answer of an earlier connection
    /// is dropped; any other answer than a list, and an error, leaves the project
    /// `Failed` rather than keeping a list the core no longer vouches for.
    pub(super) fn skills_answered(
        &mut self,
        project: String,
        generation: u64,
        reply: FeatureReply,
    ) {
        if generation != self.skills_generation {
            return;
        }
        let state = match reply {
            FeatureReply::Query(Ok(QueryReply::Workspace(WorkspaceReply::Skills(skills)))) => {
                ProjectSkills::Ready(skills)
            }
            FeatureReply::Query(_) | FeatureReply::Command(_) => ProjectSkills::Failed,
        };
        self.skills.insert(project, state);
    }
}
