use super::{
    models::{catalog, parameter, sent_routes},
    *,
};

#[tokio::test]
async fn blocked_enum_and_boolean_variants_never_enter_roster_or_default_route() {
    for has_usable_family in [true, false] {
        let server = MockServer::start().await;
        let definition = |id, kind, blocked_tag, blocked_value, allowed_value| {
            Proto::new().string(1, id).message(
                4,
                Proto::new().message(
                    kind,
                    Proto::new()
                        .message(
                            1,
                            Proto::new()
                                .string(1, blocked_value)
                                .integer(blocked_tag, 1),
                        )
                        .message(
                            1,
                            Proto::new()
                                .string(1, allowed_value)
                                .integer(blocked_tag, 0),
                        ),
                ),
            )
        };
        let variant = |slug, id, value| {
            Proto::new()
                .string(11, slug)
                .message(1, parameter(id, value))
        };
        let rich = Proto::new().message(
            2,
            Proto::new()
                .string(1, "family")
                .integer(5, 1)
                .message(29, definition("reasoning", 2, 4, "xhigh", "low"))
                .message(29, definition("thinking", 1, 6, "true", "false"))
                // A blocked variant flagged default must not influence the bare family.
                .message(
                    30,
                    variant("family-xhigh", "reasoning", "xhigh").integer(5, 1),
                )
                .message(30, variant("family-thinking", "thinking", "true"))
                .message(30, variant("family-low", "reasoning", "low"))
                .message(30, variant("family-toggle-off", "thinking", "false"))
                // Matching is by parameter ID as well as value; missing definitions
                // do not imply blocked. Keep this independent future parameter usable.
                .message(30, variant("family-unknown", "context", "xhigh")),
        );
        let usable = if has_usable_family {
            Proto::new().message(1, Proto::new().string(1, "family"))
        } else {
            Proto::new()
        };
        catalog(&server, usable, rich).await;
        let client = client(&server);
        let listed = client
            .list_models(&ProviderAuth::new(""))
            .await
            .unwrap()
            .unwrap();
        let mut expected = vec!["family-low", "family-toggle-off", "family-unknown"];
        if has_usable_family {
            expected.insert(0, "family");
        }
        assert_eq!(listed, expected);
        for id in listed {
            let mut request = request();
            request.model = id;
            client.send(&ProviderAuth::new(""), &request).await.unwrap();
        }
        let sent = sent_routes(&server).await;
        for (details, requested, parameters, _) in &sent {
            assert_eq!(requested, "family");
            let expected_parameter = match details.as_str() {
                "family" | "family-low" => ("reasoning".into(), "low".into()),
                "family-toggle-off" => ("thinking".into(), "false".into()),
                "family-unknown" => ("context".into(), "xhigh".into()),
                _ => panic!("blocked variant appeared on wire"),
            };
            assert_eq!(parameters, &[expected_parameter]);
        }
        assert_eq!(sent.len(), expected.len());
    }
}
