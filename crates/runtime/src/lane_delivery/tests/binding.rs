use super::*;

#[tokio::test]
async fn delivery_checks_root_and_generation_without_process_owner() {
    let mut f = Fixture::new();
    let (lead, _) = f.lead().await;
    let lane = f.lane(lead, "binding-fence").await;
    let target = LaneDeliveryTarget {
        root_run_id: lane.root_run_id,
        generation: f
            .storage
            .handle()
            .lane(&lane.lane_id)
            .unwrap()
            .unwrap()
            .generation,
        authority: RunConfig::default(),
    };
    assert_eq!(
        f.runtime.check_delivery_target(&lane.thread_id, &target),
        Ok(())
    );
    let stale = LaneDeliveryTarget {
        generation: target.generation + 1,
        ..target.clone()
    };
    assert_eq!(
        f.runtime.check_delivery_target(&lane.thread_id, &stale),
        Err(LaneDeliveryNack::BindingConflict)
    );
    let wrong = LaneDeliveryTarget {
        root_run_id: lead,
        ..target.clone()
    };
    assert_eq!(
        f.runtime.check_delivery_target(&lane.thread_id, &wrong),
        Err(LaneDeliveryNack::BindingConflict)
    );
    f.runtime
        .stop(lane.root_run_id, crate::StopScope::SelfOnly)
        .unwrap();
    f.runtime.wait(lane.root_run_id).await.unwrap();
    assert_eq!(
        f.runtime.check_delivery_target(&lane.thread_id, &target),
        Err(LaneDeliveryNack::Stopped)
    );
    f.runtime.stop(lead, crate::StopScope::SelfOnly).unwrap();
    f.runtime.wait(lead).await.unwrap();
}

#[tokio::test]
async fn only_the_exact_obsolete_ownership_reason_allows_lane_history_reuse() {
    let mut f = Fixture::new();
    let (lead, host) = f.lead().await;
    f.finish(lead).await;
    for (reason, uncertain, accepted) in [
        (None, false, true),
        (
            Some(crate::restore::OWNERSHIP_ONLY_UNRESTORABLE_REASON),
            false,
            true,
        ),
        (Some("復元対象外の実行状態: ownership, team"), false, false),
        (Some("unknown reason"), false, false),
        (
            Some(crate::restore::OWNERSHIP_ONLY_UNRESTORABLE_REASON),
            true,
            true,
        ),
    ] {
        let mut bundle = f
            .runtime
            .shared
            .run_store
            .get()
            .unwrap()
            .restore_bundle(lead)
            .unwrap()
            .unwrap();
        let mut descriptor: crate::restore::RunRestoreDescriptor =
            serde_json::from_str(&bundle.record.config_json).unwrap();
        descriptor.restorable = reason.is_none();
        descriptor.non_restorable_reason = reason.map(str::to_owned);
        descriptor.interrupted_tool_calls = if uncertain {
            vec![crate::restore::InterruptedToolCall {
                call_id: "uncertain".into(),
                tool_name: "shell".into(),
                result_observed: false,
                may_have_side_effects: true,
            }]
        } else {
            Vec::new()
        };
        bundle.record.config_json = serde_json::to_string(&descriptor).unwrap();
        let provenance = bundle.provenance.unwrap();
        f.storage
            .handle()
            .save_snapshot_bundle(&storage::thread_index::SnapshotWrite {
                record: bundle.record,
                expected_snapshot_revision: Some(bundle.snapshot_revision),
                binding: None,
                status: provenance.status,
                map: provenance.map,
            })
            .unwrap();
        assert_eq!(
            f.runtime
                .check_delivery_target("delivery-lead", &host.target),
            if accepted {
                Ok(())
            } else {
                Err(LaneDeliveryNack::RestoreFailed)
            }
        );
    }
    f.runtime.stop(lead, crate::StopScope::SelfOnly).unwrap();
    f.runtime.wait(lead).await.unwrap();
}

#[tokio::test]
async fn stop_rechecks_destination_epoch_after_a_concurrent_bind() {
    let mut f = Fixture::new();
    let (lead, host) = f.lead().await;
    let handle = f.storage.handle();
    let owner = host.target.owner();
    BEFORE_STOP_CAS.with(|h| {
        *h.borrow_mut() = Some(Box::new(move || {
            handle
                .bind_lane_delivery_destination(
                    "delivery-lead".into(),
                    0,
                    owner,
                    LaneDestinationAction::Bind,
                )
                .unwrap();
        }))
    });
    f.runtime.persist_lane_delivery_stop(lead).unwrap();
    let destination = f
        .storage
        .handle()
        .lane_delivery_destination("delivery-lead".into())
        .unwrap()
        .unwrap();
    assert!(destination.stopped);
    assert_eq!(destination.epoch, 2);
    f.runtime.stop(lead, crate::StopScope::SelfOnly).unwrap();
    f.runtime.wait(lead).await.unwrap();
}

#[tokio::test]
async fn a_threadless_stop_is_inapplicable_but_a_bound_root_requires_storage() {
    let f = Fixture::new();
    let bus = Arc::new(EventBus::new(32));
    let bare = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        f.model.clone(),
    );
    let run = bare.reserve_run_id();
    assert!(bare.persist_lane_delivery_stop(run).is_ok());
    bare.bind_thread_root("bound", run).unwrap();
    assert!(bare.persist_lane_delivery_stop(run).is_err());
}
