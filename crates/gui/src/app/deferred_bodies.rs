//! The app's side of lazy bodies: wiring the resolver into frames, showing
//! unreadable events where they belong, and applying a re-read event.

use std::collections::BTreeMap;

use super::WorkbenchState;
use super::history::UnreadableEvent;
use crate::body::{
    BodyError, BodyResolver, DeferredPartAssembler, EventFailure, FailureReport, HistoryRow,
    RowsPurpose,
};
use crate::model::tasks::AgentRunSource;
use crate::model::transcript::Fill;

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
        resolver.note_shown_through(self.shown_through_event);
        self.apply_rows_results();
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

    /// Applies the rows the resolver's worker has read since the last call:
    /// the earlier history of a conversation that became known, and the events
    /// that were unreadable and were read again. Nothing here reads the
    /// database; the worker did.
    pub(super) fn apply_rows_results(&mut self) {
        let Some(resolver) = self.body_resolver.clone() else {
            return;
        };
        for outcome in resolver.take_rows_results() {
            match outcome.request.purpose {
                RowsPurpose::Rehydrate { thread } => {
                    self.finish_rehydrate(&thread, &outcome.request.runs, outcome.result);
                }
                RowsPurpose::Gap { run, event_id } => {
                    self.finish_gap(&run, event_id, outcome.request.through, outcome.result);
                }
            }
        }
        // Forgotten runs must not keep the feed waiting. A read that finished
        // after we took the results still holds the feed until its next drain;
        // otherwise its history would replace newer events folded meanwhile.
        self.shared_feed
            .rehydrating
            .retain(|thread| resolver.rehydrating(thread));
    }

    /// Runs what a manual resolver has queued and applies what it read, until
    /// nothing is left to run. For headless tests, which draw no frames; a
    /// resolver with a thread of its own is only waited for, so the reads
    /// stay on its thread.
    pub fn settle_body_rows(&mut self) {
        let Some(resolver) = self.body_resolver.clone() else {
            return;
        };
        loop {
            // Asked before the results are taken: a read that is over by now
            // has left its result behind, in the same step that ended it.
            let busy = resolver.has_rows_work();
            if !resolver.has_worker() {
                resolver.pump_all();
            }
            self.apply_rows_results();
            // Events of other processes that waited for the history are shown.
            self.retry_pending_shared_events();
            if !busy && !resolver.has_rows_work() {
                break;
            }
            std::thread::yield_now();
        }
    }

    fn finish_gap(
        &mut self,
        run: &str,
        event_id: i64,
        through: i64,
        result: Result<Vec<HistoryRow>, BodyError>,
    ) {
        let rows = match result {
            Ok(rows) => rows,
            Err(error) => {
                self.transcripts
                    .update_unreadable(event_id, None, &error.reason, None);
                if let Some(known) = self.unreadable_history_events.get_mut(&event_id) {
                    known.reason = error.reason;
                }
                return;
            }
        };
        let still_unreadable = rows.iter().find_map(|row| match row {
            HistoryRow::Failure(failure) if failure.id == event_id => Some(failure),
            _ => None,
        });
        if let Some(failure) = still_unreadable {
            self.transcripts.update_unreadable(
                event_id,
                Some(&failure.kind),
                &failure.reason,
                failure.reference,
            );
            if let Some(known) = self.unreadable_history_events.get_mut(&event_id) {
                known.reason.clone_from(&failure.reason);
            }
            return;
        }
        match self
            .transcripts
            .fill_unreadable(run, event_id, &rows, through)
        {
            Fill::Stale => self.read_gap_again_for_newer_events(run, event_id),
            Fill::Filled | Fill::Absent => {
                self.gap_rereads.remove(&event_id);
                self.unreadable_history_events.remove(&event_id);
            }
        }
    }

    /// What the rows would replace changed after they were read (another window
    /// sent more of a stream meanwhile): they are not applied over it. The run
    /// is read again, now up to where the history reaches, a few times; if it
    /// keeps changing, the gap says so and waits for the user.
    fn read_gap_again_for_newer_events(&mut self, run: &str, event_id: i64) {
        let Some(resolver) = self.body_resolver.clone() else {
            return;
        };
        let rereads = self.gap_rereads.entry(event_id).or_default();
        if *rereads < crate::body::resolver::AUTO_RETRIES {
            *rereads += 1;
            resolver.request_gap(run, event_id);
            return;
        }
        self.gap_rereads.remove(&event_id);
        let reason = "The conversation kept changing while it was read again; try again";
        self.transcripts
            .update_unreadable(event_id, None, reason, None);
        if let Some(known) = self.unreadable_history_events.get_mut(&event_id) {
            known.reason = reason.to_owned();
        }
    }

    /// The earlier events of a conversation that just became known, replacing
    /// what was shown for its runs while no conversation owned them. The rows
    /// come with the places of the events that could not be read, so each gap
    /// is where it was, and a message the user sent goes in at its time.
    fn finish_rehydrate(
        &mut self,
        thread: &str,
        runs: &[String],
        result: Result<Vec<HistoryRow>, BodyError>,
    ) {
        self.shared_feed.rehydrating.remove(thread);
        let rows = match result {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, thread, "earlier events of a conversation cannot be loaded");
                return;
            }
        };
        let outer = self.transcripts.set_foreign_scope(true);
        for run in runs {
            self.transcripts.forget_run(run);
        }
        let mut messages: Vec<_> = self
            .history
            .iter()
            .filter(|message| message.thread_id == thread && message.shown_as_sent())
            .cloned()
            .collect();
        messages.sort_by_key(|message| message.at);
        let mut messages = messages.into_iter().peekable();
        let mut unreadable = BTreeMap::new();
        for row in rows {
            let at = match &row {
                HistoryRow::Event(row) => Some(row.event.meta.wall_clock),
                HistoryRow::Failure(failure) => failure.wall_clock,
            };
            if let Some(at) = at {
                while messages.peek().is_some_and(|message| message.at <= at) {
                    if let Some(message) = messages.next() {
                        self.restore_user_message(message);
                    }
                }
            }
            match row {
                HistoryRow::Failure(failure) => {
                    self.show_unreadable_event(&failure, &mut unreadable);
                }
                HistoryRow::Event(row) => {
                    self.transcripts
                        .set_source(Some((row.first_id.unwrap_or(row.id), row.id)));
                    self.apply_conversation_event_with_body(&row.event, row.body.as_ref());
                    self.transcripts.set_source(None);
                }
            }
        }
        for message in messages {
            self.restore_user_message(message);
        }
        for (id, event) in unreadable {
            self.unreadable_history_events.entry(id).or_insert(event);
        }
        self.transcripts.set_foreign_scope(outer);
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
            failure.reference,
        );
        if let (Some(resolver), Some(run)) = (&self.body_resolver, failure.run_id.as_deref()) {
            resolver.watch_gap(run, failure.id, &failure.kind);
        }
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
