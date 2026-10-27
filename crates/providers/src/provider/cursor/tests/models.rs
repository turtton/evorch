use super::*;

pub(super) async fn catalog(server: &MockServer, usable: Proto, available: Proto) {
    for (endpoint, body) in [
        ("/agent.v1.AgentService/GetUsableModels", usable),
        ("/aiserver.v1.AiService/AvailableModels", available),
    ] {
        Mock::given(path(endpoint))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body.0, "application/proto"))
            .mount(server)
            .await;
    }
    Mock::given(path("/aiserver.v1.BidiService/BidiAppend"))
        .respond_with(ResponseTemplate::new(200))
        .mount(server)
        .await;
    Mock::given(path("/agent.v1.AgentService/RunSSE"))
        .respond_with(response(vec![end()]))
        .mount(server)
        .await;
}
pub(super) fn parameter(id: &str, value: &str) -> Proto {
    Proto::new().string(1, id).string(2, value)
}
fn route(run: &[u8]) -> (String, String, Vec<(String, String)>, bool) {
    let details = child(run, 3);
    let requested = child(run, 9);
    let max_mode = wire::integer(requested, 2).unwrap() == Some(1);
    assert_eq!(
        wire::integer(details, 7).unwrap(),
        Some(u64::from(max_mode))
    );
    (
        wire::string(details, 1).unwrap(),
        wire::string(requested, 1).unwrap(),
        wire::fields(requested)
            .unwrap()
            .into_iter()
            .filter(|f| f.number == 3 && f.wire == 2)
            .map(|f| {
                (
                    wire::string(f.data, 1).unwrap(),
                    wire::string(f.data, 2).unwrap(),
                )
            })
            .collect(),
        max_mode,
    )
}
pub(super) async fn sent_routes(
    server: &MockServer,
) -> Vec<(String, String, Vec<(String, String)>, bool)> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().ends_with("/BidiAppend"))
        .filter_map(|r| wire::nested(child(&r.body, 4), 1).unwrap())
        .map(route)
        .collect()
}

#[tokio::test]
async fn listed_legacy_effort_siblings_and_rich_routes_send_distinct_wire_identities() {
    let server = MockServer::start().await;
    let ids = [
        "gpt-5.4",
        "gpt-5.4-high",
        "gpt-5.4-none",
        "gpt-5.4-xhigh-fast",
        "claude-opus-high",
        "composer-2.5",
        "rich-sibling",
    ];
    let usable = ids.iter().fold(Proto::new(), |p, id| {
        p.message(
            1,
            Proto::new()
                .string(1, id)
                .integer(7, u64::from(*id == "claude-opus-high")),
        )
    });
    let available = Proto::new().message(
        2,
        Proto::new()
            .string(1, "rich-base")
            .string(18, "not-the-requested-id")
            .integer(5, 1)
            .message(
                30,
                Proto::new()
                    .string(11, "rich-sibling")
                    .integer(3, 1)
                    .message(1, parameter("reasoning", "xhigh"))
                    .message(1, parameter("context", "200k"))
                    .message(1, parameter("fast", "true")),
            ),
    );
    catalog(&server, usable, available).await;
    let client = client(&server);
    let listed = client
        .list_models(&ProviderAuth::new(""))
        .await
        .unwrap()
        .unwrap();
    let mut expected_ids: Vec<_> = ids.iter().map(|s| s.to_string()).collect();
    expected_ids.sort();
    assert_eq!(listed, expected_ids);
    for id in ids {
        let mut request = request();
        request.model = id.into();
        request.reasoning_effort = Some("low".into());
        client.send(&ProviderAuth::new(""), &request).await.unwrap();
    }
    let expected = [
        ("gpt-5.4", "gpt-5.4", vec![("reasoning", "low")], false),
        (
            "gpt-5.4-high",
            "gpt-5.4",
            vec![("reasoning", "high")],
            false,
        ),
        ("gpt-5.4-none", "gpt-5.4", vec![], false),
        (
            "gpt-5.4-xhigh-fast",
            "gpt-5.4-fast",
            vec![("reasoning", "xhigh")],
            false,
        ),
        // A legacy non-OpenAI ID passes through: its name may already fix an effort.
        ("claude-opus-high", "claude-opus-high", vec![], true),
        (
            "composer-2.5",
            "composer-2.5",
            vec![("fast", "false")],
            false,
        ),
        (
            "rich-sibling",
            "rich-base",
            vec![
                ("reasoning", "xhigh"),
                ("context", "200k"),
                ("fast", "true"),
            ],
            true,
        ),
    ]
    .into_iter()
    .map(|(details, requested, params, max)| {
        (
            details.to_string(),
            requested.to_string(),
            params
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
            max,
        )
    })
    .collect::<Vec<_>>();
    assert_eq!(sent_routes(&server).await, expected);
}

#[tokio::test]
async fn rich_only_catalog_exposes_sendable_variants_and_resolves_colliding_slugs() {
    let server = MockServer::start().await;
    let rich = Proto::new()
        .message(
            2,
            Proto::new()
                .string(1, "family")
                .string(18, "wrong-id")
                .integer(5, 1)
                .message(
                    30,
                    Proto::new()
                        .string(11, "family-standard")
                        .integer(3, 0)
                        .integer(5, 1)
                        .message(1, parameter("fast", "false")),
                )
                .message(
                    30,
                    Proto::new()
                        .string(11, "family-standard")
                        .string(9, "family-fast")
                        .integer(3, 1)
                        .message(1, parameter("fast", "true")),
                )
                .message(30, Proto::new().message(1, parameter("context", "1m"))),
        )
        .message(
            2,
            Proto::new()
                .string(1, "plain")
                .string(18, "not-plain")
                .integer(5, 1),
        );
    catalog(&server, Proto::new(), rich).await;
    let client = client(&server);
    let listed = client
        .list_models(&ProviderAuth::new(""))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        listed,
        [
            "family-fast",
            "family-standard",
            "family@context=1m",
            "plain"
        ]
    );
    for id in listed {
        let mut request = request();
        request.model = id;
        client.send(&ProviderAuth::new(""), &request).await.unwrap();
    }
    let routes = sent_routes(&server).await;
    assert_eq!(
        routes[0],
        (
            "family-fast".into(),
            "family".into(),
            vec![("fast".into(), "true".into())],
            true
        )
    );
    assert_eq!(
        routes[1],
        (
            "family-standard".into(),
            "family".into(),
            vec![("fast".into(), "false".into())],
            false
        )
    );
    assert_eq!(
        routes[2],
        (
            "family@context=1m".into(),
            "family".into(),
            vec![("context".into(), "1m".into())],
            false
        )
    );
    assert_eq!(routes[3], ("plain".into(), "plain".into(), vec![], false));
}

#[tokio::test]
async fn fresh_client_discovers_default_parameters_and_pins_them_across_catalog_refresh() {
    let server = MockServer::start().await;
    let rich = |effort| {
        Proto::new().message(
            2,
            Proto::new()
                .string(1, "family")
                .integer(5, 1)
                .message(
                    30,
                    Proto::new()
                        .string(11, "family-high")
                        .integer(3, 1)
                        .message(1, parameter("reasoning", "high")),
                )
                .message(
                    30,
                    Proto::new()
                        .string(11, "family-default")
                        .integer(3, 0)
                        .integer(5, 1)
                        .message(1, parameter("reasoning", effort)),
                ),
        )
    };
    Mock::given(path("/aiserver.v1.AiService/AvailableModels"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(rich("medium").0, "application/proto"),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    catalog(
        &server,
        Proto::new().message(1, Proto::new().string(1, "family")),
        rich("low"),
    )
    .await;
    let client = client(&server);
    let mut request = request();
    request.model = "family".into();
    client.send(&ProviderAuth::new(""), &request).await.unwrap();
    client.list_models(&ProviderAuth::new("")).await.unwrap();
    request.messages.push(Message {
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: "Continue".into(),
        }],
    });
    client.send(&ProviderAuth::new(""), &request).await.unwrap();
    request.observation.as_mut().unwrap().run_id = "new-conversation".into();
    client.send(&ProviderAuth::new(""), &request).await.unwrap();
    let routes = sent_routes(&server).await;
    assert_eq!(routes.len(), 3);
    assert_eq!(
        routes[0],
        (
            "family".into(),
            "family".into(),
            vec![("reasoning".into(), "medium".into())],
            false
        )
    );
    assert_eq!(routes[1], routes[0]);
    assert_eq!(
        routes[2],
        (
            "family".into(),
            "family".into(),
            vec![("reasoning".into(), "low".into())],
            false
        )
    );
}

fn observed_client(server: &MockServer) -> (CursorClient, event_bus::EventReceiver) {
    let store = Arc::new(InMemoryCursorTokenStore::new());
    store.save(&token()).unwrap();
    let bus = Arc::new(event_bus::EventBus::new(64));
    let receiver = bus.subscribe();
    let client = CursorClient::new(
        CursorConfig {
            base_url: server.uri(),
            event_bus: Some(bus),
        },
        store,
    )
    .unwrap()
    .with_profile("cursor");
    (client, receiver)
}

/// Route diagnostics and the efforts `RequestStarted` reported, in emission order.
fn route_observations(
    receiver: &mut event_bus::EventReceiver,
) -> (Vec<event_bus::DiagnosticEvent>, Vec<Option<String>>) {
    use futures_util::FutureExt;
    let (mut diagnostics, mut efforts) = (Vec::new(), Vec::new());
    while let Some(Ok(event)) = receiver.recv().now_or_never() {
        match event.kind {
            event_bus::EventKind::Diagnostic(diagnostic)
                if diagnostic.code == event_bus::event::diagnostic_codes::PROVIDER_MODEL_ROUTE =>
            {
                diagnostics.push(diagnostic);
            }
            event_bus::EventKind::Provider(event_bus::ProviderEvent::RequestStarted {
                reasoning_effort,
                ..
            }) => efforts.push(reasoning_effort),
            _ => {}
        }
    }
    (diagnostics, efforts)
}

#[tokio::test]
async fn role_effort_is_not_added_to_a_legacy_model_whose_name_fixes_it() {
    // Given: Cursor lists `claude-fable-5-1-xhigh` only as a legacy usable row, and the
    // role asks for `max`.
    let server = MockServer::start().await;
    let usable = Proto::new().message(1, Proto::new().string(1, "claude-fable-5-1-xhigh"));
    catalog(&server, usable, Proto::new()).await;
    let (client, mut receiver) = observed_client(&server);
    let mut request = request();
    request.model = "claude-fable-5-1-xhigh".into();
    request.reasoning_effort = Some("max".into());
    // When
    client.send(&ProviderAuth::new(""), &request).await.unwrap();
    // Then: the model goes out as named, with no contradicting `reasoning` parameter,
    // and the unapplied effort is reported once as information.
    assert_eq!(
        sent_routes(&server).await,
        [(
            "claude-fable-5-1-xhigh".to_string(),
            "claude-fable-5-1-xhigh".to_string(),
            vec![],
            false
        )]
    );
    let (diagnostics, efforts) = route_observations(&mut receiver);
    assert_eq!(efforts, [None]);
    let [diagnostic] = diagnostics.as_slice() else {
        panic!("{diagnostics:?}");
    };
    assert_eq!(diagnostic.severity, event_bus::DiagnosticSeverity::Info);
    assert_eq!(diagnostic.run_id.as_deref(), Some("cursor-contract"));
    assert!(
        diagnostic
            .detail
            .contains("requested_effort=Some(\"max\") sent_effort=None")
            && diagnostic
                .detail
                .contains("reason=requested reasoning effort not applied"),
        "{}",
        diagnostic.detail
    );
}

#[tokio::test]
async fn sent_effort_comes_from_the_route_and_a_matching_request_reports_nothing() {
    // Given: a rich variant fixed at xhigh, requested once at xhigh and once at low.
    let server = MockServer::start().await;
    let available = Proto::new().message(
        2,
        Proto::new().string(1, "rich-base").message(
            30,
            Proto::new()
                .string(11, "rich-xhigh")
                .message(1, parameter("reasoning", "xhigh")),
        ),
    );
    catalog(&server, Proto::new(), available).await;
    let (client, mut receiver) = observed_client(&server);
    for (prompt, effort) in [("first", "xhigh"), ("second", "low")] {
        let mut request = request();
        request.model = "rich-xhigh".into();
        request.reasoning_effort = Some(effort.into());
        // A new first user message starts a new conversation, so the route resolves again.
        request.messages[1].content = vec![ContentBlock::Text {
            text: prompt.into(),
        }];
        // When
        client.send(&ProviderAuth::new(""), &request).await.unwrap();
    }
    // Then: both requests report the effort on the wire; only the overridden one is noted.
    let (diagnostics, efforts) = route_observations(&mut receiver);
    assert_eq!(efforts, [Some("xhigh".into()), Some("xhigh".into())]);
    let [diagnostic] = diagnostics.as_slice() else {
        panic!("{diagnostics:?}");
    };
    assert!(
        diagnostic
            .detail
            .contains("requested_effort=Some(\"low\") sent_effort=Some(\"xhigh\")"),
        "{}",
        diagnostic.detail
    );
}

#[tokio::test]
async fn unavailable_catalog_is_reported_and_its_guessed_route_is_not_cached() {
    // Given: both catalog RPCs fail with a server error that echoes secret text.
    let server = MockServer::start().await;
    for endpoint in [
        "/agent.v1.AgentService/GetUsableModels",
        "/aiserver.v1.AiService/AvailableModels",
    ] {
        Mock::given(path(endpoint))
            .respond_with(ResponseTemplate::new(503).set_body_string("secret"))
            .mount(&server)
            .await;
    }
    Mock::given(path("/aiserver.v1.BidiService/BidiAppend"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(path("/agent.v1.AgentService/RunSSE"))
        .respond_with(response(vec![end()]))
        .mount(&server)
        .await;
    let (client, mut receiver) = observed_client(&server);
    for prompt in ["first", "second"] {
        let mut request = request();
        request.messages[1].content = vec![ContentBlock::Text {
            text: prompt.into(),
        }];
        // When
        client.send(&ProviderAuth::new(""), &request).await.unwrap();
    }
    // Then: each new conversation asks the catalog again and warns about its guess.
    let catalog_calls = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().ends_with("/GetUsableModels"))
        .count();
    assert_eq!(catalog_calls, 2);
    let (diagnostics, _) = route_observations(&mut receiver);
    assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
    for diagnostic in diagnostics {
        assert_eq!(diagnostic.severity, event_bus::DiagnosticSeverity::Warning);
        assert!(
            diagnostic
                .detail
                .contains("reason=account catalog unavailable (server error)")
                && !diagnostic.detail.contains("secret"),
            "{}",
            diagnostic.detail
        );
    }
}
