//! The workspace feature's side of the Ctrl+K thread search: it owns the request, the
//! tag and the reply of the core's `search_threads`, and keeps the [`BodySearch`] the
//! modal reads. The rules of the search itself are in [`super::body_search`].
use std::collections::BTreeSet;

use workbench_protocol::{
    namespaces::{WorkspaceQuery, WorkspaceReply},
    queries::{Query, QueryReply},
};

use super::body_search::{Scope, SearchView};
use super::{FeatureError, FeatureReply, Inflight, Outgoing, WorkspaceFeature};
use crate::core_client::cache::Cache;
use crate::core_client::thread_search::search_titles;

impl WorkspaceFeature {
    /// The search the Ctrl+K modal wants now: `None` while it is closed, has no project or
    /// a blank query. A changed search drops what was found for the old one.
    pub fn want_search(&mut self, scope: Option<Scope>) {
        self.search.want(scope);
    }

    pub fn search_view(&self) -> SearchView<'_> {
        self.search.view()
    }

    /// "Search more" in the modal.
    pub fn continue_search(&mut self) {
        self.search.continue_search();
    }

    /// "Search again" in the modal.
    pub fn research(&mut self) {
        self.search.research();
    }

    /// The reply of a request sent by [`Self::drive_search`].
    pub(super) fn on_search_reply(&mut self, epoch: u64, reply: FeatureReply) {
        let result = match reply {
            FeatureReply::Query(Ok(QueryReply::Workspace(
                WorkspaceReply::ThreadSearchResults(results),
            ))) => Ok(results),
            FeatureReply::Query(Err(error)) | FeatureReply::Command(Err(error)) => Err(error),
            _ => Err(FeatureError::new("the core answered something unexpected")),
        };
        self.search.reply(epoch, result);
    }

    /// Once per frame: settles an answer against what this client can open, and sends the
    /// request that is due. At most one is out at a time.
    pub(super) fn drive_search(&mut self, cache: &Cache) {
        if self.search.has_pending()
            && let Some(scope) = self.search.view().scope.cloned()
        {
            let listed: BTreeSet<String> =
                search_titles(cache, Some(&self.model), Some(&scope.project), &scope.query)
                    .into_iter()
                    .map(|row| row.thread.to_string())
                    .collect();
            self.search.settle(
                |thread| {
                    cache
                        .threads
                        .get(thread.as_str())
                        .is_some_and(|summary| summary.project == scope.project)
                },
                &listed,
            );
        }
        if let Some((epoch, request)) = self.search.issue() {
            let tag = self.tag(Inflight::Search(epoch));
            self.outbox.push(Outgoing::Query(
                tag,
                Query::Workspace(WorkspaceQuery::SearchThreads(request)),
            ));
        }
    }
}
