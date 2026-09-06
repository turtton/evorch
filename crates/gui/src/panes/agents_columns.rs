//! Agents telemetry grid の列幅自動調整ロジック。

use crate::theme::tokens::{AGENTS_COL_MAX, AGENTS_COL_MIN};

pub(crate) fn fit_columns(natural: &[f32], spacing: f32, available: f32) -> Vec<f32> {
    let count = natural.len();
    if count == 0 {
        return Vec::new();
    }

    let mut widths: Vec<f32> = natural
        .iter()
        .map(|width| width.clamp(AGENTS_COL_MIN, AGENTS_COL_MAX))
        .collect();

    let total_with_spacing =
        |widths: &[f32]| widths.iter().sum::<f32>() + spacing * (count.saturating_sub(1) as f32);

    let total = total_with_spacing(&widths);
    if total <= available {
        return widths;
    }

    let deficit = total - available;
    let slack_sum = widths
        .iter()
        .map(|width| width - AGENTS_COL_MIN)
        .sum::<f32>();

    if slack_sum > 0.0 {
        if slack_sum <= deficit {
            widths.fill(AGENTS_COL_MIN);
        } else {
            for width in &mut widths {
                let slack = *width - AGENTS_COL_MIN;
                *width -= deficit * slack / slack_sum;
            }
        }
    }

    let total = total_with_spacing(&widths);
    if total > available {
        let content = available - spacing * (count.saturating_sub(1) as f32);
        let scale = content / widths.iter().sum::<f32>();
        for width in &mut widths {
            *width *= scale;
        }
    }

    widths
}

#[cfg(test)]
mod tests {
    use crate::theme::tokens::{AGENTS_COL_MAX, AGENTS_COL_MIN};

    use super::fit_columns;

    fn approx_eq(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.01
    }

    #[test]
    fn fit_clamps_natural_widths_into_token_bounds() {
        let widths = fit_columns(&[10.0, 400.0], 4.0, 1000.0);
        assert_eq!(widths.len(), 2);
        assert!(approx_eq(widths[0], AGENTS_COL_MIN));
        assert!(approx_eq(widths[1], AGENTS_COL_MAX));
    }

    #[test]
    fn fit_keeps_widths_when_total_fits() {
        let widths = fit_columns(&[50.0, 100.0, 80.0], 4.0, 1000.0);
        assert_eq!(widths.len(), 3);
        assert!(approx_eq(widths[0], AGENTS_COL_MIN));
        assert!(approx_eq(widths[1], 100.0));
        assert!(approx_eq(widths[2], 80.0));
    }

    #[test]
    fn fit_shrinks_proportionally_to_available() {
        let widths = fit_columns(&[160.0, 160.0, 160.0], 4.0, 300.0);
        let total = widths.iter().sum::<f32>() + 4.0 * 2.0;
        assert!(total < 300.5 && total > 299.5, "total was {total}");
        for w in &widths {
            assert!(*w >= AGENTS_COL_MIN, "width {w} below minimum");
        }
        assert!(widths[0] <= widths[1] && widths[1] <= widths[2]);
    }

    #[test]
    fn fit_scales_below_min_as_last_resort() {
        let widths = fit_columns(&[160.0, 160.0, 160.0], 4.0, 50.0);
        let total = widths.iter().sum::<f32>() + 4.0 * 2.0;
        assert!(
            total < 50.5 && total > 49.5,
            "total {total} should equal available 50"
        );
        assert!(widths[0] <= widths[1] && widths[1] <= widths[2]);
    }
}
