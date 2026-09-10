use super::parse_role;
use agents::Role;

#[test]
fn additional_role_names_are_case_insensitive() {
    for (name, expected) in [
        ("planner", Role::Planner),
        ("ORACLE", Role::Oracle),
        ("multimodal_looker", Role::MultimodalLooker),
        ("MultimodalLooker", Role::MultimodalLooker),
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
