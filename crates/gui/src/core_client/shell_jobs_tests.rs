use super::*;
use egui_kittest::{Harness, kittest::Queryable};
fn job(run: &str, uid: &str, state: ShellJobState) -> ShellJobView {
    ShellJobView {
        thread: ThreadId::new("thread").unwrap(),
        run: RunId::new(run).unwrap(),
        job_uid: uid.into(),
        handle: "job-1".into(),
        source_call_id: Some("start".into()),
        command: Some("cargo check".into()),
        state,
        exit_code: (state == ShellJobState::Completed).then_some(0),
    }
}
fn output(run: &str, uid: &str, offset: u64, chunk: &str) -> ExecutionEvent {
    ExecutionEvent::ShellJobOutput {
        thread: ThreadId::new("thread").unwrap(),
        run: RunId::new(run).unwrap(),
        job_uid: uid.into(),
        offset,
        chunk: chunk.into(),
    }
}
fn reply(run: &str, uid: &str, chunk: &str) -> ShellOutput {
    ShellOutput {
        thread: ThreadId::new("thread").unwrap(),
        run: RunId::new(run).unwrap(),
        job_uid: uid.into(),
        available_from: 0,
        offset: 0,
        chunk: chunk.into(),
        output_end: chunk.len() as u64,
    }
}
#[test]
fn tail_deduplicates_overlaps_marks_gaps_and_caps_utf8_bytes() {
    let mut tail = Tail::default();
    for (offset, chunk) in [(0, "abc"), (0, "abc"), (1, "bcde")] {
        tail.append(offset, chunk);
    }
    assert_eq!(tail.text, "abcde");
    assert!(!tail.gap);
    tail.append(10, "界z");
    assert_eq!(tail.text, "界z");
    assert!(tail.gap);
    let huge = "界".repeat(MAX_SHELL_TAIL_BYTES);
    tail.append(14, &huge);
    assert!(tail.text.len() <= MAX_SHELL_TAIL_BYTES);
    assert_eq!(tail.offset + tail.text.len() as u64, tail.end);
    let text = tail.text.clone();
    tail.append(u64::MAX, "bad");
    assert_eq!(tail.text, text);
}
#[test]
fn selection_reads_once_and_stale_reply_cannot_cross_run_or_session() {
    let mut catalog = JobCatalog::default();
    catalog.insert(job("run", "uid", ShellJobState::Running));
    catalog.insert(job("other", "uid", ShellJobState::Running));
    let thread = ThreadId::new("thread").unwrap();
    let mut panel = ShellJobsPanel::default();
    assert!(panel.request(Some(&thread), &catalog).is_none());
    panel.select((RunId::new("run").unwrap(), "uid".into()));
    let (generation, q) = panel.request(Some(&thread), &catalog).unwrap();
    assert_eq!(q.run.as_str(), "run");
    assert!(panel.request(Some(&thread), &catalog).is_none());
    panel.event(&output("other", "uid", 0, "wrong run"));
    assert!(panel.tail.text.is_empty());
    panel.event(&output("run", "uid", 3, "def"));
    panel.reply(generation, Ok(reply("run", "uid", "abcde")));
    assert_eq!(panel.tail.text, "abcdef"); // delayed read plus newer stream
    panel.reset();
    panel.reply(generation, Ok(reply("run", "uid", "stale")));
    assert!(panel.tail.text.is_empty());
    assert!(panel.request(Some(&thread), &catalog).is_some());
    panel.request(Some(&ThreadId::new("another-thread").unwrap()), &catalog);
    assert!(panel.selected.is_none());
    assert!(panel.tail.text.is_empty());
}
#[test]
fn terminal_catalog_uses_the_shared_count_bound_without_evicting_active_jobs() {
    let mut catalog = JobCatalog::default();
    catalog.insert(job("run", "active", ShellJobState::Running));
    for n in 0..MAX_RETAINED_SHELL_JOBS + 5 {
        catalog.insert(job("run", &format!("uid-{n}"), ShellJobState::Completed));
    }
    assert_eq!(catalog.jobs.len(), MAX_RETAINED_SHELL_JOBS + 1);
    assert!(
        catalog
            .jobs
            .contains_key(&(RunId::new("run").unwrap(), "active".into()))
    );
    assert!(
        !catalog
            .jobs
            .contains_key(&(RunId::new("run").unwrap(), "uid-0".into()))
    );
}
#[test]
fn panel_shows_command_state_exit_code_and_selected_live_output() {
    let thread = ThreadId::new("thread").unwrap();
    let mut catalog = JobCatalog::default();
    catalog.insert(job("run", "uid", ShellJobState::Completed));
    let mut harness = Harness::builder()
        .with_size(egui::vec2(800.0, 500.0))
        .build_ui_state(
            move |ui, panel: &mut ShellJobsPanel| panel.render(ui, Some(&thread), &catalog),
            ShellJobsPanel::default(),
        );
    harness
        .get_by_label("job-1 · Completed · exit 0 · cargo check")
        .click();
    harness.run();
    harness
        .state_mut()
        .event(&output("run", "uid", 0, "hello from shell"));
    harness.run();
    harness.get_by_label("hello from shell");
}

#[test]
fn snapshot_then_changed_event_update_the_shell_catalog_once() {
    use crate::core_client::cache::Cache;
    use workbench_protocol::{
        events::{EventCursor, EventEnvelope, SnapshotItem, UiEvent},
        ids::SubscriptionId,
        wire::{InstanceEpoch, Sequence},
    };
    let mut cache = Cache::default();
    cache.snapshot(SnapshotItem::ShellJob(job(
        "run",
        "uid",
        ShellJobState::Running,
    )));
    cache.at(EventCursor {
        core_epoch: InstanceEpoch(1),
        sequence: Sequence(0),
    });
    let event = EventEnvelope {
        subscription_id: SubscriptionId::new("sub").unwrap(),
        cursor: EventCursor {
            core_epoch: InstanceEpoch(1),
            sequence: Sequence(1),
        },
        caused_by_request_id: None,
        event: UiEvent::Execution(ExecutionEvent::ShellJobChanged {
            job: job("run", "uid", ShellJobState::Completed),
        }),
    };
    assert!(cache.event(event.clone()));
    assert!(cache.event(event));
    assert_eq!(cache.shell_jobs.jobs.len(), 1);
    let projected = cache.shell_jobs.jobs.values().next().unwrap();
    assert_eq!(projected.state, ShellJobState::Completed);
    assert_eq!(projected.exit_code, Some(0));
}
