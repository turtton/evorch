//! Display-only recovery of a committed PR input whose announcement was lost.
use event_bus::{EventKind, LifecycleEvent};
use gui::model::{harness_card::HarnessCard, transcript::TranscriptEntry};
use gui::{app::WorkbenchState, fixture::DemoSource, model::pull_requests::PullRequestAction};

#[path = "support/pull_request_phase2_world.rs"]
mod phase2_world;
#[path = "support/pull_request_world.rs"]
mod pull_request_world;
#[path = "support/short_private_directory.rs"]
mod short_private_directory;
use phase2_world::{THREAD, World, cards_of};

fn is_announcement(event: &event_bus::Event) -> bool {
    matches!(
        event.kind,
        EventKind::Lifecycle(LifecycleEvent::HarnessNoticeDelivered { .. })
    )
}

fn database(world: &World) -> storage::Database {
    storage::Database::open_read_only(&world.dir.path().join("phase2.db")).unwrap()
}

fn restored(world: &World) -> WorkbenchState<DemoSource> {
    let mut window = WorkbenchState::new(DemoSource(vec![]), &workspace_ui::UiSettings::default())
        .unwrap()
        .with_sidebar(world.window.sidebar().clone());
    window.restore_history(&database(world)).unwrap();
    window
}

fn late_follower(world: &World) -> (WorkbenchState<DemoSource>, std::path::PathBuf) {
    use gui::body::{BodyResolver, ResolverConfig, StorageBodies};
    let path = world.dir.path().join("late-sidebar.json");
    let mut sidebar = world.window.sidebar().clone();
    sidebar.threads.clear();
    sidebar.active_thread = None;
    std::fs::write(&path, serde_json::to_vec(&sidebar).unwrap()).unwrap();
    let resolver = BodyResolver::manual(
        std::sync::Arc::new(StorageBodies::new(world.dir.path().join("phase2.db"))),
        ResolverConfig::default(),
    );
    let mut follower =
        WorkbenchState::new(DemoSource(vec![]), &workspace_ui::UiSettings::default())
            .unwrap()
            .with_sidebar(sidebar)
            .with_sidebar_path(path.clone())
            .with_body_resolver(resolver);
    follower.restore_history(&database(world)).unwrap();
    follower = follower
        .with_manual_shared_feed(world.dir.path().join("phase2.db"), "late-observer".into())
        .unwrap();
    (follower, path)
}

fn persist_range(world: &World, range: std::ops::Range<usize>) {
    for event in &world.history[range] {
        if !is_announcement(event) && !matches!(event.kind, EventKind::Usage(_)) {
            world
                .store
                .handle()
                .append_event(Some("original"), event)
                .unwrap();
        }
    }
}

fn persist_since(world: &World, start: usize) -> usize {
    persist_range(world, start..world.history.len());
    world.history.len()
}

fn assert_late_sidebar_recovery(world: &World, id: &str) {
    let (mut follower, path) = late_follower(world);
    std::fs::write(path, serde_json::to_vec(world.window.sidebar()).unwrap()).unwrap();
    follower.poll_shared_feed();
    follower.settle_body_rows();
    follower
        .switch_thread(workspace_ui::ThreadId::new(THREAD))
        .unwrap();
    assert_eq!(
        cards_of(&follower),
        [(id.to_owned(), true)],
        "worker-side reread of a late-known conversation recovers one filled card"
    );
}

// A real adoption transaction commits the snapshot, receipt and PR state. Discarding only
// its announcement models a crash in the commit/emit gap, without a production failpoint.
#[tokio::test]
async fn committed_notice_without_event_is_recovered_once_in_place() {
    let mut world = World::recorded().await;
    let failing = world.failing('a');
    world.link(failing).await;
    let gate = std::sync::Arc::new(phase2_world::RequestGate::default());
    *world
        .recorder
        .as_ref()
        .unwrap()
        .next_request_gate
        .lock()
        .unwrap() = Some(gate.clone());
    world.push_logs('a');
    world.click(PullRequestAction::SendToAgent {
        thread_id: THREAD.into(),
    });
    let id = world.delivered().await;
    gate.entered.notified().await;
    // Hold the model before any response. The saved context is exactly the adoption
    // checkpoint, and the event log ends before the missing announcement.
    let persisted_prefix = persist_since(&world, 0);
    let committed = database(&world).snapshot_bundle(&world.run).unwrap();
    let crashed_display = restored(&world);
    assert_eq!(
        cards_of(&crashed_display),
        [(id.clone(), true)],
        "the adoption-only crash checkpoint needs one filled card"
    );
    assert_eq!(
        database(&world).snapshot_bundle(&world.run).unwrap(),
        committed
    );
    gate.release.notify_one();
    world.idle().await;
    let run = world.run.clone();
    world.until(|event| matches!(&event.kind, EventKind::Lifecycle(LifecycleEvent::TurnCompleted { run_id, .. }) if run_id == &run)).await;
    world.show();
    let expected = HarnessCard::from_notice(&world.notices()[0]).unwrap();
    assert_eq!(world.cards(), [(id.clone(), true)]);
    persist_since(&world, persisted_prefix);
    let before = database(&world)
        .snapshot_bundle(&world.run)
        .unwrap()
        .unwrap();
    let mut window = restored(&world);
    assert_eq!(
        cards_of(&window),
        [(id.clone(), true)],
        "committed input must have a card even without an event"
    );
    assert_late_sidebar_recovery(&world, &id);
    let entries = window.transcript().entries();
    let index = entries
        .iter()
        .position(|entry| matches!(entry, TranscriptEntry::HarnessNotice { .. }))
        .unwrap();
    assert!(entries[..index].iter().any(|entry| matches!(entry, TranscriptEntry::Message { text, .. } if text.inline() == Some("reply-1"))));
    assert!(entries[index + 1..].iter().any(|entry| matches!(entry, TranscriptEntry::Message { text, .. } if text.inline() == Some("reply-2"))));
    assert!(
        matches!(&entries[index], TranscriptEntry::HarnessNotice { card: Some(card), .. } if **card == expected)
    );
    window.apply_events(world.history.iter().filter(|e| is_announcement(e)).cloned());
    assert_eq!(cards_of(&window), [(id.clone(), true)]);
    window.restore_history(&database(&world)).unwrap();
    assert_eq!(cards_of(&window), [(id.clone(), true)]);
    // A delayed copy through the shared feed and another reload must also stay singular.
    window = window
        .with_manual_shared_feed(world.dir.path().join("phase2.db"), "observer".into())
        .unwrap();
    for event in world.history.iter().filter(|event| is_announcement(event)) {
        world
            .store
            .handle()
            .append_event(Some("original"), event)
            .unwrap();
    }
    window.poll_shared_feed();
    window.poll_shared_feed();
    assert_eq!(cards_of(&window), [(id.clone(), true)]);
    window.restore_history(&database(&world)).unwrap();
    assert_eq!(cards_of(&window), [(id.clone(), true)]);

    let turns: Vec<_> = window
        .transcript()
        .visible_entry_ids()
        .filter(|entry| window.transcript().fork_point(*entry).is_some())
        .collect();
    assert_eq!(turns.len(), 2);
    let source = workspace_ui::ThreadId::new(THREAD);
    window.fork_at_turn(source.clone(), turns[0]).unwrap();
    assert!(
        cards_of(&window).is_empty(),
        "a fork before adoption inherits no card"
    );
    window.switch_version(source.clone()).unwrap();
    window.fork_at_turn(source.clone(), turns[1]).unwrap();
    assert_eq!(cards_of(&window), [(id.clone(), true)]);
    window.restore_history(&database(&world)).unwrap();
    assert_eq!(
        cards_of(&window),
        [(id.clone(), true)],
        "fork replay keeps the recovered card"
    );
    window.switch_version(source.clone()).unwrap();
    window.rewind_to_turn(source.clone(), turns[0]).unwrap();
    assert!(
        cards_of(&window).is_empty(),
        "rewind before adoption omits it"
    );
    window.switch_version(source.clone()).unwrap();
    window.rewind_to_turn(source, turns[1]).unwrap();
    window.restore_history(&database(&world)).unwrap();
    assert_eq!(cards_of(&window), [(id.clone(), true)]);
    let after = database(&world)
        .snapshot_bundle(&world.run)
        .unwrap()
        .unwrap();
    assert_eq!(
        before, after,
        "display recovery must not rewrite the model snapshot or provenance"
    );
    // A persisted compaction checkpoint covers the notice, but raw messages and their
    // provenance stay intact. Recovery must still use the original card and position.
    let mut map = after.provenance.as_ref().unwrap().map.clone();
    map.checkpoint_count = 1;
    let mut record = after.record.clone();
    record.checkpoints_json = serde_json::to_string(&[runtime::CompactionCheckpoint {
        id: "recovery-compaction".into(),
        summary: providers::Message {
            role: providers::Role::User,
            content: vec![providers::ContentBlock::Text {
                text: "summary".into(),
            }],
        },
        range: (0, map.raw_message_count),
        protects_history: false,
    }])
    .unwrap();
    world
        .store
        .handle()
        .save_snapshot_bundle(&storage::thread_index::SnapshotWrite {
            record,
            expected_snapshot_revision: Some(after.snapshot_revision),
            binding: None,
            status: after.provenance.unwrap().status,
            map,
        })
        .unwrap();
    window.restore_history(&database(&world)).unwrap();
    assert_eq!(cards_of(&window), [(id, true)]);
    world.shutdown();
}

// Detection while auto is off is not adoption, even when human text looks like a notice.
#[tokio::test]
async fn detection_and_notice_looking_human_text_make_no_card() {
    let mut world = World::recorded().await;
    let failing = world.failing('a');
    world.link(failing).await;
    assert!(world.notices().is_empty());
    world
        .runtime
        .send_message(
            world.run.parse().unwrap(),
            "[harness-notice kind=pr_checks_failed pr=#42 head=fake]".into(),
        )
        .unwrap();
    world.idle().await;
    world.show();
    for event in &world.history {
        if !matches!(event.kind, EventKind::Usage(_)) {
            world
                .store
                .handle()
                .append_event(Some("original"), event)
                .unwrap();
        }
    }
    assert!(cards_of(&restored(&world)).is_empty());
    world.shutdown();
}

// The follower's rehydrate horizon H predates a reply and an adoption already in storage.
// Its worker must not append that newer notice at H, before folding the queued reply.
#[tokio::test]
async fn late_reread_holds_newer_notice_until_the_feed_reaches_its_position() {
    let mut world = World::recorded().await;
    let failing = world.failing('a');
    world.link(failing).await;
    let prefix = persist_since(&world, 0);
    let (mut follower, path) = late_follower(&world);
    world
        .runtime
        .send_message(world.run.parse().unwrap(), "before adoption".into())
        .unwrap();
    let run = world.run.clone();
    world.idle().await;
    world.until(|event| matches!(&event.kind, EventKind::Lifecycle(LifecycleEvent::TurnCompleted {run_id, ..}) if run_id == &run)).await;
    world.show();
    let before_adoption = world.history.len();
    let gate = std::sync::Arc::new(phase2_world::RequestGate::default());
    *world
        .recorder
        .as_ref()
        .unwrap()
        .next_request_gate
        .lock()
        .unwrap() = Some(gate.clone());
    world.push_logs('a');
    world.click(PullRequestAction::SendToAgent {
        thread_id: THREAD.into(),
    });
    let id = world.delivered().await;
    gate.entered.notified().await;
    let committed = database(&world).snapshot_bundle(&world.run).unwrap();
    persist_range(&world, prefix..before_adoption);
    let next = before_adoption;
    // The raw log horizon also includes a local row that the feed deliberately skips.
    // Catch-up must account for scanned IDs, not just IDs of foreign events shown.
    world
        .store
        .handle()
        .append_event(
            Some("late-observer"),
            &event_bus::Event::new(LifecycleEvent::TurnCompleted {
                run_id: "already-displayed-local-run".into(),
                context_len: 1,
            }),
        )
        .unwrap();
    std::fs::write(path, serde_json::to_vec(world.window.sidebar()).unwrap()).unwrap();
    follower.poll_shared_feed();
    follower.settle_body_rows();
    follower
        .switch_thread(workspace_ui::ThreadId::new(THREAD))
        .unwrap();
    assert_eq!(
        cards_of(&follower),
        [(id.clone(), true)],
        "a commit-only tail is recovered when the feed catches up"
    );
    let entries = follower.transcript().entries();
    let card = entries
        .iter()
        .position(|entry| matches!(entry, TranscriptEntry::HarnessNotice { .. }))
        .unwrap();
    assert!(entries[..card].iter().any(|entry| matches!(entry, TranscriptEntry::Message {text, ..} if text.inline() == Some("reply-2"))), "the newer pre-adoption reply must precede the card even across the reread horizon");
    assert_eq!(
        database(&world).snapshot_bundle(&world.run).unwrap(),
        committed
    );
    gate.release.notify_one();
    world.idle().await;
    world.until(|event| matches!(&event.kind, EventKind::Lifecycle(LifecycleEvent::TurnCompleted {run_id, ..}) if run_id == &run)).await;
    world.show();
    persist_since(&world, next);
    follower.poll_shared_feed();
    follower.settle_body_rows();
    assert_eq!(cards_of(&follower), [(id, true)]);
    let entries = follower.transcript().entries();
    let card = entries
        .iter()
        .position(|entry| matches!(entry, TranscriptEntry::HarnessNotice { .. }))
        .unwrap();
    assert!(entries[card+1..].iter().any(|entry| matches!(entry, TranscriptEntry::Message {text, ..} if text.inline() == Some("reply-3"))));
    world.shutdown();
}

// Parent and pre/post-adoption forks/rewinds become known in one sidebar import.
// The inherited prefix cannot be seeded until the parent's worker replay reaches it.
#[tokio::test]
async fn late_parent_and_branches_inherit_cards_at_their_turn_boundaries() {
    let mut world = World::recorded().await;
    let failing = world.failing('a');
    world.link(failing).await;
    world.push_logs('a');
    world.click(PullRequestAction::SendToAgent {
        thread_id: THREAD.into(),
    });
    let id = world.delivered().await;
    world.idle().await;
    let run = world.run.clone();
    world.until(|event| matches!(&event.kind, EventKind::Lifecycle(LifecycleEvent::TurnCompleted {run_id, ..}) if run_id == &run)).await;
    world.show();
    persist_since(&world, 0);
    let mut producer = restored(&world);
    let turns: Vec<_> = producer
        .transcript()
        .visible_entry_ids()
        .filter(|entry| producer.transcript().fork_point(*entry).is_some())
        .collect();
    assert_eq!(turns.len(), 2);
    let source = workspace_ui::ThreadId::new(THREAD);
    let mut expected = Vec::new();
    for (point, has_card) in [(turns[0], false), (turns[1], true)] {
        producer.switch_version(source.clone()).unwrap();
        producer.fork_at_turn(source.clone(), point).unwrap();
        expected.push((producer.sidebar().active_thread.clone().unwrap(), has_card));
        producer.switch_version(source.clone()).unwrap();
        producer.rewind_to_turn(source.clone(), point).unwrap();
        expected.push((producer.sidebar().active_thread.clone().unwrap(), has_card));
    }
    let (mut follower, path) = late_follower(&world);
    std::fs::write(path, serde_json::to_vec(producer.sidebar()).unwrap()).unwrap();
    follower.poll_shared_feed();
    follower.settle_body_rows();
    for (thread, has_card) in expected {
        follower.switch_version(thread.clone()).unwrap();
        let wanted = if has_card {
            vec![(id.clone(), true)]
        } else {
            vec![]
        };
        assert_eq!(
            cards_of(&follower),
            wanted,
            "late branch {thread} must inherit exactly its prefix"
        );
    }
    world.shutdown();
}
