use crate::{AgentInvocationContext, AgentModel, Role, RuntimeError};
use providers::{ChatResponse, Message, ToolSpec};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Images {
    calls: AtomicUsize,
    lose_claim: bool,
    team: crate::team_context::TeamContext,
}

#[async_trait::async_trait]
impl AgentModel for Images {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "images".into()
    }

    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        unreachable!()
    }

    fn image_generation_enabled(&self) -> bool {
        true
    }

    async fn generate_image(
        &self,
        _: &AgentInvocationContext,
        _: providers::ImageGenerationRequest,
    ) -> Result<crate::image_generation::GeneratedImage, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.lose_claim {
            self.team
                .board
                .expire(self.team.now().saturating_add(10_000))
                .unwrap();
        }
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(128, 64)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        Ok(crate::image_generation::GeneratedImage {
            response: providers::ImageGenerationResponse {
                png: png.into_inner(),
                width: 128,
                height: 64,
                background: None,
                usage: Default::default(),
                request_id: None,
            },
            profile: "images".into(),
            model: "gpt-image-2".into(),
        })
    }
}

#[tokio::test]
async fn ac9_team_image_claims_guard_both_paths_before_request_and_after_generation() {
    for scenario in [
        "missing",
        "other_worker",
        "expired",
        "original_only",
        "valid",
        "lost_during_generation",
    ] {
        for absolute in [false, true] {
            let workspace = tempfile::tempdir().unwrap();
            std::fs::create_dir(workspace.path().join("images")).unwrap();
            let original = workspace.path().join("images/hero.png");
            let variant = workspace.path().join("images/hero.64.png");
            std::fs::write(&original, b"old original").unwrap();
            std::fs::write(&variant, b"old variant").unwrap();
            let (_cancel, cancel_rx) = tokio::sync::watch::channel(crate::RunInterrupt::None);
            let (runtime, mut state) = super::super::delegate_cleanup_tests::fixture(cancel_rx);
            state
                .shared
                .executor
                .set_default_cwd(workspace.path().into());
            state.task.role = Role::Worker;
            state.task.config.category = Some("visual".into());
            state.task.config.topology =
                crate::CoordinationTopology::DynamicTeam { max_workers: 3 };
            let team = crate::team_context::TeamContext::new(state.task.run_id, 3);
            team.board
                .enqueue(crate::team::TaskSpec {
                    id: "images".into(),
                    paths: vec![
                        if scenario == "original_only" {
                            "images/hero.png"
                        } else {
                            "images"
                        }
                        .into(),
                    ],
                })
                .unwrap();
            if scenario != "missing" {
                let owner = if scenario == "other_worker" {
                    "another-worker".into()
                } else {
                    state.task.run_id.to_string()
                };
                let now = if scenario == "expired" {
                    team.now().saturating_sub(6_000)
                } else {
                    team.now()
                };
                team.board.claim("images", &owner, now).unwrap();
            }
            let model = Arc::new(Images {
                calls: AtomicUsize::new(0),
                lose_claim: scenario == "lost_during_generation",
                team: team.clone(),
            });
            state.task.config.team = Some(team);
            state.image_model = model.clone();
            let input = json!({
                "prompt": "material",
                "output_path": if absolute { original.clone() } else { "images/hero.png".into() },
                "variant": {"max_side":64,"format":"png"}
            });
            let may_request = matches!(scenario, "valid" | "lost_during_generation");
            assert_eq!(
                state.guard_team_artifact("generate_image", &input).is_ok(),
                may_request,
                "{scenario}"
            );
            // Dispatch also enforces the guard, independently of tool_calls.
            let result = crate::image_generation::dispatch(&mut state, &runtime, input)
                .await
                .result;
            assert_eq!(
                model.calls.load(Ordering::SeqCst),
                usize::from(may_request),
                "{scenario}"
            );
            assert_eq!(
                result.is_error,
                scenario != "valid",
                "{scenario}: {}",
                result.content
            );
            if scenario == "valid" {
                assert!(std::fs::read(&original).unwrap().starts_with(b"\x89PNG"));
                assert!(std::fs::read(&variant).unwrap().starts_with(b"\x89PNG"));
            } else {
                assert_eq!(
                    std::fs::read(&original).unwrap(),
                    b"old original",
                    "{scenario}"
                );
                assert_eq!(
                    std::fs::read(&variant).unwrap(),
                    b"old variant",
                    "{scenario}"
                );
            }
            assert_eq!(
                std::fs::read_dir(workspace.path().join("images"))
                    .unwrap()
                    .count(),
                2
            );
            runtime.cancel(state.task.run_id).unwrap();
        }
    }
}
