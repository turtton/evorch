//! Dedicated Reviewer tool lifecycle through HTTP with input-derived cache usage.
//! Gates order independent runs by explicit tool arrivals, never scheduler timing.
use super::*;
use sha2::{Digest, Sha256};
pub(super) struct ReadGate(
    pub(super) tokio::sync::mpsc::UnboundedSender<(String, tokio::sync::oneshot::Sender<()>)>,
);
#[async_trait::async_trait]
impl Tool for ReadGate {
    fn name(&self) -> &'static str {
        "read"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"index":{"type":"integer"}},"required":["index"]})
    }
    fn permissions(&self) -> Permissions {
        Permissions::read_only()
    }
    fn execution_mode(&self) -> tools::ToolExecutionMode {
        tools::ToolExecutionMode::Shared
    }
    async fn execute(&self, _: Value) -> Result<ToolResult, ToolError> {
        unreachable!("context required")
    }
    async fn execute_with_context(
        &self,
        ctx: &tools::ToolExecutionContext,
        _: Value,
    ) -> Result<ToolResult, ToolError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.0.send((ctx.run_id.clone(), tx)).unwrap();
        rx.await.unwrap();
        Ok(ToolResult::success("actual artifact bytes"))
    }
}
#[tokio::test]
async fn reviewer_results_close_success_and_stale_rejection_preserve_prefix_after_toolset_boundary()
{
    let (tx, mut reads) = tokio::sync::mpsc::unbounded_channel();
    let mut h = harness_with_read(
        vec![text_response("lead ready")],
        1_000_000,
        Arc::new(ReadGate(tx)),
    );
    let config = storage::StorageConfig {
        db_path: h._directory.path().join("verification.db"),
        ..Default::default()
    };
    let db = storage::Storage::open(config.clone()).unwrap();
    h.runtime = h
        .runtime
        .with_run_store(runtime::RunStore::open(&config, db.handle()).unwrap());
    h.runtime
        .set_project_root(h._directory.path().to_owned())
        .unwrap();
    let artifact = h._directory.path().join("artifact.txt");
    std::fs::write(&artifact, b"actual artifact bytes").unwrap();
    let digest = format!("{:x}", Sha256::digest(b"actual artifact bytes"));
    let lead = h
        .runtime
        .delegate_chat(
            "lead",
            Role::Orchestrator,
            "coordinate lane verification".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                conversation: true,
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = through_phase(&mut h.receiver, lead, AgentRunPhase::Waiting).await;
    let lane = open(
        &mut h,
        lead,
        "open-verified",
        "reviewable artifact",
        &mut events,
    )
    .await;
    let lane_id = result(&h, "open-verified")["lane_id"]
        .as_str()
        .unwrap()
        .to_owned();
    input(&mut h,lane,"report ready",vec![call("ready","lane_report",json!({"status":"ready","summary":"claimed ready","artifact_revision":{"kind":"sha256","value":digest},"artifact_refs":[artifact.to_string_lossy()]})),text_response("reported")],&mut events).await;
    for (round, approved) in [("rejected", false), ("approved", true)] {
        let verify_call = format!("verify-{round}");
        h.mock.enqueue([
            call(
                &verify_call,
                "lane_verify",
                json!({"lane_id":lane_id,"ready_seq":1}),
            ),
            call("read-boundary", "read", json!({"index":0})),
            call("read-boundary", "read", json!({"index":0})),
        ]);
        h.runtime
            .send_message(lead, format!("verify {round}"))
            .unwrap();
        // Both next provider requests have been recorded before either read can finish.
        let a = reads.recv().await.unwrap();
        let b = reads.recv().await.unwrap();
        let (lead_gate, reviewer_gate) = if a.0 == lead.to_string() {
            (a.1, b)
        } else {
            (b.1, a)
        };
        let verification = result(&h, &verify_call);
        let reference = verification["verification_ref"]
            .as_str()
            .unwrap()
            .to_owned();
        let reviewer: RunId = verification["reviewer_run_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(reviewer_gate.0, reviewer.to_string());
        h.mock
            .enqueue([text_response("Reviewer started; awaiting result")]);
        lead_gate.send(()).unwrap();
        events.extend(through_phase(&mut h.receiver, lead, AgentRunPhase::Waiting).await);
        h.mock.enqueue([call("submit","submit_lane_verification",json!({"verification_ref":reference,"verdict":if approved{"approved"}else{"rejected"},"checks":[{"criterion_index":0,"passed":approved,"evidence_refs":[artifact.to_string_lossy()]}],"evidence_refs":[artifact.to_string_lossy()],"findings":if approved{Vec::<String>::new()}else{vec!["missing evidence found independently".into()]}})),text_response("Reviewer result observed")]);
        reviewer_gate.1.send(()).unwrap();
        h.runtime.wait(reviewer).await.unwrap();
        events.extend(through_phase(&mut h.receiver, lead, AgentRunPhase::Waiting).await);
        let close_call = format!("close-{round}");
        input(
            &mut h,
            lead,
            "close with Reviewer evidence",
            vec![
                call(
                    &close_call,
                    "lane_close",
                    json!({"lane_id":lane_id,"ready_seq":1,"verification_ref":reference}),
                ),
                text_response("close result observed"),
            ],
            &mut events,
        )
        .await;
        let closed = result(&h, &close_call);
        if approved {
            assert_eq!(closed["outcome"], "closed");
            std::fs::write(&artifact, b"changed after review").unwrap();
            input(
                &mut h,
                lead,
                "try old evidence after artifact change",
                vec![
                    call(
                        "stale-close",
                        "lane_close",
                        json!({"lane_id":lane_id,"ready_seq":1,"verification_ref":reference}),
                    ),
                    text_response("stale evidence rejected"),
                ],
                &mut events,
            )
            .await;
            assert_eq!(
                result(&h, "stale-close")["error"]["code"],
                "artifact_changed"
            );
        } else {
            assert_eq!(closed["error"]["code"], "verification_rejected");
        }
    }
    input(
        &mut h,
        lead,
        "continue after both close results",
        vec![text_response("prefix still reusable")],
        &mut events,
    )
    .await;
    for run in [lead, lane] {
        h.runtime.cancel(run).unwrap();
        h.runtime.wait(run).await.unwrap();
    }
    assert_eq!(h.mock.remaining_scripts(), 0);
    let requests: Vec<_> = h
        .mock
        .recorded_requests()
        .into_iter()
        .filter(|r| r.path == Wire::ChatCompletions.path())
        .map(|r| r.body)
        .collect();
    let roots: Vec<_> = requests
        .iter()
        .filter(|r| {
            r["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["function"]["name"] == "lane_verify")
        })
        .cloned()
        .collect();
    assert!(roots.len() > 10);
    for pair in roots.windows(2) {
        assert_append_only(CacheProtocol::OpenAi, &pair[0], &pair[1]).unwrap();
    }
    assert!(
        roots.last().unwrap()["messages"]
            .to_string()
            .contains("missing evidence found independently")
    );
    assert!(
        roots.last().unwrap()["messages"]
            .to_string()
            .contains("artifact_changed")
    );
    assert!(
        roots.last().unwrap()["messages"]
            .to_string()
            .contains("close-approved")
    );
    let first_toolset = &roots[0]["tools"];
    let open_spec = first_toolset
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["function"]["name"] == "lane_open")
        .unwrap();
    assert!(open_spec.to_string().contains("verification_requirements"));
    let reviewer_requests: Vec<_> = requests
        .iter()
        .filter(|r| {
            r["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["function"]["name"] == "submit_lane_verification")
        })
        .collect();
    assert_eq!(reviewer_requests.len(), 4);
    for pair in reviewer_requests.chunks(2) {
        assert_append_only(CacheProtocol::OpenAi, pair[0], pair[1]).unwrap();
    }
    let usage: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Provider(ProviderEvent::RequestCompleted {
                run_id: Some(id),
                input_tokens,
                cache_read_tokens,
                ..
            }) if id == &lead.to_string() => Some((*input_tokens, *cache_read_tokens)),
            EventKind::Diagnostic(d) if d.code == "CacheRegression" => panic!("{d:?}"),
            _ => None,
        })
        .collect();
    assert_eq!(usage.len(), roots.len());
    for pair in usage.windows(2) {
        assert!(pair[1].1 >= pair[0].0);
    }
}
