use gui::{
    app::{
        WorkbenchState,
        auto_title::{TitleSelection, select_model},
    },
    fixture::DemoSource,
    headless::HeadlessWorkbench,
    model::composer::ProviderStatus,
};
use std::sync::Arc;
use workspace_ui::UiSettings;

fn fixture(title: &str) -> (tempfile::TempDir, HeadlessWorkbench<DemoSource>) {
    let root = tempfile::tempdir().unwrap();
    let mut state = WorkbenchState::new(DemoSource(vec![]), &UiSettings::default())
        .unwrap()
        .with_provider_status(ProviderStatus::Configured);
    state.add_project(root.path()).unwrap();
    state.create_thread(title).unwrap();
    (root, HeadlessWorkbench::new(state, [1200.0, 900.0]))
}

fn finish(h: &mut HeadlessWorkbench<DemoSource>) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while h.state().auto_title_running() {
        assert!(std::time::Instant::now() < deadline);
        h.step();
        std::thread::yield_now();
    }
    h.run();
}

#[test]
fn first_user_message_on_default_title_triggers_rename() {
    // Given a new thread and an injected generator.
    let (_root, mut h) = fixture("New thread");
    h.state_mut()
        .set_title_generator(Arc::new(|_| Ok("Generated title".into())));
    // When the first real message is submitted.
    h.state_mut().composer_mut().input = "Explain Rust ownership".into();
    h.state_mut().submit_composer();
    finish(&mut h);
    // Then its visible title changes.
    assert_eq!(h.state().sidebar().threads[0].title, "Generated title");
}

#[test]
fn custom_title_never_overwritten() {
    // Given a custom title.
    let (_root, mut h) = fixture("My title");
    h.state_mut()
        .set_title_generator(Arc::new(|_| panic!("must not generate")));
    // When the first message is submitted.
    h.state_mut().composer_mut().input = "hello".into();
    h.state_mut().submit_composer();
    finish(&mut h);
    // Then the custom title survives.
    assert_eq!(h.state().sidebar().threads[0].title, "My title");
}

#[test]
fn quick_category_model_preferred_else_thread_model() {
    // Given distinct quick and thread models, and a fake selection consumer.
    let mut agents = config::AgentsConfig::default();
    let preference = Some(runtime::ModelPreference {
        profile: "thread-profile".into(),
        model: Some("thread-model".into()),
    });
    let fake = |selection| match selection {
        TitleSelection::Quick(binding) => binding.logical_model,
        TitleSelection::Thread(value) => value.unwrap().model.unwrap(),
    };
    agents.worker.categories.insert(
        "quick".into(),
        config::CategoryBindingConfig {
            logical_model: Some("fast-logical".into()),
            ..Default::default()
        },
    );
    // When selecting with and without the category.
    let quick = fake(select_model(&agents, preference.clone()));
    agents.worker.categories.clear();
    let fallback = fake(select_model(&agents, preference));
    // Then category wins only when configured.
    assert_eq!(quick, "fast-logical");
    assert_eq!(fallback, "thread-model");
}

#[test]
fn failure_falls_back_to_truncation_100_char_cap() {
    // Given an error or an overlong model response and Unicode input.
    for response in [Err("offline".into()), Ok("x".repeat(101))] {
        let (_root, mut h) = fixture("New thread");
        h.state_mut()
            .set_title_generator(Arc::new(move |_| response.clone()));
        h.state_mut().composer_mut().input = format!("{}\nsecond line", "界".repeat(120));
        // When generation finishes.
        h.state_mut().submit_composer();
        finish(&mut h);
        // Then fallback is the first line, capped by characters rather than bytes.
        assert_eq!(h.state().sidebar().threads[0].title, "界".repeat(100));
    }
}

#[test]
fn rename_while_generation_pending_wins_even_if_reset_to_default() {
    let (_root, mut h) = fixture("New thread");
    let (release, wait) = std::sync::mpsc::channel();
    let wait = std::sync::Mutex::new(wait);
    h.state_mut().set_title_generator(Arc::new(move |_| {
        wait.lock().unwrap().recv().unwrap();
        Ok("Late title".into())
    }));
    h.state_mut().composer_mut().input = "hello".into();
    h.state_mut().submit_composer();
    let id = h.state().sidebar().threads[0].id.clone();
    h.state_mut()
        .rename_thread(id.clone(), "My title".into())
        .unwrap();
    h.state_mut()
        .rename_thread(id, "New thread".into())
        .unwrap();
    release.send(()).unwrap();
    finish(&mut h);
    assert_eq!(h.state().sidebar().threads[0].title, "New thread");
}

#[test]
fn second_message_does_not_regenerate_default_looking_title() {
    let (_root, mut h) = fixture("New thread");
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let recorded = calls.clone();
    h.state_mut().set_title_generator(Arc::new(move |_| {
        recorded.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok("New thread".into())
    }));
    for message in ["first", "second"] {
        h.state_mut().composer_mut().input = message.into();
        h.state_mut().submit_composer();
        finish(&mut h);
    }
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn explicit_default_title_rename_before_first_message_is_preserved() {
    let (_root, mut h) = fixture("My title");
    let id = h.state().sidebar().threads[0].id.clone();
    h.state_mut()
        .rename_thread(id, "New thread".into())
        .unwrap();
    h.state_mut()
        .set_title_generator(Arc::new(|_| Ok("Unexpected".into())));
    h.state_mut().composer_mut().input = "first".into();
    h.state_mut().submit_composer();
    finish(&mut h);
    assert_eq!(h.state().sidebar().threads[0].title, "New thread");
}
