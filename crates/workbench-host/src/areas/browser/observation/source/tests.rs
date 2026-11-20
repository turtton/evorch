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
fn tab(id: &str, title: &str) -> ObservedTab {
    ObservedTab {
        id: id.repeat(32),
        title: title.into(),
        url: "local-only".into(),
    }
}
fn frame_of(id: &str, sequence: u64) -> Arc<Frame> {
    Arc::new(Frame {
        tab: id.repeat(32),
        sequence,
        width: 1,
        height: 1,
        rgba: Arc::from([sequence as u8, 0, 0, 255]),
    })
}
#[test]
fn active_tab_switch_pairs_new_metadata_only_with_its_own_pixels() {
    let (scope, mut page) = fixture();
    page.tabs = vec![tab("a", "first"), tab("b", "second")];
    // before the switch: the first tab and its pixels
    page.active_tab = Some("a".repeat(32));
    page.frame = Some(frame_of("a", 1));
    let before = convert(&scope, &page).unwrap();
    assert_eq!(
        before.frame.as_ref().unwrap().start.tab.as_str(),
        "a".repeat(32)
    );
    // a switch whose pixels have not arrived: new metadata, no old pixels
    page.active_tab = Some("b".repeat(32));
    page.frame = None;
    let switching = convert(&scope, &page).unwrap();
    assert_eq!(
        switching.snapshot.active_tab.as_ref().unwrap().as_str(),
        "b".repeat(32)
    );
    assert!(switching.frame.is_none());
    // new metadata with the old tab's pixels is never converted
    page.frame = Some(frame_of("a", 2));
    assert!(matches!(
        convert(&scope, &page),
        Err(ObservationFailure::Unavailable)
    ));
    // and once the new tab's pixels arrive the pair is whole
    page.frame = Some(frame_of("b", 3));
    let after = convert(&scope, &page).unwrap();
    assert!(after.validate());
    assert_eq!(
        after.frame.as_ref().unwrap().start.tab.as_str(),
        "b".repeat(32)
    );
    assert_eq!(after.frame.as_ref().unwrap().rgba[0], 3);
}
#[test]
fn trimming_an_oversized_envelope_keeps_the_active_tab_its_pixels_belong_to() {
    let (scope, mut page) = fixture();
    let ids: Vec<String> = (0..MAX_TABS)
        .map(|n| format!("{n:02x}").repeat(16))
        .collect();
    page.tabs = ids
        .iter()
        .map(|id| ObservedTab {
            id: id.clone(),
            title: "界".repeat(512),
            url: "\\".repeat(1024),
        })
        .collect();
    let active = ids.last().unwrap().clone();
    page.active_tab = Some(active.clone());
    page.frame = Some(Arc::new(Frame {
        tab: active.clone(),
        sequence: 1,
        width: 1,
        height: 1,
        rgba: Arc::from([1, 2, 3, 255]),
    }));
    let trimmed = convert(&scope, &page).unwrap();
    assert!(
        trimmed.snapshot.tabs.len() < MAX_TABS,
        "the envelope was trimmed"
    );
    assert_eq!(
        trimmed.snapshot.active_tab.as_ref().unwrap().as_str(),
        active
    );
    assert!(
        trimmed
            .snapshot
            .tabs
            .iter()
            .any(|t| t.id.as_str() == active)
    );
    assert_eq!(trimmed.frame.as_ref().unwrap().start.tab.as_str(), active);
}
