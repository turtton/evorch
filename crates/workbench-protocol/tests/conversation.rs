use workbench_protocol::{
    commands::{Command, Delegation},
    conversation::{ConversationCommand, MAX_IMAGE_BASE64_BYTES},
    framing::encode_frame,
    handshake::Capability,
    ids::*,
};

#[test]
fn conversation_has_one_capability_and_rejects_client_authority_and_paths() {
    let wire = serde_json::json!({"type":"conversation","payload":{"type":"upload_image","payload":{"project":"p","thread":"t","mime_type":"image/png","data":"AA==","path":"/etc/passwd"}}});
    assert!(serde_json::from_value::<Command>(wire).is_err());
    for extra in ["provenance", "trust", "force", "owner_id"] {
        let mut wire = serde_json::json!({"type":"conversation","payload":{"type":"prepare_existing_thread","payload":{"project":"p","thread":"t"}}});
        wire["payload"]["payload"][extra] = serde_json::json!(true);
        assert!(serde_json::from_value::<Command>(wire).is_err(), "{extra}");
    }
    let upload = |len| {
        Command::Conversation(ConversationCommand::UploadImage {
            project: ProjectId::new("p").unwrap(),
            thread: ThreadId::new("t").unwrap(),
            mime_type: "image/png".into(),
            data: "A".repeat(len),
        })
    };
    let largest = upload(MAX_IMAGE_BASE64_BYTES);
    largest.validate().unwrap();
    assert_eq!(largest.required_capability(), Capability::Conversation);
    encode_frame(&largest, workbench_protocol::limits::DEFAULT_FRAME_BYTES).unwrap();
    assert!(upload(MAX_IMAGE_BASE64_BYTES + 1).validate().is_err());
    let team = Delegation::Team {
        value: "review this change".into(),
    };
    assert_eq!(
        serde_json::from_value::<Delegation>(serde_json::to_value(&team).unwrap()).unwrap(),
        team
    );
}
