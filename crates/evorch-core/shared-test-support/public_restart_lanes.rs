//! One lane graph through the public, sealed product. Route the concurrent
//! lead/lane/worker requests by their public tool schemas to independent scripted
//! input-derived mock caches, so scheduler order never selects a response.
use super::{product_exec, public_restart_product as common};
use common::{chat, connect};
use mock_openai::{
    ResponseGate, ScriptedResponse,
    cache_contract::{CacheProtocol, assert_append_only},
};
use product_exec::{
    artifact::ProductArtifact,
    env::IsolatedEnv,
    launch::{KEY_ENV, ProductLaunch, READY_WATCHDOG},
    observe,
    wire::MockProvider,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use workbench_client::{
    WorkbenchClient,
    restart::{
        confirmation::confirm,
        intent::{TargetSelector, plan_restart, request_restart},
        session::RestartAdmin,
    },
};
use workbench_protocol::{
    events::*,
    handshake::LocalSurface,
    namespaces::{
        ExecutionEvent,
        lanes::{LanesQuery, LanesReply, ListLanes},
        lifecycle::AdminId,
    },
    queries::{PageRequest, Query, QueryReply},
    views::RunPhase,
};

struct Release(ResponseGate);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
fn tool(id: &str, name: &str, input: Value) -> ScriptedResponse {
    ScriptedResponse::tool_call(id, "mock-model", 0, id, name, [input.to_string()])
}
fn text(id: &str) -> ScriptedResponse {
    ScriptedResponse::text_stream(id, "mock-model", [id])
}
struct Router {
    base: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Router {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Router {
    async fn start(lead: String, lane: String, worker: String) -> Self {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (lead, lane, worker, client) =
                    (lead.clone(), lane.clone(), worker.clone(), client.clone());
                connections.spawn(async move {
                    let mut stream = BufReader::new(stream);
                    let mut line = String::new();
                    stream.read_line(&mut line).await.unwrap();
                    let method = line.split_whitespace().next().unwrap().to_owned();
                    let mut length = 0;
                    loop {
                        line.clear();
                        if stream.read_line(&mut line).await.unwrap() == 0 { return; }
                        if line == "\r\n" {
                            break;
                        }
                        if let Some((name, value)) = line.split_once(':')
                            && name.eq_ignore_ascii_case("content-length")
                        {
                            length = value.trim().parse().unwrap();
                        }
                    }
                    let mut body = vec![0; length];
                    stream.read_exact(&mut body).await.unwrap();
                    let response = if method == "GET" {
                        client.get(format!("{lead}/models")).send().await.unwrap()
                    } else {
                        let request: Value = serde_json::from_slice(&body).unwrap();
                        let has = |name: &str| request["tools"].as_array().unwrap().iter()
                            .any(|t| t["function"]["name"].as_str() == Some(name));
                        let target = if has("lane_open") { lead }
                            else if has("lane_report") { lane } else { worker };
                        client.post(format!("{target}/chat/completions"))
                            .header("Content-Type", "application/json").body(body)
                            .send().await.unwrap()
                    };
                    let status = response.status().as_u16();
                    let content_type = response.headers()["content-type"].to_str().unwrap().to_owned();
                    let body = response.bytes().await.unwrap();
                    let header = format!("HTTP/1.1 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    // Exec/shutdown may close an already serviced socket.
                    if stream.get_mut().write_all(header.as_bytes()).await.is_ok() {
                        let _ = stream.get_mut().write_all(&body).await;
                    }
                });
                // Reap completed connections without waiting for any gated one.
                while let Some(result) = connections.try_join_next() {
                    result.unwrap();
                }
            }
        });
        Self { base, task }
    }
}
async fn subscribe(client: &WorkbenchClient) -> common::Subscription {
    client
        .subscribe(Subscribe {
            scope: SubscriptionScope::All,
            after: None,
        })
        .await
        .unwrap()
}
async fn phases(
    client: &WorkbenchClient,
    events: &mut common::Subscription,
    wanted: BTreeMap<String, RunPhase>,
    include_snapshot: bool,
) {
    let mut pending: BTreeSet<_> = wanted.keys().cloned().collect();
    if include_snapshot {
        observe_snapshot(events, &wanted, &mut pending);
    }
    while !pending.is_empty() {
        let event = events.next().await.expect("core event stream");
        match event.event {
            UiEvent::Execution(ExecutionEvent::RunChanged { run }) => {
                if let Some(phase) = wanted.get(run.id.as_str()) {
                    assert_ne!(run.phase, RunPhase::Failed, "{run:?}");
                    assert_ne!(run.phase, RunPhase::Cancelled, "{run:?}");
                    if &run.phase == phase {
                        pending.remove(run.id.as_str());
                    }
                }
            }
            UiEvent::Projection(
                workbench_protocol::namespaces::ProjectionEvent::ResyncRequired { .. },
            ) => {
                *events = subscribe(client).await;
                observe_snapshot(events, &wanted, &mut pending);
            }
            _ => {}
        }
    }
}
fn observe_snapshot(
    events: &common::Subscription,
    wanted: &BTreeMap<String, RunPhase>,
    pending: &mut BTreeSet<String>,
) {
    if let Some(page) = &events.started().initial_page {
        for item in &page.items {
            if let SnapshotItem::Run(run) = item {
                assert_ne!(run.phase, RunPhase::Failed, "{run:?}");
            }
            if let SnapshotItem::Run(run) = item
                && wanted.get(run.id.as_str()) == Some(&run.phase)
            {
                pending.remove(run.id.as_str());
            }
        }
    }
}

pub fn drive(artifact: &ProductArtifact) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let env = IsolatedEnv::new();
    std::fs::write(
        env.project_dir().join("child-input.txt"),
        "lane child evidence",
    )
    .unwrap();
    let (gate, arrivals) = ResponseGate::new();
    let (child_notice_gate, child_notice_arrival) = ResponseGate::new();
    let (report_notice_gate, report_notice_arrival) = ResponseGate::new();
    let lead = MockProvider::scripted(vec![
        tool(
            "open",
            "lane_open",
            json!({"brief":{
                "objective":"recover this unopened lane",
                "inputs":[], "deliverables":["result"], "acceptance_criteria":["checked"],
                "constraints":[], "workspace":{}, "report_policy":"attached"
            }}),
        ),
        text("lead-before").with_gate(gate.clone()),
        text("lead-received-report").with_gate(report_notice_gate.clone()),
    ]);
    let lane = MockProvider::scripted(vec![
        tool(
            "delegate-child",
            "delegate",
            json!({"target":{"role":"worker"}, "prompt":"read child-input.txt",
                "background":true}),
        ),
        text("lane-before").with_gate(gate.clone()),
        // The successor delivers the background child completion as ordinary
        // parent input. Report from that recovered run, without opening its GUI.
        tool("restored-report", "lane_report", json!({
            "status":"ready", "summary":"ready after graph recovery", "artifact_refs":["child-input.txt"]
        })).with_gate(child_notice_gate.clone()),
        text("lane-after-report"),
    ]);
    let worker = MockProvider::scripted(vec![
        tool("child-read", "read", json!({"file_path":"child-input.txt"})).with_gate(gate.clone()),
        text("child-after"),
    ]);
    let _release = Release(gate.clone());
    let _release_child_notice = Release(child_notice_gate.clone());
    let _release_report_notice = Release(report_notice_gate.clone());
    // Providers deliberately refuse HTTP redirects (credential safety), so
    // forward bytes locally instead. Cache accounting remains input-derived
    // and comes from the same mock used by the other public restart scenarios.
    let router = rt.block_on(Router::start(
        lead.base_url(),
        lane.base_url(),
        worker.base_url(),
    ));
    env.write_product_config(&router.base, KEY_ENV);
    let mut product = ProductLaunch::new(artifact, &env).start().unwrap();
    let ready = product.wait_ready().unwrap();
    let pid = product.pid();
    let (root, lane_record, child, old_epoch, connections) = rt.block_on(async {
        let desktop = connect(&ready.socket, "lane-desktop", LocalSurface::Desktop).await;
        let root = desktop
            .command(chat("open-lane", "start the lane"))
            .await
            .unwrap()
            .run_id
            .unwrap();
        // Each backend reports arrival after recording its HTTP request. This
        // proves all three runs are inside their provider turn before restart.
        for _ in 0..3 {
            arrivals.recv().unwrap();
        }
        let QueryReply::Lanes(LanesReply::Lanes(list)) = desktop
            .query(Query::Lanes(LanesQuery::ListLanes(ListLanes {
                lead_thread: workbench_protocol::ids::ThreadId::new("restart-thread").unwrap(),
                policy: None,
                detached: false,
                page: PageRequest::default(),
            })))
            .await
            .unwrap()
        else {
            panic!("lane listing")
        };
        assert_eq!(list.items.len(), 1);
        let lane = list.items.into_iter().next().unwrap();
        let events = subscribe(&desktop).await;
        let child = events
            .started()
            .initial_page
            .as_ref()
            .unwrap()
            .items
            .iter()
            .find_map(|item| match item {
                SnapshotItem::Run(run) if run.parent_run.as_ref() == lane.root_run.as_ref() => {
                    Some(run.id.clone())
                }
                _ => None,
            })
            .expect("the live lane child is visible");
        let cli = connect(&ready.socket, "lane-cli", LocalSurface::Cli).await;
        let admin = RestartAdmin::new(&cli);
        let old_epoch = cli.server_hello().instance_epoch;
        let binding = plan_restart(
            &admin.targets().await.unwrap(),
            &TargetSelector::Current,
            AdminId::new("lane-restart").unwrap(),
        )
        .unwrap();
        admin
            .send(request_restart(
                binding,
                confirm(false, true, false).unwrap(),
            ))
            .await
            .expect("public restart accepts the unopened lane and its child");
        // The sealed child process exposes no restart phase event/watch. Await
        // existing public status replies until the runtime gate has closed;
        // yielding lets the coordinator advance without elapsed-time assertions.
        let status = loop {
            let status = admin.status().await.unwrap();
            use workbench_protocol::namespaces::lifecycle::RestartPhase;
            match status.status.phase {
                RestartPhase::Requested | RestartPhase::PreparingDrains => {
                    tokio::task::yield_now().await;
                }
                RestartPhase::WaitingSafePoint => break status,
                _ => panic!("restart left preparation unexpectedly: {status:?}"),
            }
        };
        for run in [&root, lane.root_run.as_ref().unwrap(), &child] {
            assert!(
                status.status.blockers.iter().any(|b| &b.run == run),
                "missing held run {run}: {status:?}"
            );
        }
        assert_eq!(cli.server_hello().instance_epoch, old_epoch);
        gate.release();
        (root, lane, child, old_epoch, (desktop, cli, events))
    });
    // The external stdout source cannot publish an in-process Notify; reuse the
    // product harness's supervised process/event watchdog for its Ready line.
    let (line, _) = product
        .process
        .wait_for(READY_WATCHDOG, |line| {
            serde_json::from_str::<Value>(line)
                .ok()
                .filter(|v| v["status"] == "ready")
        })
        .unwrap_or_else(|e| {
            let status = rt.block_on(async {
                let client = connect(&ready.socket, "lane-timeout-diag", LocalSurface::Cli).await;
                RestartAdmin::new(&client).status().await
            });
            panic!(
                "successor Ready: {e:?}\n{status:?}\n{}",
                product.process.stderr()
            )
        });
    drop(connections);
    assert_eq!(product.pid(), pid);
    assert!(observe::same_file(
        std::path::Path::new(&format!("/proc/{pid}/exe")),
        &artifact.path,
    ));
    rt.block_on(async {
        let socket = std::path::PathBuf::from(line["socket"].as_str().unwrap());
        let desktop = connect(&socket, "lane-successor", LocalSurface::Desktop).await;
        assert!(desktop.server_hello().instance_epoch > old_epoch);
        let lane_run = lane_record.root_run.as_ref().unwrap();
        // The child completion has reached the original lane and caused its
        // ordinary next turn. The successor is connected before it may report.
        child_notice_arrival.recv().unwrap();
        child_notice_gate.release();
        // The recovered lane reports on its child-completion turn, without any
        // GUI input or PR A's binding/materialization path. Await the actual lead
        // notification turn as well, rather than assuming when outbox delivery runs.
        report_notice_arrival.recv().unwrap();
        let mut events = subscribe(&desktop).await;
        report_notice_gate.release();
        phases(&desktop, &mut events, BTreeMap::from([
            (root.to_string(), RunPhase::WaitingForUser),
            (lane_run.to_string(), RunPhase::WaitingForUser),
            (child.to_string(), RunPhase::Done),
        ]), true).await;
        let QueryReply::Lanes(LanesReply::LaneClosePreview(preview)) = desktop.query(
            Query::Lanes(LanesQuery::GetLaneClosePreview { lane_id: lane_record.lane_id.clone() })
        ).await.unwrap() else { panic!("close preview") };
        assert_eq!(preview.ready.as_ref().unwrap().summary, "ready after graph recovery");
        let (close_gate, close_arrived) = ResponseGate::new();
        let _release_close = Release(close_gate.clone());
        lead.0.enqueue([
            tool("restored-close", "lane_close", json!({
                "lane_id":lane_record.lane_id, "ready_seq":preview.ready.unwrap().report_seq.get(),
                "expected_management_revision":preview.management_revision.get(),
                "expected_request_revision":preview.request_revision.get(), "note":"checked after recovery"
            })),
            text("lead-closed").with_gate(close_gate.clone()), text("lead-close-notice"),
        ]);
        events = subscribe(&desktop).await;
        assert_eq!(desktop.command(chat("close", "close the recovered lane")).await.unwrap().run_id.as_ref(), Some(&root));
        close_arrived.recv().unwrap();
        close_gate.release();
        phases(&desktop, &mut events, BTreeMap::from([(root.to_string(), RunPhase::WaitingForUser)]), false).await;
        let QueryReply::Lanes(LanesReply::Lane(detail)) = desktop.query(
            Query::Lanes(LanesQuery::GetLane { lane_id: lane_record.lane_id.clone() })
        ).await.unwrap() else { panic!("lane detail") };
        assert!(detail.summary.current_close.is_some(), "{detail:?}");
        assert_eq!(detail.summary.root_run.as_ref(), Some(lane_run));
        assert_eq!(detail.summary.generation, lane_record.generation, "no ordinary lane re-launch");
    });
    for (provider, minimum) in [(&lead, 4), (&lane, 4), (&worker, 2)] {
        let requests = provider.chat_requests();
        assert!(requests.len() >= minimum, "{:?}", requests.len());
        for pair in requests.windows(2) {
            assert_append_only(CacheProtocol::OpenAi, &pair[0].body, &pair[1].body).unwrap();
        }
    }
    assert_eq!(
        worker.chat_requests().len(),
        2,
        "no replay of the captured child turn"
    );
    let config = core_instance::ConfigIdentity::new(&env.config_dir()).unwrap();
    assert!(
        !core_instance::LaunchGuard::acquire(&config)
            .unwrap()
            .lifetime_available()
            .unwrap()
    );
    assert!(!observe::descendants(pid).iter().any(|p| {
        p.exe
            .as_deref()
            .is_some_and(|exe| observe::same_file(exe, &artifact.path))
    }));
    let (report, footprint) = product.stop();
    assert!(report.success(), "{:?} {}", report.status, report.stderr);
    assert!(report.orphans.is_empty());
    drop(env);
    assert!(footprint.remains().is_empty());
}
