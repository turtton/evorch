//! Terminal ペインの描画。

use crate::model::terminal::TerminalBuffer;
use crate::pty::PtySession;
use crate::theme::tokens::palette;
use crate::theme::widgets::{empty_state, surface_frame};

/// 端末バッファと一行入力を描画します。
/// Enter が押されたら `PtySession` へ入力行を送信します。
pub fn terminal_pane(
    ui: &mut egui::Ui,
    buffer: &TerminalBuffer,
    input: &mut String,
    pty: &mut Option<PtySession>,
) {
    surface_frame(palette().INPUT).show(ui, |ui| {
        ui.vertical(|ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    let lines = buffer.lines();
                    if lines.iter().all(String::is_empty) {
                        let (title, hint) = match pty {
                            Some(_) => (
                                "Waiting for shell output",
                                "Type a command below and press Enter to run it.",
                            ),
                            None => (
                                "Waiting for terminal connection",
                                "Shell output will appear when a terminal session is connected.",
                            ),
                        };
                        empty_state(ui, title, hint, None);
                    }
                    for line in lines {
                        ui.monospace(&line);
                    }
                });

            ui.horizontal(|ui| {
                let response = ui.text_edit_singleline(input);
                let pressed_enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                if pressed_enter && response.has_focus() {
                    if let Some(pty) = pty {
                        let _ = pty.write(input.as_bytes());
                        let _ = pty.write(b"\n");
                    }
                    input.clear();
                    response.request_focus();
                }
            });
        });
    });
}
