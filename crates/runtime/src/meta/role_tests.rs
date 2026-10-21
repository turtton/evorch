use super::parse_role;
use agents::Role;

#[test]
fn additional_role_names_are_case_insensitive() {
    for (name, expected) in [
        ("web_researcher", Role::WebResearcher),
        ("WEB_RESEARCHER", Role::WebResearcher),
        ("WebResearcher", Role::WebResearcher),
        ("planner", Role::Planner),
        ("ADVISOR", Role::Advisor),
        ("ORACLE", Role::Advisor),
        ("oracle", Role::Advisor),
        ("multimodal_looker", Role::MultimodalLooker),
        ("MultimodalLooker", Role::MultimodalLooker),
        ("evaluator", Role::Evaluator),
        ("Evaluator", Role::Evaluator),
    ] {
        assert_eq!(parse_role(name), Ok(expected));
    }
}

#[test]
fn unknown_role_is_structured_and_preserves_original_name() {
    let error = parse_role("CustomRole").expect_err("closed roles");
    let value: serde_json::Value = serde_json::from_str(&error).expect("JSON error");
    assert_eq!(value["code"], "unknown_role");
    assert_eq!(value["role"], "CustomRole");
}

#[test]
fn persisted_role_aliases_deserialize_and_serialize_canonically() {
    for name in ["Oracle", "oracle", "Advisor"] {
        let role: Role = serde_json::from_value(serde_json::json!(name)).unwrap();
        assert_eq!(role, Role::Advisor);
        assert_eq!(serde_json::to_value(role).unwrap(), "Advisor");
    }
}
