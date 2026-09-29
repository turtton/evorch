use std::sync::Arc;

use gui::{app::WorkbenchState, fixture::DemoSource, headless::HeadlessWorkbench};
use runtime::ownership::OwnerHost;
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

fn state(
    owner: Option<Arc<OwnerHost>>,
    project_root: &std::path::Path,
) -> WorkbenchState<DemoSource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "project", project_root)
        .unwrap();
    sidebar.select_project(&project).unwrap();
    let thread = ThreadId::new("thread");
    sidebar
        .create_thread(thread.clone(), project, "Header controls")
        .unwrap();
    sidebar.switch_thread(&thread).unwrap();
    let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar);
    match owner {
        Some(owner) => state.with_ownership(owner),
        None => state,
    }
}

#[test]
fn diagnostics_info_button_sits_before_thread_title_and_opens_dialog() {
    let dir = tempfile::tempdir().unwrap();
    let mut workbench = HeadlessWorkbench::new(state(None, dir.path()), [1280.0, 720.0]);
    workbench.run();

    let info = workbench.label_rects("ℹ")[0];
    let title = workbench.label_rects("Thread: Header controls")[0];
    assert!(
        info.right() < title.left(),
        "info={info:?}, title={title:?}"
    );
    assert!(
        (info.center().y - title.center().y).abs() < title.height(),
        "info={info:?}, title={title:?}"
    );
    workbench.click_label("ℹ");
    workbench.run();
    assert!(workbench.has_label("実行の診断"));
}

#[test]
fn owner_controls_stay_above_conversation_while_settings_and_quota_stay_in_footer() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(
        OwnerHost::open(
            dir.path(),
            Default::default(),
            Arc::new(event_bus::EventBus::new(64)),
        )
        .unwrap(),
    );
    host.start("thread").unwrap();
    let owner = host.attach("thread").unwrap();
    let label = format!(
        "Owner {} · generation {} · {:?} · write",
        owner.lease.owner_id, owner.lease.generation, owner.state
    );
    let mut workbench = HeadlessWorkbench::new(state(Some(host), dir.path()), [1280.0, 720.0]);
    workbench.run();

    let owner = workbench.label_rects(&label)[0];
    let title = workbench.label_rects("Thread: Header controls")[0];
    let attach = workbench.label_rects("Attach (read-only)")[0];
    let settings = workbench.label_rects("⚙")[0];
    let quota = workbench.label_rects("Codex · unavailable")[0];
    assert!(
        owner.bottom() < title.top(),
        "owner={owner:?}, title={title:?}"
    );
    assert!((owner.center().y - attach.center().y).abs() < owner.height());
    assert!(settings.right() < quota.left());
    assert!(settings.top() > title.bottom() && quota.top() > title.bottom());
}
