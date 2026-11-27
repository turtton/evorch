//! CPU, memory and storage bars at the right end of the footer, drawn like the quota bars.

pub mod stats;

use std::path::PathBuf;

use crate::panes::quota_footer::remaining_fill;
use crate::theme::text::muted;
use stats::{SystemSampler, SystemSnapshot, Usage, format_bytes};

/// Starts the system sampler on the first footer frame and keeps its latest reading.
#[derive(Default)]
pub struct SystemFooter {
    /// The directory holding the database; its filesystem is the one reported.
    storage_directory: Option<PathBuf>,
    sampler: Option<SystemSampler>,
    started: bool,
    #[cfg(test)]
    reading: Option<SystemSnapshot>,
}

impl SystemFooter {
    /// Repaint-policy fixtures must not start the machine's periodic sampler.
    #[cfg(test)]
    pub fn without_sampling() -> Self {
        Self {
            started: true,
            ..Default::default()
        }
    }

    #[cfg(test)]
    pub fn with_reading(reading: SystemSnapshot) -> Self {
        Self {
            started: true,
            reading: Some(reading),
            ..Default::default()
        }
    }

    pub fn set_storage_directory(&mut self, directory: Option<PathBuf>) {
        self.storage_directory = directory;
    }

    pub fn latest(&mut self, ctx: &egui::Context) -> Option<&SystemSnapshot> {
        #[cfg(test)]
        if self.reading.is_some() {
            return self.reading.as_ref();
        }
        if !self.started {
            self.started = true;
            let ctx = ctx.clone();
            // One coalesced repaint per reading; nothing repaints while no reading arrives.
            self.sampler = SystemSampler::start(self.storage_directory.clone(), move || {
                crate::repaint::coalesced(&ctx);
            });
        }
        self.sampler.as_mut()?.latest()
    }
}

pub fn usage_label(today_cost: Option<String>) -> String {
    today_cost.map_or_else(|| "Usage".to_owned(), |cost| format!("{cost} today"))
}

/// A bar that fills as the machine gets busier, so the colour thresholds of the quota bars
/// (which warn as the remaining share falls) read the free share.
fn used_fill(ratio: f64) -> egui::Color32 {
    remaining_fill((1.0 - ratio) * 100.0)
}

fn percent(ratio: f64) -> f64 {
    ratio * 100.0
}

fn usage_line(usage: Usage) -> String {
    format!(
        "{} of {} used ({:.0}%)",
        format_bytes(usage.used),
        format_bytes(usage.total),
        percent(usage.ratio())
    )
}

/// The label, fill share and tooltip lines of each bar.
fn bars(snapshot: &SystemSnapshot) -> Vec<(&'static str, f64, Vec<String>)> {
    let mut bars = vec![
        (
            "CPU",
            snapshot.cpu_ratio(),
            vec![
                "CPU · load averaged over all cores".to_owned(),
                format!("{:.0}% in use", percent(snapshot.cpu_ratio())),
            ],
        ),
        (
            "Mem",
            snapshot.memory.ratio(),
            vec![
                "Memory · system-wide".to_owned(),
                usage_line(snapshot.memory),
            ],
        ),
    ];
    if let Some(storage) = &snapshot.storage {
        bars.push((
            "Disk",
            storage.usage.ratio(),
            vec![
                "Storage · filesystem holding the evorch data directory".to_owned(),
                format!("Data directory: {}", storage.directory.display()),
                format!("Mounted at: {}", storage.mount.display()),
                usage_line(storage.usage),
            ],
        ));
    }
    bars
}

fn show_bars(ui: &mut egui::Ui, snapshot: &SystemSnapshot) {
    // The row is laid out right to left, so the last bar is added first, and each
    // bar before its label, which reads as "label: bar" in the order of `bars`.
    for (label, ratio, details) in bars(snapshot).into_iter().rev() {
        let tooltip = |ui: &mut egui::Ui| {
            for line in &details {
                ui.label(line);
            }
        };
        // The ratio is clamped to 0..=1, which f32 represents.
        #[allow(clippy::cast_possible_truncation)]
        let fraction = ratio as f32;
        ui.add(
            egui::ProgressBar::new(fraction)
                .fill(used_fill(ratio))
                .desired_width(54.0)
                .desired_height(7.0),
        )
        .on_hover_ui(tooltip);
        ui.label(muted(label)).on_hover_ui(tooltip);
    }
}

/// Lays out one footer row: `left` fills the row and `snapshot`'s bars sit at its right end.
/// The layout is the same with or without a reading, and `left` has an explicit id, so the
/// widget ids in `left` (an open menu, say) survive the first reading arriving.
pub fn with_right_stats(
    ui: &mut egui::Ui,
    snapshot: Option<&SystemSnapshot>,
    left: impl FnOnce(&mut egui::Ui),
) {
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if let Some(snapshot) = snapshot {
                show_bars(ui, snapshot);
            }
            ui.scope_builder(
                egui::UiBuilder::new()
                    .id(ui.id().with("system-footer-left"))
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
                left,
            );
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::tokens::palette;
    use egui_kittest::kittest::Queryable as _;
    use stats::StorageUsage;
    use std::path::PathBuf;

    fn snapshot(storage: bool) -> SystemSnapshot {
        SystemSnapshot {
            cpu_percent: 25.0,
            memory: Usage {
                used: 1 << 30,
                total: 4 << 30,
            },
            storage: storage.then(|| StorageUsage {
                directory: PathBuf::from("/data/evorch"),
                mount: PathBuf::from("/data"),
                usage: Usage {
                    used: 90,
                    total: 100,
                },
            }),
        }
    }

    // Given: readings with and without a known data directory
    // When: the bars are built / Then: storage appears only with a directory, and its tooltip names what it measures
    #[test]
    fn storage_bar_names_its_target_and_needs_a_directory() {
        let labels = |snapshot| {
            bars(&snapshot)
                .into_iter()
                .map(|(label, ..)| label)
                .collect::<Vec<_>>()
        };
        assert_eq!(labels(snapshot(false)), ["CPU", "Mem"]);
        assert_eq!(labels(snapshot(true)), ["CPU", "Mem", "Disk"]);

        let all = bars(&snapshot(true));
        let (_, ratio, details) = &all[2];
        assert!((ratio - 0.9).abs() < 1e-9);
        let text = details.join("\n");
        for needle in [
            "evorch data directory",
            "/data/evorch",
            "Mounted at: /data",
            "90 B of 100 B used (90%)",
        ] {
            assert!(text.contains(needle), "{needle} in {text}");
        }
        assert!(
            all[1]
                .2
                .join("\n")
                .contains("1.0 GiB of 4.0 GiB used (25%)")
        );
    }

    // Given: a footer row with the three bars
    // When: it is laid out / Then: each label sits left of its own bar, reading "label: bar" in order
    #[test]
    fn each_label_precedes_its_bar() {
        let reading = snapshot(true);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(900.0, 100.0))
            .build_ui(|ui| {
                with_right_stats(ui, Some(&reading), |ui| {
                    ui.label("left");
                });
            });
        harness.run();
        let labels: Vec<f32> = ["CPU", "Mem", "Disk"]
            .into_iter()
            .map(|label| harness.get_by_label(label).rect().left())
            .collect();
        let mut bars: Vec<f32> = harness
            .query_all_by_role(egui::accesskit::Role::ProgressIndicator)
            .map(|bar| bar.rect().left())
            .collect();
        bars.sort_by(f32::total_cmp);
        assert_eq!(bars.len(), 3);
        for index in 0..3 {
            assert!(labels[index] < bars[index], "label {index} before its bar");
            if let Some(next) = labels.get(index + 1) {
                assert!(bars[index] < *next, "bar {index} before the next label");
            }
        }
    }

    // Given: busy and idle readings / When: filled / Then: the colour warns as the free share falls
    #[test]
    fn fill_warns_as_usage_rises() {
        let p = palette();
        assert_eq!(used_fill(0.1), p.SUCCESS);
        assert_eq!(used_fill(0.6), p.WARNING_FG);
        assert_eq!(used_fill(0.95), p.ERROR_FG);
    }

    // Given: a footer row with left content and a reading
    // When: it is laid out in a wide panel / Then: the bars end at the right edge and the left content stays at the left
    #[test]
    fn stats_sit_at_the_right_end_of_the_row() {
        const WIDTH: f32 = 900.0;
        let rects = std::cell::Cell::new(None);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(WIDTH, 100.0))
            .build_ui(|ui| {
                crate::theme::install(ui.ctx());
                let reading = snapshot(true);
                let mut left = egui::Rect::NOTHING;
                with_right_stats(ui, Some(&reading), |ui| {
                    left = ui.label("left").rect;
                });
                rects.set(Some((left, ui.min_rect())));
            });
        harness.run();
        let (left, row) = rects.get().expect("row drawn");
        assert!(left.left() < 20.0, "left content stays left: {left:?}");
        assert!(
            row.right() > WIDTH - 20.0,
            "the bars reach the right edge: {row:?}"
        );
    }

    // Given: a settings-style menu in the left content, opened before any reading exists
    // When: the first reading arrives and the bars appear / Then: the menu is still open
    #[test]
    fn open_menu_survives_the_first_reading() {
        let reading = std::cell::Cell::new(None);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(900.0, 200.0))
            .build_ui(|ui| {
                let snapshot = reading.take();
                with_right_stats(ui, snapshot.as_ref(), |ui| {
                    ui.menu_button("Menu", |ui| {
                        let _ = ui.button("Entry");
                    });
                });
                reading.set(snapshot);
            });
        harness.get_by_label("Menu").click();
        harness.run();
        assert!(harness.query_by_label("Entry").is_some(), "the menu opened");
        reading.set(Some(snapshot(true)));
        harness.run();
        assert!(harness.query_by_label("CPU").is_some(), "the bars appeared");
        assert!(
            harness.query_by_label("Entry").is_some(),
            "the menu stayed open"
        );
    }
}
