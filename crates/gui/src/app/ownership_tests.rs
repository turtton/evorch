use super::*;
use crate::fixture::DemoSource;
use egui_kittest::{Harness, kittest::Queryable};
use runtime::ownership::{Lease, ThreadOwner};
use std::time::Duration;

fn busy(code: i32) -> RegistryError {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(code),
        Some("database is locked (probe)".into()),
    )
    .into()
}

fn snapshot(id: &str, writable: bool) -> OwnershipSnapshot {
    OwnershipSnapshot::Owned {
        owner: ThreadOwner::new(
            "thread".into(),
            Lease {
                owner_id: id.into(),
                generation: 7,
                expires_at: u64::MAX,
            },
        ),
        writable,
    }
}

fn status_harness(
    status: OwnershipStatus,
    now: Instant,
) -> Harness<'static, (OwnershipStatus, Instant)> {
    Harness::builder()
        .with_size(egui::vec2(1000.0, 300.0))
        .build_ui_state(
            |ui, (status, now)| {
                ui.ctx().global_style_mut(|style| {
                    style.interaction.tooltip_delay = 0.0;
                    style.interaction.show_tooltips_only_when_still = false;
                });
                ui.horizontal_wrapped(|ui| ownership_status_ui(ui, status, *now));
            },
            (status, now),
        )
}

#[test]
fn contention_stays_in_header_and_recovers_without_a_raw_error_label() {
    for code in [rusqlite::ffi::SQLITE_BUSY, rusqlite::ffi::SQLITE_LOCKED] {
        let now = Instant::now();
        let mut status = OwnershipStatus::default();
        status.observe("thread", Ok(snapshot("owner-full-id", true)), now);
        status.observe("thread", Err(busy(code)), now);
        let mut harness = status_harness(status, now);
        harness.run();
        harness.get_by_label("Owner owner… · generation 7 · Running ·");
        harness.get_by_label("write");
        assert!(harness.query_by_label("write ⚠").is_none());
        assert!(
            harness
                .query_by_label("database is locked (probe)")
                .is_none()
        );
        harness.state_mut().1 = now + Duration::from_secs(1);
        harness.run();
        harness.get_by_label("write ⚠");
        assert!(harness.output().shapes.iter().any(|shape| {
            matches!(&shape.shape, egui::epaint::Shape::Text(text)
                if text.galley.text() == "write ⚠"
                    && text.galley.job.sections.iter().all(|section| section.format.color == palette().WARNING_FG))
        }));
        assert!(harness.query_by_label("read-only").is_none());
        assert!(
            harness
                .query_by_label("database is locked (probe)")
                .is_none()
        );
        harness.state_mut().0.observe(
            "thread",
            Ok(snapshot("owner-full-id", true)),
            now + Duration::from_secs(1),
        );
        harness.run();
        harness.get_by_label("write");
        assert!(harness.query_by_label("write ⚠").is_none());
    }
}

#[test]
fn warning_tooltip_contains_full_id_generation_state_and_technical_detail() {
    let now = Instant::now();
    let mut status = OwnershipStatus::default();
    status.observe("thread", Ok(snapshot("owner-full-id", false)), now);
    status.observe("thread", Err(RegistryError::ReaderPoisoned), now);
    let mut harness = status_harness(status, now);
    harness.run();
    let tooltip = "Owner owner-full-id · generation 7 · Running · read-only\n操作権限の確認を再試行中...\nownership reader lock is poisoned";
    assert!(harness.query_by_label(tooltip).is_none());
    harness.get_by_label("read-only ⚠").hover();
    harness.run_steps(3);
    harness.get_by_label(tooltip);
    harness
        .get_by_label("Owner owner… · generation 7 · Running ·")
        .hover();
    harness.run_steps(3);
    harness.get_by_label(tooltip);
}

#[test]
fn startup_and_thread_switch_show_checking_not_read_only_or_previous_owner() {
    let now = Instant::now();
    let mut status = OwnershipStatus::default();
    status.observe("first", Ok(snapshot("previous-owner", true)), now);
    status.observe("first", Err(RegistryError::ReaderPoisoned), now);
    status.observe("second", Err(busy(rusqlite::ffi::SQLITE_BUSY)), now);
    let mut harness = status_harness(status, now);
    harness.run();
    harness.get_by_label("checking ownership");
    assert!(harness.query_by_label("read-only").is_none());
    assert!(
        harness
            .query_by_label("Owner previ… · generation 7 · Running ·")
            .is_none()
    );
    harness.state_mut().1 = now + Duration::from_secs(1);
    harness.run();
    harness.get_by_label("checking ownership ⚠");
}

#[test]
fn owner_ids_truncate_by_character_and_remain_inspectable_on_hover() {
    for (id, short) in [
        ("", ""),
        ("abc", "abc"),
        ("abcde", "abcde"),
        ("abcdef", "abcde…"),
        ("所有者識別", "所有者識別"),
        ("所有者識別番号", "所有者識別…"),
    ] {
        assert_eq!(short_owner_id(id), short);
        let now = Instant::now();
        let mut status = OwnershipStatus::default();
        status.observe("thread", Ok(snapshot(id, true)), now);
        let mut harness = status_harness(status, now);
        harness.run();
        harness
            .get_by_label(&format!("Owner {short} · generation 7 · Running ·"))
            .hover();
        harness.run_steps(3);
        harness.get_by_label(&format!("Owner {id} · generation 7 · Running · write"));
    }
}

fn host(dir: &tempfile::TempDir) -> Arc<OwnerHost> {
    Arc::new(
        OwnerHost::open(
            dir.path(),
            Default::default(),
            Arc::new(event_bus::EventBus::new(64)),
        )
        .unwrap(),
    )
}

fn state(host: Arc<OwnerHost>) -> WorkbenchState<DemoSource> {
    let mut state =
        WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
            .unwrap()
            .with_ownership(host);
    state.sidebar.active_thread = Some(workspace_ui::ThreadId::new("thread"));
    state
}

#[test]
fn cached_write_display_never_authorizes_after_live_ownership_is_lost() {
    let dir = tempfile::tempdir().unwrap();
    let first = host(&dir);
    let second = host(&dir);
    let permit = first.start("thread").unwrap();
    let mut state = state(first.clone());
    let now = Instant::now();
    state
        .ownership_status
        .observe("thread", probe_ownership(&first, "thread", false), now);
    assert!(state.thread_writable());
    first.handoff(&permit, &second).unwrap();
    state
        .ownership_status
        .observe("thread", Err(busy(rusqlite::ffi::SQLITE_BUSY)), now);
    assert_eq!(state.ownership_status.display(now).access, "write");
    assert!(!state.thread_writable());
    assert!(state.chat_permit("thread").is_err());
    assert!(first.owned_permit("thread").is_err());
}

#[test]
fn action_failure_is_local_and_independent_of_probe_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(&dir);
    host.start("thread").unwrap();
    let now = Instant::now();
    let mut state = state(host.clone());
    // A stale unowned display must still call the live host on Start.
    state
        .ownership_status
        .observe("thread", Ok(OwnershipSnapshot::Unowned), now);
    state
        .ownership_status
        .observe("thread", Err(busy(rusqlite::ffi::SQLITE_BUSY)), now);
    let mut harness = Harness::builder()
        .with_size(egui::vec2(1400.0, 300.0))
        .build_ui_state(
            move |ui, state: &mut WorkbenchState<DemoSource>| {
                state.ownership_controls_ui(ui, &host, "thread", now)
            },
            state,
        );
    harness.run();
    harness.get_by_label("Start").click();
    harness.run();
    let error = "Start: thread already exists; attach or claim explicitly";
    let error_rect = harness.get_by_label(error).rect();
    let button = harness.get_by_label("Start").rect();
    assert!((error_rect.center().y - button.center().y).abs() < button.height());
    assert!(harness.state().ownership_status.failure.is_some());
    harness
        .state_mut()
        .ownership_status
        .observe("thread", Ok(OwnershipSnapshot::Unowned), now);
    harness.run();
    harness.get_by_label(error);
    assert!(harness.state().ownership_status.failure.is_none());
}

#[test]
fn failed_attach_does_not_switch_to_read_only_and_success_clears_action_error() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(&dir);
    let now = Instant::now();
    let mut state = state(host.clone());
    state
        .ownership_status
        .observe("thread", Ok(snapshot("stale-owner", true)), now);
    let controls_host = host.clone();
    let mut harness = Harness::builder().build_ui_state(
        move |ui, state: &mut WorkbenchState<DemoSource>| {
            state.ownership_controls_ui(ui, &controls_host, "thread", now)
        },
        state,
    );
    harness.run();
    harness.get_by_label("Attach (read-only)").click();
    harness.run();
    harness.get_by_label("Attach (read-only): thread does not exist");
    assert!(!harness.state().readonly_threads.contains("thread"));
    host.start("thread").unwrap();
    harness.get_by_label("Attach (read-only)").click();
    harness.run();
    assert!(harness.state().readonly_threads.contains("thread"));
    assert!(harness.state().ownership_action_error.is_none());
    assert!(!harness.state().thread_writable());
}

#[test]
fn shutdown_failure_is_in_the_close_dialog_not_the_header() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(&dir);
    let permit = host.start("thread").unwrap();
    permit.begin_turn().unwrap();
    let mut state = state(host.clone());
    state.shutdown_requested = true;
    let mut harness = Harness::builder()
        .with_size(egui::vec2(1200.0, 600.0))
        .build_ui_state(
            |ui, state: &mut WorkbenchState<DemoSource>| state.ownership_ui(ui),
            state,
        );
    harness.run_steps(4);
    // Existing readers keep working; quiesce cannot open its live writer.
    std::fs::rename(dir.path().join("owners.db"), dir.path().join("moved.db")).unwrap();
    harness.get_by_label("Drain and close").click();
    harness.run_steps(4);
    let error = harness.state().ownership_action_error.as_ref().unwrap();
    assert_eq!(error.operation, SHUTDOWN);
    let label = format!("Shutdown: {}", error.detail);
    let dialog = harness.get_by_label("Active thread ownership").rect();
    assert!(dialog.contains_rect(harness.get_by_label(&label).rect()));
    assert!(harness.state().ownership_status.failure.is_none());
    harness.get_by_label("Keep open").click();
    harness.run_steps(4);
    assert!(harness.state().ownership_action_error.is_none());
    assert!(!harness.state().shutdown_requested);
}
