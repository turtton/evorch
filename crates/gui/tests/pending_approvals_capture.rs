use gui::{
    app::WorkbenchState,
    fixture::{DemoSource, demo_pending_approval_events, demo_runs, demo_sidebar, populate},
    headless::HeadlessWorkbench,
};
use workspace_ui::{PanelId, UiSettings};

fn workbench() -> Result<HeadlessWorkbench<DemoSource>, Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let mut state = populate(
        WorkbenchState::new(DemoSource(demo_runs()), &UiSettings::default())?,
        demo_sidebar(dir.path())?,
    );
    state.apply_events(demo_pending_approval_events());
    let path = state
        .dock()
        .find_tab(&PanelId::new("approvals-main"))
        .ok_or("approvals tab missing")?;
    state
        .dock_mut()
        .set_active_tab(path)
        .map_err(|_| "activate approvals")?;
    Ok(HeadlessWorkbench::new(state, [1280.0, 900.0]))
}

#[test]
fn displays_two_scoped_requests_when_demo_is_populated() -> Result<(), Box<dyn std::error::Error>> {
    // Given: CLI と共通の承認待ちデモイベント。
    let mut workbench = workbench()?;
    // When: 実際の Workbench を描画する。
    workbench.run();
    // Then: 引数を持つ行とフォールバック行がそれぞれ判断可能になる。
    for label in [
        "shell",
        "write",
        "run-2 · call-1 · attempt 17",
        "run-3 · call-2 · attempt 18",
        r#"{"command":"rm -rf /tmp/build"}"#,
        "引数情報なし",
    ] {
        assert!(workbench.has_label(label), "missing {label}");
    }
    assert_eq!(workbench.count_labels("Approve"), 2);
    assert_eq!(workbench.count_labels("Reject"), 2);
    Ok(())
}

#[test]
#[ignore = "承認一覧の PNG 証跡を offscreen adapter で生成する"]
fn capture_pending_approvals_png_evidence() -> Result<(), Box<dyn std::error::Error>> {
    // Given: 2 件の要求がある承認一覧。
    let mut workbench = workbench()?;
    workbench.run();
    // When: 共通 adapter policy に従ってキャプチャする。
    let Some(frame) = gui::evidence::capture_or_skip(&mut workbench) else {
        return Ok(());
    };
    // Then: 指定サイズの PNG を保存し、デコード可能な証跡として確認する。
    let temporary = tempfile::tempdir()?;
    let output = std::env::var_os("EVORCH_APPROVALS_EVIDENCE")
        .map(std::path::PathBuf::from)
        .map_or_else(|| temporary.path().to_path_buf(), |path| path);
    std::fs::create_dir_all(&output)?;
    let path = output.join("pending-approvals-1280x900@1.0.png");
    frame.save_png(&path)?;
    assert_eq!(image::image_dimensions(&path)?, (1280, 900));
    println!("{}", path.display());
    Ok(())
}
