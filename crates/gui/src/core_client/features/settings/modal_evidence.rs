//! Offscreen PNGs of Routing and Agent Roles for review: a file shaped like the mocks, with
//! edits left unsaved in each section. Ignored by default (it needs a GPU adapter); the PNGs
//! go to `EVORCH_SETTINGS_EVIDENCE_DIR`, else `target/gui-evidence/model-settings`.
use super::*;
use crate::core_client::features::execution::tests::Screen;
use crate::evidence::{AdapterPolicy, Decision, decide};
use crate::headless::{CapturedFrame, OffscreenError};
use egui::accesskit::Role;
use egui_kittest::{Harness, kittest::Queryable};

const SHOWCASE: &str = r#"version = 3
[providers.codex-main]
type = "openai-compatible"
base_url = "http://localhost:1/v1"
api_key_env = "KEY"
models = ["gpt-5.5", "gpt-5.5-mini", "gpt-image-2"]
default_model = "gpt-5.5"

[providers.claude-max]
type = "openai-compatible"
base_url = "http://localhost:2/v1"
api_key_env = "KEY"
models = ["opus", "sonnet"]
default_model = "opus"

[providers.local-llm]
type = "openai-compatible"
base_url = "http://localhost:3/v1"
api_key_env = "KEY"
models = ["qwen3"]
default_model = "qwen3"

[routing.routes]
orchestrator = [{ profile = "codex-main", model = "gpt-5.5" }, { profile = "claude-max", model = "opus" }]
explorer = [{ profile = "codex-main", model = "gpt-5.5-mini" }]
worker = [{ profile = "codex-main" }, { profile = "claude-max", model = "sonnet", auto = true }]
reviewer = [{ profile = "claude-max", model = "opus" }]
advisor = [{ profile = "claude-max", model = "opus" }]
image = [{ profile = "codex-main", model = "gpt-image-2" }]
cheap = [{ profile = "local-llm", model = "qwen3" }]

[agents.orchestrator]
models = [{ route = "orchestrator" }]

[agents.worker]
models = [{ route = "worker" }, { route = "cheap" }]

[agents.worker.categories.deep]
models = [{ route = "deep-route" }]
generation = { max_tokens = 4000 }

[agents.worker.categories.quick]
models = [{ route = "cheap" }]

[agents.reviewer]
models = [{ route = "reviewer" }]

[agents.roles.advisor]
models = [{ route = "advisor" }]

[agents.roles.image_generator]
models = [{ route = "image" }]
"#;

fn evidence_dir() -> std::path::PathBuf {
    std::env::var_os("EVORCH_SETTINGS_EVIDENCE_DIR").map_or_else(
        || {
            std::path::Path::new(&std::env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"))
                .join("../../target/gui-evidence/model-settings")
        },
        Into::into,
    )
}

/// Renders the screen and saves it as `name`.png; without an adapter it skips or fails as
/// `EVORCH_REQUIRE_ADAPTER` says.
fn capture(screen: &mut Harness<'static, Screen>, name: &str) {
    let rendered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| screen.render()));
    let frame = match rendered {
        Ok(Ok(image)) => {
            let (width, height) = image.dimensions();
            Ok(CapturedFrame {
                width,
                height,
                rgba: image.into_raw(),
            })
        }
        Ok(Err(message)) => Err(message),
        Err(_) => Err("no adapter found (render panicked)".to_owned()),
    }
    .map_err(|message| {
        if message.to_ascii_lowercase().contains("no adapter found") {
            OffscreenError::AdapterUnavailable(message)
        } else {
            OffscreenError::Render(message)
        }
    });
    match decide(AdapterPolicy::from_env(), &frame) {
        Decision::Frame => {
            let dir = evidence_dir();
            std::fs::create_dir_all(&dir).expect("evidence directory");
            let path = dir.join(format!("{name}.png"));
            frame
                .expect("a frame")
                .save_png(&path)
                .expect("the PNG is saved");
            eprintln!("saved {}", path.display());
        }
        Decision::Skip(reason) => eprintln!("offscreen evidence skipped: {reason}"),
        Decision::Fail(message) => panic!("{message}"),
    }
}

/// Runs the frames a change needs; with the real theme a grid keeps asking for one more, so
/// running out of steps is not a failure here.
fn settle(screen: &mut Harness<'static, Screen>) {
    let _ = screen.run_ok();
}

fn press(screen: &mut Harness<'static, Screen>, label: &str) {
    screen.get_by_label(label).scroll_to_me();
    settle(screen);
    screen.get_by_label(label).click();
    settle(screen);
}

fn pick(screen: &mut Harness<'static, Screen>, label: &str) {
    screen
        .get_by_role_and_label(Role::Button, label)
        .scroll_to_me();
    settle(screen);
    screen.get_by_role_and_label(Role::Button, label).click();
    settle(screen);
}

#[test]
#[ignore = "writes PNG review evidence using an offscreen GPU adapter"]
fn capture_routing_and_agent_roles_evidence() {
    let mut screen = loaded_with(SHOWCASE, None);
    screen.set_size(egui::vec2(1280.0, 800.0));
    let ctx = screen.ctx.clone();
    crate::theme::fonts::install(&ctx);
    crate::theme::style::install(&ctx);
    settle(&mut screen);

    settings(&mut screen).open_modal(SettingsTab::Routing);
    settle(&mut screen);
    press(&mut screen, "Route worker");
    press(&mut screen, "Add candidate");
    {
        let routing = settings(&mut screen)
            .modal
            .as_mut()
            .and_then(|modal| modal.routing.as_mut())
            .expect("the routing draft");
        // An unsaved edit of another route that mixes chat and image models.
        routing
            .model
            .routes
            .get_mut("image")
            .expect("the image route")
            .push(crate::model::routing_settings::RouteCandidateConfig {
                profile: "local-llm".into(),
                model: Some("qwen3".into()),
                auto: false,
            });
    }
    settle(&mut screen);
    capture(&mut screen, "routing");

    settings(&mut screen).open_modal(SettingsTab::AgentRoles);
    settle(&mut screen);
    pick(&mut screen, "Worker");
    press(&mut screen, "Edit category deep");
    press(&mut screen, "Temperature");
    capture(&mut screen, "agent-roles-category-edit");

    pick(&mut screen, "Planner");
    capture(&mut screen, "agent-roles-no-route");
}
