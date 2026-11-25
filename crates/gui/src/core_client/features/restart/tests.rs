use super::*;
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use workbench_protocol::{
    namespaces::lifecycle::{
        LifecycleReply, RestartPhase,
        restart::{
            AdminAuditDiagnostics, RestartManagementStatus, RestartReply, RestartStatus,
            RestartTarget, RestartTargets,
        },
    },
    wire::Revision,
};

struct Window {
    feature: RestartFeature,
    cache: crate::core_client::cache::Cache,
    selection: crate::core_client::features::Selection,
    frame: crate::core_client::features::FeatureFrame,
}

fn window(feature: RestartFeature) -> Harness<'static, Window> {
    Harness::builder()
        .with_size(egui::vec2(900.0, 900.0))
        .build_ui_state(
            |ui, window: &mut Window| {
                let Window {
                    feature,
                    cache,
                    selection,
                    frame,
                } = window;
                let mut context = FeatureContext {
                    data: &mut crate::core_client::features::data::DataInbox::default(),
                    feature: FeatureId::Restart,
                    view: crate::core_client::features::FeatureView {
                        connected: true,
                        capabilities: &[],
                        cache,
                        selection,
                    },
                    frame,
                };
                feature.render(ui, &mut context);
            },
            Window {
                feature,
                cache: Default::default(),
                selection: Default::default(),
                frame: Default::default(),
            },
        )
}

fn lifecycle(reply: RestartReply) -> FeatureReply {
    FeatureReply::Query(Ok(QueryReply::Lifecycle(LifecycleReply::Restart(reply))))
}

fn targets() -> FeatureReply {
    lifecycle(RestartReply::Targets(RestartTargets {
        core_epoch: InstanceEpoch(7),
        revision: Revision(1),
        targets: vec![RestartTarget {
            id: AdminId::new("current").unwrap(),
            revision: Revision(11),
        }],
        truncated: false,
    }))
}

fn status() -> FeatureReply {
    lifecycle(RestartReply::Status(RestartManagementStatus {
        audit: AdminAuditDiagnostics::default(),
        core_epoch: InstanceEpoch(7),
        status: RestartStatus {
            restart_id: None,
            phase: RestartPhase::Running,
            revision: 1,
            blockers: vec![],
            blocker_total: 0,
            blocker_omitted: 0,
            active_producers: 0,
            cancel_reason: None,
        },
    }))
}

fn has_cjk(text: &str) -> bool {
    text.chars()
        .any(|c| matches!(c, '\u{3000}'..='\u{30ff}' | '\u{3400}'..='\u{9fff}' | '\u{ff00}'..='\u{ffef}'))
}

fn assert_english(window: &Harness<'static, Window>) {
    for node in window.root().children_recursive() {
        let node = node.accesskit_node();
        for text in [node.label(), node.value()].into_iter().flatten() {
            let text = text.to_string();
            assert!(!has_cjk(&text), "{text}");
        }
    }
}

#[test]
fn the_panel_and_its_confirmation_read_in_english() {
    let mut feature = RestartFeature::default();
    feature.reply(1, targets());
    feature.reply(2, status());
    let mut window = window(feature);
    window.run_steps(2);
    assert!(
        window
            .query_by_label("Status: Running (revision 1)")
            .is_some()
    );
    assert_english(&window);

    window.get_by_label("Restart core…").click();
    window.run_steps(2);
    assert!(
        window
            .query_by_label_contains("Restart the core (target current, core epoch 7)")
            .is_some()
    );
    assert!(window.query_by_label("Restart now").is_some());
    assert_english(&window);

    window.get_by_label("Cancel").click();
    window.run_steps(2);
    assert!(window.state().feature.panel.confirming().is_none());
    assert!(window.query_by_label("Restart now").is_none());
    assert!(window.query_by_label("Restart core…").is_some());
}
