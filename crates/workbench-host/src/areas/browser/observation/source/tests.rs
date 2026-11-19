use super::*;
use browser::agent::observation::source::{Frame, ObservedTab};
fn fixture() -> (ObservationScope, Publication) {
    let scope = ObservationScope {
        project: ProjectId::new("p").unwrap(),
        thread: ThreadId::new("t").unwrap(),
        run: workbench_protocol::ids::RunId::new("run-1").unwrap(),
        incarnation: workbench_protocol::ids::RunId::new("incarnation").unwrap(),
        core_epoch: 1,
        browser_generation: 2,
        page_generation: 3,
    };
    let tab = "a".repeat(32);
    let page = Publication {
        execution: Some(execution(&scope)),
        revision: 9,
        tabs: vec![ObservedTab {
            id: tab.clone(),
            title: "title".into(),
            url: "local-only".into(),
        }],
        active_tab: Some(tab.clone()),
        activity: vec![],
        frame: Some(Arc::new(Frame {
            tab,
            sequence: 1,
            width: 1,
            height: 1,
            rgba: Arc::from([255, 0, 0, 255]),
        })),
    };
    (scope, page)
}
#[test]
fn rejects_wrong_incarnation_tab_pixels_and_oversized_rgba() {
    let (scope, mut page) = fixture();
    assert!(convert(&scope, &page).unwrap().validate());
    let frame = page.frame.as_ref().unwrap().clone();
    Arc::make_mut(page.frame.as_mut().unwrap()).tab = "b".repeat(32);
    assert!(matches!(
        convert(&scope, &page),
        Err(ObservationFailure::Unavailable)
    ));
    page.frame = Some(frame);
    Arc::make_mut(page.frame.as_mut().unwrap()).width = MAX_WIDTH + 1;
    assert!(convert(&scope, &page).is_err());
    page.execution.as_mut().unwrap().page_generation += 1;
    assert!(matches!(
        convert(&scope, &page),
        Err(ObservationFailure::Revoked)
    ));
}
#[test]
fn limits_utf8_and_escaped_whole_envelope_without_changing_identity() {
    let (scope, mut page) = fixture();
    page.tabs[0].title = "界".repeat(2048);
    page.activity = vec![
        browser::agent::Activity::Action {
            label: "\\".repeat(2048),
            error: Some("\\".repeat(2048))
        };
        MAX_ACTIVITY
    ];
    let bounded = convert(&scope, &page).unwrap();
    assert_eq!(bounded.snapshot.scope, scope);
    assert_eq!(bounded.snapshot.tabs[0].title.len(), 2046);
    assert!(bounded.snapshot.activity.len() < MAX_ACTIVITY);
    let reply = ObservationReply::Snapshot(Box::new(bounded.snapshot.clone()));
    assert!(reply.validate().is_ok());
    assert!(serde_json::to_vec(&reply).unwrap().len() <= MAX_REPLY_BYTES);
}
