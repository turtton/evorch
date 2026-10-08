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
        (
            "claude-opus-high",
            "claude-opus-high",
            vec![("reasoning", "low")],
            true,
        ),
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
