//! Exercise the production composition, not direct lifecycle-hook calls.
use super::*;
use crate::core_client::{CoreClientScreen, Shared};
use std::sync::{Arc, Mutex};

#[derive(Debug, PartialEq)]
enum Observed {
    Selection(Option<ProjectId>, Option<ThreadId>),
    Maintain(Visibility, bool),
    Render,
    Disconnected,
    Preview,
}
type Log = Arc<Mutex<Vec<Observed>>>;
struct Probe {
    log: Log,
    availability: Availability,
}
impl Feature for Probe {
    fn id(&self) -> FeatureId {
        FeatureId::Browser
    }
    fn title(&self) -> &'static str {
        "maintenance probe"
    }
    fn availability(&self) -> Availability {
        self.availability
    }
    fn disconnected(&mut self) {
        self.log.lock().unwrap().push(Observed::Disconnected);
    }
    fn open_preview(&mut self, _artifact: ArtifactRef) {
        self.log.lock().unwrap().push(Observed::Preview);
    }
    fn selection_changed(&mut self, ctx: &mut FeatureContext<'_>) {
        self.log.lock().unwrap().push(Observed::Selection(
            ctx.selection().project.clone(),
            ctx.selection().thread.clone(),
        ));
    }
    fn maintain(&mut self, ctx: &mut FeatureContext<'_>, visibility: Visibility) {
        self.log
            .lock()
            .unwrap()
            .push(Observed::Maintain(visibility, ctx.connected()));
        if !visibility.is_visible() {
            ctx.cancel_data(9);
        }
    }
    fn render(&mut self, _ui: &mut egui::Ui, _ctx: &mut FeatureContext<'_>) {
        self.log.lock().unwrap().push(Observed::Render);
    }
}
fn features(log: &Log, availability: Availability) -> Features {
    Features {
        features: vec![Box::new(Probe {
            log: log.clone(),
            availability,
        })],
        data: DataInbox::default(),
        selection: None,
        expanded: Default::default(),
        reveal: None,
    }
}
fn take(log: &Log) -> Vec<Observed> {
    std::mem::take(&mut *log.lock().unwrap())
}
fn input(minimized: bool) -> egui::RawInput {
    let mut input = egui::RawInput::default();
    input
        .viewports
        .get_mut(&egui::ViewportId::ROOT)
        .unwrap()
        .minimized = Some(minimized);
    input
}
fn compose(
    ctx: &egui::Context,
    screen: &mut CoreClientScreen,
    input: egui::RawInput,
) -> egui::FullOutput {
    let mut output = ctx.run_ui(input, |ui| {
        screen.logic(ui.ctx());
        screen.ui(ui);
    });
    output.textures_delta.clear();
    output
}
fn title_position(shapes: &[egui::epaint::ClippedShape]) -> egui::Pos2 {
    text_position(shapes, "maintenance probe")
}
fn text_position(shapes: &[egui::epaint::ClippedShape], wanted: &str) -> egui::Pos2 {
    fn find(shape: &egui::Shape, wanted: &str) -> Option<egui::Pos2> {
        match shape {
            egui::Shape::Text(text) if text.galley.text() == wanted => {
                Some(text.pos + text.galley.rect.center().to_vec2())
            }
            egui::Shape::Vec(shapes) => shapes.iter().find_map(|s| find(s, wanted)),
            _ => None,
        }
    }
    shapes
        .iter()
        .find_map(|s| find(&s.shape, wanted))
        .unwrap_or_else(|| panic!("{wanted:?} was drawn"))
}
fn assert_cancels(
    rx: &mut tokio::sync::mpsc::Receiver<crate::core_client::connection::Action>,
    expected: usize,
) {
    let mut count = 0;
    while let Ok(action) = rx.try_recv() {
        match action {
            crate::core_client::connection::Action::Data(action) => {
                assert_eq!((action.feature, action.tag), (FeatureId::Browser, 9));
                assert!(matches!(action.request, DataRequest::Cancel));
                count += 1;
            }
            _ => panic!("maintenance must not create unrelated actions"),
        }
    }
    assert_eq!(
        count, expected,
        "cancel_data must reach the shared actor queue"
    );
}

#[test]
fn collapsed_selection_and_minimized_disconnect_run_maintenance_through_screen_composition() {
    let log = Log::default();
    let shared = Arc::new(Mutex::new(Shared {
        connected: true,
        features: features(&log, Availability::Implemented { required: &[] }),
        ..Shared::default()
    }));
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let mut screen = CoreClientScreen::new(shared.clone(), tx, None, None);
    let ctx = egui::Context::default();
    let first = compose(&ctx, &mut screen, input(false));
    let expanded = Visibility {
        available: true,
        expanded: true,
        minimized: false,
    };
    assert_eq!(
        take(&log),
        [
            Observed::Selection(None, None),
            Observed::Maintain(expanded, true),
            Observed::Render
        ]
    );
    assert_cancels(&mut rx, 0);
    let pos = title_position(&first.shapes);
    // Real pointer input closes the production header (including its title).
    for pressed in [true, false] {
        let mut input = input(false);
        input.events = vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::default(),
            },
        ];
        compose(&ctx, &mut screen, input);
    }
    take(&log);
    while rx.try_recv().is_ok() {}
    let project = ProjectId::new("new-project").unwrap();
    let thread = ThreadId::new("new-thread").unwrap();
    screen.project = Some(project.clone());
    screen.selected = Some(thread.clone());
    compose(&ctx, &mut screen, input(false));
    let collapsed = Visibility {
        expanded: false,
        ..expanded
    };
    assert_eq!(
        take(&log),
        [
            Observed::Selection(Some(project), Some(thread)),
            Observed::Maintain(collapsed, true)
        ]
    );
    assert_cancels(&mut rx, 1);
    // Same selection: maintain repeats, selection_changed does not.
    compose(&ctx, &mut screen, input(true));
    assert_eq!(
        take(&log),
        [Observed::Maintain(
            Visibility {
                minimized: true,
                ..collapsed
            },
            true
        )]
    );
    assert_cancels(&mut rx, 1);
    shared.lock().unwrap().disconnect();
    assert_eq!(
        take(&log),
        [Observed::Disconnected],
        "no composition is needed for immediate invalidation"
    );
    shared.lock().unwrap().disconnect();
    assert!(
        take(&log).is_empty(),
        "only the true -> false transition notifies"
    );
    compose(&ctx, &mut screen, input(true));
    assert_eq!(
        take(&log),
        [Observed::Maintain(
            Visibility {
                available: false,
                minimized: true,
                ..collapsed
            },
            false
        )]
    );
    assert_cancels(&mut rx, 1);
    // Restoring the viewport preserves collapse; the body is still not rendered.
    compose(&ctx, &mut screen, input(false));
    assert_eq!(
        take(&log),
        [Observed::Maintain(
            Visibility {
                available: false,
                ..collapsed
            },
            false
        )]
    );
    assert_cancels(&mut rx, 1);
}

#[test]
fn unavailable_and_placeholder_features_still_maintain_on_each_render_all_pass() {
    for availability in [
        Availability::Placeholder,
        Availability::Implemented {
            required: &[Capability::Queries],
        },
    ] {
        let log = Log::default();
        let mut features = features(&log, availability);
        let ctx = egui::Context::default();
        let cache = Cache::default();
        let selection = Selection::default();
        for pass in 0..2 {
            let mut frame = None;
            let mut output = ctx.run_ui(input(false), |ui| {
                frame = Some(features.render(
                    ui,
                    FeatureView {
                        connected: true,
                        capabilities: &[],
                        cache: &cache,
                        selection: &selection,
                    },
                ))
            });
            output.textures_delta.clear();
            let mut expected = vec![];
            if pass == 0 {
                expected.push(Observed::Selection(None, None));
            }
            expected.push(Observed::Maintain(
                Visibility {
                    available: false,
                    expanded: true,
                    minimized: false,
                },
                true,
            ));
            assert_eq!(take(&log), expected);
            let frame = frame.unwrap();
            assert_eq!(frame.data.len(), 1);
            assert!(matches!(frame.data[0].request, DataRequest::Cancel));
            assert!(frame.actions.is_empty());
        }
    }
}

#[test]
fn expanded_minimize_restore_and_disconnect_preserve_visibility_and_invalidate_data() {
    use workbench_protocol::namespaces::browser::RendererId;
    let log = Log::default();
    let shared = Arc::new(Mutex::new(Shared {
        connected: true,
        features: features(&log, Availability::Implemented { required: &[] }),
        ..Shared::default()
    }));
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let mut screen = CoreClientScreen::new(shared.clone(), tx, None, None);
    let ctx = egui::Context::default();
    compose(&ctx, &mut screen, input(false));
    take(&log);
    let expanded = Visibility {
        available: true,
        expanded: true,
        minimized: false,
    };
    compose(&ctx, &mut screen, input(true));
    assert_eq!(
        take(&log),
        [Observed::Maintain(
            Visibility {
                minimized: true,
                ..expanded
            },
            true
        )]
    );
    assert_cancels(&mut rx, 1);
    compose(&ctx, &mut screen, input(false));
    assert_eq!(
        take(&log),
        [Observed::Maintain(expanded, true), Observed::Render]
    );
    assert_cancels(&mut rx, 0);
    let old_action = {
        let mut state = shared.lock().unwrap();
        let data = &mut state.features.data;
        let source = DataSource::Capture {
            operation_id: RendererId::new("a".repeat(32)).unwrap(),
            capture_id: RendererId::new("b".repeat(32)).unwrap(),
        };
        let action = DataAction {
            feature: FeatureId::Browser,
            tag: 9,
            generation: data.generation(FeatureId::Browser),
            request: DataRequest::Start(source.clone()),
        };
        let key = data.claim(&action, source).unwrap();
        state.disconnect();
        assert!(!state.features.data.contains(key));
        assert!(
            !state.features.data.current(&action),
            "old queued starts/cancels are stale"
        );
        action
    };
    assert_eq!(take(&log), [Observed::Disconnected]);
    // A disconnected selection change still precedes maintain with a usable ctx.
    let project = ProjectId::new("disconnected-project").unwrap();
    screen.project = Some(project.clone());
    compose(&ctx, &mut screen, input(false));
    assert_eq!(
        take(&log),
        [
            Observed::Selection(Some(project), None),
            Observed::Maintain(
                Visibility {
                    available: false,
                    ..expanded
                },
                false
            )
        ]
    );
    let crate::core_client::connection::Action::Data(cancel) = rx.try_recv().unwrap() else {
        panic!("expected cancellation")
    };
    assert!(matches!(cancel.request, DataRequest::Cancel));
    assert_ne!(cancel.generation, old_action.generation);
    assert!(shared.lock().unwrap().features.data.current(&cancel));
    assert!(rx.try_recv().is_err());
}

#[test]
fn native_logic_only_minimization_delivers_maintenance_and_cancel_without_a_ui_pass() {
    use crate::core_client::CoreClientApp;
    use eframe::App;

    fn native_ui(
        ctx: &egui::Context,
        app: &mut CoreClientApp,
        frame: &mut eframe::Frame,
        input: egui::RawInput,
    ) -> egui::FullOutput {
        // eframe::Integration::update runs these in this order in each UI pass.
        let mut output = ctx.run_ui(input, |ui| {
            app.logic(ui.ctx(), frame);
            app.ui(ui, frame);
        });
        output.textures_delta.clear();
        output
    }
    fn native_logic(ctx: &egui::Context, app: &mut CoreClientApp, frame: &mut eframe::Frame) {
        // eframe::Integration::update_logic_only uses run_logic, not run_ui.
        // The unchanged pass number proves this is not another forced UI pass.
        let before = ctx.cumulative_pass_nr();
        let _ = ctx.run_logic(&input(true), |ctx| app.logic(ctx, frame));
        assert_eq!(ctx.cumulative_pass_nr(), before);
    }

    let log = Log::default();
    let shared = Arc::new(Mutex::new(Shared {
        connected: true,
        features: features(&log, Availability::Implemented { required: &[] }),
        ..Shared::default()
    }));
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let task = runtime.spawn(async {});
    let mut app = CoreClientApp {
        screen: CoreClientScreen::new(shared, tx, None, None),
        _runtime: runtime,
        task,
    };
    let ctx = egui::Context::default();
    let mut frame = eframe::Frame::_new_kittest();
    let minimized = Visibility {
        available: true,
        expanded: true,
        minimized: true,
    };

    // Starting minimized must also work before there has ever been a UI pass.
    native_logic(&ctx, &mut app, &mut frame);
    assert_eq!(
        take(&log),
        [
            Observed::Selection(None, None),
            Observed::Maintain(minimized, true)
        ]
    );
    assert_cancels(&mut rx, 1);
    let project = ProjectId::new("hidden-selection").unwrap();
    app.screen.project = Some(project.clone());
    native_logic(&ctx, &mut app, &mut frame);
    assert_eq!(
        take(&log),
        [
            Observed::Selection(Some(project), None),
            Observed::Maintain(minimized, true)
        ]
    );
    assert_cancels(&mut rx, 1);

    let visible = Visibility {
        minimized: false,
        ..minimized
    };
    let output = native_ui(&ctx, &mut app, &mut frame, input(false));
    assert_eq!(
        take(&log),
        [Observed::Maintain(visible, true), Observed::Render]
    );
    assert_cancels(&mut rx, 0);
    let pos = title_position(&output.shapes);
    for pressed in [true, false] {
        let mut input = input(false);
        input.events = vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::default(),
            },
        ];
        native_ui(&ctx, &mut app, &mut frame, input);
    }
    take(&log);
    while rx.try_recv().is_ok() {}
    let collapsed_minimized = Visibility {
        expanded: false,
        ..minimized
    };
    native_logic(&ctx, &mut app, &mut frame);
    assert_eq!(take(&log), [Observed::Maintain(collapsed_minimized, true)]);
    assert_cancels(&mut rx, 1);

    // A visible child can make eframe run UI even with the root minimized.
    // Calling BOTH actual App entry points must still maintain/cancel only once.
    native_ui(&ctx, &mut app, &mut frame, input(true));
    assert_eq!(take(&log), [Observed::Maintain(collapsed_minimized, true)]);
    assert_cancels(&mut rx, 1);
    native_ui(&ctx, &mut app, &mut frame, input(false));
    assert_eq!(
        take(&log),
        [Observed::Maintain(
            Visibility {
                minimized: false,
                ..collapsed_minimized
            },
            true
        )]
    );
    assert_cancels(&mut rx, 1);
}

fn click(ctx: &egui::Context, screen: &mut CoreClientScreen, pos: egui::Pos2) {
    for pressed in [true, false] {
        let mut input = input(false);
        input.events = vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::default(),
            },
        ];
        compose(ctx, screen, input);
    }
}

fn picture() -> ArtifactRef {
    ArtifactRef {
        project: ProjectId::new("p").unwrap(),
        thread: ThreadId::new("t").unwrap(),
        artifact_id: format!("artifact-{}", "07".repeat(16)),
        digest: "07".repeat(32),
    }
}

#[test]
fn a_preview_asked_for_in_a_collapsed_section_opens_the_section_so_it_can_start() {
    let log = Log::default();
    let shared = Arc::new(Mutex::new(Shared {
        connected: true,
        features: features(&log, Availability::Implemented { required: &[] }),
        ..Shared::default()
    }));
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let mut screen = CoreClientScreen::new(shared.clone(), tx, None, None);
    let ctx = egui::Context::default();
    let first = compose(&ctx, &mut screen, input(false));
    click(&ctx, &mut screen, title_position(&first.shapes));
    take(&log);
    compose(&ctx, &mut screen, input(false));
    let collapsed = take(&log);
    assert!(
        matches!(collapsed.as_slice(), [Observed::Maintain(v, true)] if !v.expanded),
        "the click collapsed the section: {collapsed:?}"
    );
    // The same route the artifacts feature takes: the frame's reference reaches the
    // browser feature and the section is opened for it.
    shared.lock().unwrap().features.open_preview(picture());
    compose(&ctx, &mut screen, input(false));
    let opened = take(&log);
    assert_eq!(opened.first(), Some(&Observed::Preview));
    assert!(
        opened
            .iter()
            .any(|o| matches!(o, Observed::Maintain(v, true) if v.expanded)),
        "{opened:?}"
    );
    assert!(opened.contains(&Observed::Render), "{opened:?}");
    // Asking once opens once: the user may collapse the section again.
    let again = compose(&ctx, &mut screen, input(false));
    click(&ctx, &mut screen, title_position(&again.shapes));
    take(&log);
    compose(&ctx, &mut screen, input(false));
    assert!(
        !take(&log).contains(&Observed::Render),
        "a spent request is not repeated"
    );
}

#[test]
fn a_preview_the_core_cannot_show_is_refused_with_a_message_not_queued() {
    let log = Log::default();
    let mut shared = Shared {
        connected: true,
        capabilities: vec![],
        features: features(
            &log,
            Availability::Implemented {
                required: &[Capability::Queries],
            },
        ),
        ..Shared::default()
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let (mut project, mut selected) = (None, None);
    crate::core_client::apply_frame(
        &tx,
        &mut shared,
        &mut project,
        &mut selected,
        FeatureFrame {
            previews: vec![picture()],
            ..FeatureFrame::default()
        },
    );
    assert!(take(&log).is_empty(), "nothing reached the feature");
    assert!(shared.status.contains("プレビュー"), "{}", shared.status);
}

#[test]
fn the_conversation_row_button_opens_the_section_of_what_the_thread_produced() {
    let log = Log::default();
    let shared = Arc::new(Mutex::new(Shared {
        connected: true,
        features: features(&log, Availability::Implemented { required: &[] }),
        ..Shared::default()
    }));
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let mut screen = CoreClientScreen::new(
        shared.clone(),
        tx,
        Some(ProjectId::new("p").unwrap()),
        Some(ThreadId::new("t").unwrap()),
    );
    let ctx = egui::Context::default();
    let first = compose(&ctx, &mut screen, input(false));
    click(&ctx, &mut screen, title_position(&first.shapes));
    take(&log);
    let collapsed = compose(&ctx, &mut screen, input(false));
    assert!(!take(&log).contains(&Observed::Render));
    // The button of the conversation row, clicked like a user does.
    click(
        &ctx,
        &mut screen,
        text_position(&collapsed.shapes, "プレビュー / 検査"),
    );
    compose(&ctx, &mut screen, input(false));
    assert!(
        take(&log).contains(&Observed::Render),
        "the section the button names is opened and drawn"
    );
}
