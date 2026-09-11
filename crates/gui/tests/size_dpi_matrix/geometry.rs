use egui::Rect;
use gui::{fixture::DemoSource, headless::HeadlessWorkbench};

pub const INPUT: &str = "Message or /command";

pub struct Geometry<'a> {
    pub workbench: &'a mut HeadlessWorkbench<DemoSource>,
    pub context: String,
    pub failures: &'a mut Vec<String>,
}

impl Geometry<'_> {
    pub fn failure(&mut self, label: &str, detail: impl std::fmt::Display) {
        self.failures.push(format!(
            "{} label={label} {detail} screen={:?}",
            self.context,
            self.workbench.screen_rect()
        ));
    }

    pub fn reachable(&mut self, label: &str) -> Vec<Rect> {
        let mut rects = self.workbench.label_rects(label);
        if rects.len() == 1 && !self.workbench.screen_rect().contains_rect(rects[0]) {
            self.workbench.scroll_label_into_view(label);
            self.workbench.run();
            rects = self.workbench.label_rects(label);
        }
        if rects.is_empty() {
            self.failure(label, "rect=missing");
        }
        for rect in &rects {
            if !rect.is_finite() || !rect.is_positive() {
                self.failure(label, format_args!("rect={rect:?} is empty or non-finite"));
            } else if !self.workbench.screen_rect().contains_rect(*rect) {
                self.failure(
                    label,
                    format_args!("rect={rect:?} is off-screen after scroll"),
                );
            }
        }
        rects
    }

    pub fn sidebar(&mut self, label: &str) {
        let rects = self.reachable(label);
        for rect in rects {
            for composer in self.workbench.label_rects(INPUT) {
                if rect.max.x > composer.min.x {
                    self.failure(
                        label,
                        format_args!("rect={rect:?} crosses composer={composer:?}"),
                    );
                }
            }
        }
    }

    pub fn composer(&mut self) {
        let inputs = self.reachable(INPUT);
        let sends = self.reachable("Send");
        for input in inputs {
            for send in &sends {
                if send.max.y < input.min.y || send.min.y > input.max.y || send.intersects(input) {
                    self.failure(
                        "Send",
                        format_args!("rect={send:?} is not beside input={input:?}"),
                    );
                }
            }
        }
    }

    pub fn modal(&mut self) {
        self.reachable("Provider settings");
        for label in ["Name", "Base URL"] {
            let fields = self.reachable(label);
            for button in ["Save", "Cancel"] {
                let buttons = self.reachable(button);
                for field in &fields {
                    for rect in &buttons {
                        if rect.intersects(*field) {
                            self.failure(
                                button,
                                format_args!("rect={rect:?} intersects {label}={field:?}"),
                            );
                        }
                    }
                }
            }
        }
    }
}
