use crate::theme::tokens::palette;

const HEALTHY_PERCENT: f64 = 50.0;
const LOW_PERCENT: f64 = 20.0;

/// The percentage stays in the label, tooltip and accessible value too.
pub fn remaining_fill(remaining_percent: f64) -> egui::Color32 {
    if remaining_percent > HEALTHY_PERCENT {
        palette().SUCCESS
    } else if remaining_percent >= LOW_PERCENT {
        palette().WARNING_FG
    } else {
        palette().ERROR_FG
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn bar_fill_tracks_remaining_quota_thresholds() {
        let p = palette();
        for (remaining, expected) in [
            (100.0, p.SUCCESS),
            (50.1, p.SUCCESS),
            (50.0, p.WARNING_FG),
            (20.0, p.WARNING_FG),
            (19.9, p.ERROR_FG),
            (0.0, p.ERROR_FG),
        ] {
            assert_eq!(remaining_fill(remaining), expected, "{remaining}%");
        }
    }
}
