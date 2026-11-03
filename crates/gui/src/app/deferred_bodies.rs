//! The app's side of lazy bodies: wiring the resolver into frames, showing
//! unreadable events where they belong, and applying a re-read event.

use std::collections::BTreeMap;

use super::WorkbenchState;
use super::history::UnreadableEvent;
use crate::body::{BodyResolver, DeferredPartAssembler, EventFailure, EventReread, FailureReport};
use crate::model::tasks::AgentRunSource;

impl<S: AgentRunSource> WorkbenchState<S> {
    /// Reads deferred bodies through `resolver` from now on. Without one a
    /// deferred body shows as unavailable.
    #[must_use]
    pub fn with_body_resolver(mut self, resolver: BodyResolver) -> Self {
        self.body_resolver = Some(resolver);
        self
    }

    #[must_use]
    pub const fn body_resolver(&self) -> Option<&BodyResolver> {
        self.body_resolver.as_ref()
    }

    /// Permanent body failures reported so far; each body and kind of failure
    /// once.
    #[must_use]
    pub fn body_failure_reports(&self) -> &[FailureReport] {
        &self.body_failure_reports
    }

    /// Before drawing: makes the resolver reachable from panes, applies the
    /// events it read again, and reports what failed for good.
    pub(super) fn bodies_begin_frame(&mut self, ctx: &egui::Context) {
        let Some(resolver) = self.body_resolver.clone() else {
            return;
        };
        crate::body::ui::install(ctx, resolver.clone());
        for (id, outcome) in resolver.take_event_results() {
            match &outcome {
                EventReread::Ready(_) | EventReread::Gone => {
                    self.unreadable_history_events.remove(&id);
                }
                EventReread::Failed(failure) => {
                    if let Some(known) = self.unreadable_history_events.get_mut(&id) {
                        known.reason.clone_from(&failure.reason);
                    }
                }
            }
            self.transcripts.apply_reread(id, &outcome);
        }
        for report in resolver.take_failure_reports() {
            let failure = &report.failure;
            tracing::warn!(
                target: "gui::bodies",
                hash = %failure.key.hash_hex(),
                byte_len = failure.key.byte_len,
                run_id = failure.origin.run_id.as_deref().unwrap_or(""),
                event_id = failure.origin.event_id.unwrap_or(-1),
                error = %failure.error,
                "a stored body could not be read"
            );
            self.body_failure_reports.push(report);
        }
    }

    /// After drawing: reads queued for bodies no pane drew are dropped.
    pub(super) fn bodies_end_frame(&mut self) {
        if let Some(resolver) = &self.body_resolver {
            resolver.finish_frame();
        }
    }

    /// Puts a gap where a stored event could not be read. An event no run is
    /// recorded for belongs to no transcript; it is only listed.
    pub(super) fn show_unreadable_event(
        &mut self,
        failure: &EventFailure,
        unreadable: &mut BTreeMap<i64, UnreadableEvent>,
    ) {
        unreadable
            .entry(failure.id)
            .or_insert_with(|| UnreadableEvent {
                run_id: failure.run_id.clone(),
                reason: failure.reason.clone(),
            });
        self.transcripts.push_unreadable(
            failure.id,
            failure.run_id.as_deref(),
            failure.kind.clone(),
            &failure.reason,
        );
    }

    /// Hands the responses a replay left unfinished to the feed's assembler, so
    /// the slices that arrive later complete them. Their text is read now: the
    /// feed joins strings.
    pub(super) fn hand_over_open_parts(&mut self, mut parts: DeferredPartAssembler) {
        for (run_id, response_id, total, next, body) in parts.take_open() {
            let text = if body.has_deferred() {
                let Some(resolver) = &self.body_resolver else {
                    tracing::warn!(%run_id, "an unfinished response could not be carried over: no body store");
                    continue;
                };
                match resolver.read_now(&body) {
                    Ok(text) => text,
                    Err(error) => {
                        tracing::warn!(%run_id, %error, "an unfinished response could not be carried over");
                        continue;
                    }
                }
            } else {
                body.inline_or_empty().to_owned()
            };
            for index in 0..next {
                let _ = self.shared_feed.parts.accept(&event_bus::Event::new(
                    event_bus::MessageEvent::MessagePart {
                        run_id: run_id.clone(),
                        response_id: response_id.clone(),
                        text: if index == 0 {
                            text.clone()
                        } else {
                            String::new()
                        },
                        index,
                        total,
                    },
                ));
            }
        }
    }
}
