#[cfg(test)]
use crate::panes::sidebar_widgets::RowAttention;

#[cfg(test)]
mod tests {

    use super::RowAttention;

    /// The palette is process-wide; tests that install one take turns.
    fn palette_lock() -> std::sync::MutexGuard<'static, ()> {
        static PALETTE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        PALETTE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn each_state_has_its_own_colors_in_both_themes() {
        use crate::theme::tokens::Palette;
        for p in [Palette::tokyo_night(), Palette::graphite()] {
            let states = [
                RowAttention::ExternalWorking,
                RowAttention::Unread,
                RowAttention::Approval,
                RowAttention::Question,
            ];
            let colors: Vec<_> = states
                .iter()
                .map(|state| state.colors(&p).expect("a state with colors"))
                .collect();

            // Then: no two states share a text color or a surface
            for (i, (fg, tone)) in colors.iter().enumerate() {
                for (other_fg, other) in &colors[i + 1..] {
                    assert_ne!(fg, other_fg);
                    assert_ne!(tone.surface, other.surface);
                }
                // And: a surface is neither the sidebar's own nor a selection / hover fill
                assert_ne!(tone.surface, p.SIDEBAR);
                assert_ne!(tone.surface, p.ACTIVE_ROW);
                assert_ne!(tone.surface, p.HOVER_ROW);
            }

            // And: a normal row keeps the shared row style
            assert!(RowAttention::Normal.colors(&p).is_none());
        }
    }

    #[test]
    fn a_tinted_row_stays_distinct_resting_hovered_and_selected() {
        use crate::theme::tokens::Palette;
        use crate::theme::widgets::toned_row_target;
        let _guard = palette_lock();
        for p in [Palette::tokyo_night(), Palette::graphite()] {
            crate::theme::tokens::install_palette(p);
            for state in [
                RowAttention::ExternalWorking,
                RowAttention::Unread,
                RowAttention::Approval,
                RowAttention::Question,
            ] {
                let (_, tone) = state.colors(&p).expect("colors");
                let hovered = toned_row_target(tone.surface, false);
                let selected = toned_row_target(tone.surface, true);
                assert_ne!(hovered, tone.surface, "{state:?} hover");
                assert_ne!(selected, tone.surface, "{state:?} selection");
                assert_ne!(hovered, selected, "{state:?} hover vs selection");
            }
        }
    }
}
