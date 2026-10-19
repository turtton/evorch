//! A user operating the preview pane against a real CEF runtime: native
//! popups, JS dialogs and copies. Ignored by default like `offline_isolation`;
//! see `docs/browser.md` for how to run it.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use browser_cef::clipboard::Clipboard;
use browser_cef::{
    AgentPage, ArtifactContext, CefRuntime, CefView, Dialogs, OsrBrowser, RuntimeConfig, Status,
};
use egui::{Event, Modifiers, PointerButton, Rect, Shape, Vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

const PAGE: &str = r#"<!doctype html><title>ready</title>
<style>
body { margin: 0; }
.at { position: absolute; left: 20px; width: 200px; height: 32px; font: 16px sans-serif; }
</style>
<select id="pick" class="at" style="top: 20px">
  <option>one</option><option>two</option><option>three</option>
</select>
<button id="confirm" class="at" style="top: 80px">confirm</button>
<button id="prompt" class="at" style="top: 130px">prompt</button>
<button id="alert" class="at" style="top: 180px">alert</button>
<input id="exec" class="at" style="top: 230px" value="exec text">
<button id="exec-copy" class="at" style="top: 280px">execCommand</button>
<button id="write" class="at" style="top: 330px">writeText</button>
<button id="read" class="at" style="top: 380px">readText</button>
<input id="keys" class="at" style="top: 430px" value="keyboard text">
<button id="select-keys" class="at" style="top: 480px">select</button>
<script>
const show = (text) => { document.title = text; };
const on = (id, listener) => document.getElementById(id).addEventListener("click", listener);
const field = (id) => { const element = document.getElementById(id); element.focus(); element.select(); };
document.getElementById("pick").addEventListener("change", (event) => show("picked:" + event.target.value));
on("confirm", () => show("confirm:" + confirm("Keep it?")));
on("prompt", () => show("prompt:" + prompt("Name?", "Ada")));
on("alert", () => { alert("Saved"); show("alert:done"); });
on("exec-copy", () => { field("exec"); show("exec:" + document.execCommand("copy")); });
on("write", () => navigator.clipboard.writeText("written text")
  .then(() => show("write:ok"), (error) => show("write:" + error.name)));
on("read", () => navigator.clipboard.readText()
  .then((text) => show("read:" + text), (error) => show("read:" + error.name)));
on("select-keys", () => { field("keys"); show("selected"); });
</script>"#;

fn control_centre(top: f32) -> Vec2 {
    Vec2::new(120.0, top + 16.0)
}

#[derive(Default)]
struct Copies(Mutex<Vec<String>>);

impl Copies {
    fn taken(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

impl Clipboard for Copies {
    fn write_text(&self, text: String) {
        self.0.lock().unwrap().push(text);
    }
}

struct Pane {
    view: CefView,
    rect: Rect,
}

fn harness(copies: Arc<Copies>) -> Harness<'static, Pane> {
    let context = ArtifactContext::create(browser_cef::offline::ArtifactBundle::html(PAGE))
        .expect("artifact context");
    let browser = OsrBrowser::create(
        context,
        &ArtifactContext::url("index.html"),
        [800, 600],
        1.0,
        Dialogs::Ask,
    )
    .expect("browser");
    let pane = Pane {
        view: CefView::new(browser).with_clipboard(copies),
        rect: Rect::NOTHING,
    };
    let mut harness = Harness::builder()
        .with_size(Vec2::new(800.0, 600.0))
        .build_ui_state(
            |ui, pane: &mut Pane| pane.rect = pane.view.ui(ui).rect,
            pane,
        );
    harness.step();
    let browser = harness.state().view.browser();
    browser
        .wait_for(|status| status.title == "ready" && !status.loading && browser.frame_seq() > 0);
    harness
}

fn wait(harness: &Harness<'_, Pane>, predicate: impl FnMut(&Status) -> bool) {
    harness.state().view.browser().wait_for(predicate);
}

fn title(harness: &Harness<'_, Pane>) -> String {
    harness.state().view.browser().status().title.clone()
}

fn click(harness: &mut Harness<'_, Pane>, at: Vec2) {
    let pos = harness.state().rect.min + at;
    harness.event(Event::PointerMoved(pos));
    for pressed in [true, false] {
        harness.event(Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        });
    }
    harness.step();
}

fn mesh_bounds(harness: &Harness<'_, Pane>) -> Vec<Rect> {
    harness
        .output()
        .shapes
        .iter()
        .filter_map(|clipped| match &clipped.shape {
            Shape::Mesh(mesh) => Some(mesh.calc_bounds()),
            _ => None,
        })
        .collect()
}

fn select_popup_is_drawn_over_the_pane_and_takes_clicks(harness: &mut Harness<'_, Pane>) {
    click(harness, control_centre(20.0));
    let browser = harness.state().view.browser();
    browser.wait_for(|status| status.popup.is_some() && browser.popup_frame_seq() > 0);
    harness.step();

    let popup = harness
        .state()
        .view
        .browser()
        .status()
        .popup
        .expect("popup");
    let shown = popup.shown;
    let origin = harness.state().rect.min;
    let expected = Rect::from_min_size(
        origin + Vec2::new(shown.x as f32, shown.y as f32),
        Vec2::new(shown.width as f32, shown.height as f32),
    );
    let meshes = mesh_bounds(harness);
    assert!(
        meshes.contains(&expected),
        "no popup image at {expected:?} among {meshes:?}"
    );

    // Three options: the bottom sixth of the list is inside the last one.
    let last = Vec2::new(
        shown.x as f32 + shown.width as f32 / 2.0,
        shown.y as f32 + shown.height as f32 * 5.0 / 6.0,
    );
    click(harness, last);
    wait(harness, |status| {
        status.title == "picked:three" && status.popup.is_none()
    });
    harness.step();
    let meshes = mesh_bounds(harness);
    assert!(!meshes.contains(&expected), "the popup stayed: {meshes:?}");
}

fn answer(harness: &mut Harness<'_, Pane>, button: &str) {
    wait(harness, |status| status.dialog.is_some());
    harness.step();
    harness.get_by_label(button).click();
    harness.step();
}

fn dialogs_ask_the_user_and_return_the_answer(harness: &mut Harness<'_, Pane>) {
    click(harness, control_centre(80.0));
    answer(harness, "Cancel");
    wait(harness, |status| status.title.starts_with("confirm:"));
    assert_eq!(title(harness), "confirm:false");

    click(harness, control_centre(130.0));
    wait(harness, |status| status.dialog.is_some());
    harness.step();
    harness.event(Event::Text(" Lovelace".into()));
    harness.step();
    harness.get_by_label("OK").click();
    harness.step();
    wait(harness, |status| status.title.starts_with("prompt:"));
    assert_eq!(title(harness), "prompt:Ada Lovelace");

    click(harness, control_centre(180.0));
    assert!(
        harness.query_by_label("Saved").is_none(),
        "the alert showed before the page opened it"
    );
    wait(harness, |status| status.dialog.is_some());
    harness.step();
    assert!(harness.query_by_label("Saved").is_some());
    harness.get_by_label("OK").click();
    harness.step();
    wait(harness, |status| status.title == "alert:done");
    assert!(harness.state().view.browser().status().dialog.is_none());
}

fn wait_for_copies(harness: &mut Harness<'_, Pane>, copies: &Copies, count: usize) -> Vec<String> {
    wait(harness, |status| {
        copies.taken().len() + status.copied.len() >= count
    });
    harness.step();
    copies.taken()
}

fn copies_reach_the_host_clipboard_but_reads_are_denied(
    harness: &mut Harness<'_, Pane>,
    copies: &Copies,
) {
    click(harness, control_centre(280.0));
    wait(harness, |status| status.title.starts_with("exec:"));
    assert_eq!(title(harness), "exec:true");
    assert_eq!(wait_for_copies(harness, copies, 1), ["exec text"]);

    click(harness, control_centre(330.0));
    wait(harness, |status| status.title.starts_with("write:"));
    assert_eq!(title(harness), "write:ok");
    assert_eq!(
        wait_for_copies(harness, copies, 2),
        ["exec text", "written text"]
    );

    click(harness, control_centre(480.0));
    wait(harness, |status| status.title == "selected");
    harness.event(Event::Copy);
    harness.step();
    assert_eq!(
        wait_for_copies(harness, copies, 3),
        ["exec text", "written text", "keyboard text"]
    );

    click(harness, control_centre(380.0));
    wait(harness, |status| status.title.starts_with("read:"));
    assert_eq!(title(harness), "read:NotAllowedError");
}

fn agent_page_dialogs_are_settled_and_reported() {
    let page = AgentPage::open(
        br#"<!doctype html><p id="out">waiting</p><script>
alert("Hello");
const kept = confirm("Keep?");
const name = prompt("Name?", "Ada");
document.getElementById("out").textContent = "confirm=" + kept + " prompt=" + name;
</script>"#
            .to_vec(),
        // nextest's slow-timeout bounds the page; it has no other deadline.
        Duration::MAX,
    )
    .expect("agent page");

    assert_eq!(
        page.take_dialogs(),
        [
            r#"alert "Hello" (closed)"#,
            r#"confirm "Keep?" (dismissed)"#,
            r#"prompt "Name?" (dismissed)"#,
        ]
    );
    assert!(page.take_dialogs().is_empty());
    let snapshot = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(page.agent().snapshot())
        .expect("snapshot");
    assert!(
        snapshot.text.contains("confirm=false prompt=null"),
        "{}",
        snapshot.text
    );
    page.close();
}

/// CEF initializes once per process, so the scenarios share one runtime.
#[test]
#[ignore = "needs the CEF runtime on LD_LIBRARY_PATH; see docs/browser.md"]
fn the_preview_pane_handles_popups_dialogs_and_copies() {
    let runtime = CefRuntime::initialize(RuntimeConfig {
        helper_path: PathBuf::from(env!("CARGO_BIN_EXE_evorch-cef-helper")),
        ..RuntimeConfig::offline()
    })
    .expect("CEF runtime");

    let copies = Arc::new(Copies::default());
    let mut pane = harness(Arc::clone(&copies));
    select_popup_is_drawn_over_the_pane_and_takes_clicks(&mut pane);
    dialogs_ask_the_user_and_return_the_answer(&mut pane);
    copies_reach_the_host_clipboard_but_reads_are_denied(&mut pane, &copies);
    pane.state().view.browser().close();
    drop(pane);
    agent_page_dialogs_are_settled_and_reported();

    drop(runtime);
}
