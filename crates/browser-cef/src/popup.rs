//! Native popups (`<select>` lists, date pickers) that CEF paints apart from
//! the view (`PET_POPUP`) for the host to draw on top.

use crate::frame::PixelRect;
use crate::input::HostInput;

/// Where CEF placed a popup and where the view shows it, in view DIP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Popup {
    pub shown: PixelRect,
    pub requested: PixelRect,
}

impl Popup {
    /// Moves the popup inside a `width`×`height` view, as a native window
    /// would be kept on screen.
    pub fn place(requested: PixelRect, width: i32, height: i32) -> Self {
        let fit = |position: i32, size: i32, limit: i32| position.min(limit - size).max(0);
        Self {
            shown: PixelRect {
                x: fit(requested.x, requested.width, width),
                y: fit(requested.y, requested.height, height),
                ..requested
            },
            requested,
        }
    }

    /// Pointer input over the shown popup goes where CEF has it.
    pub fn route(&self, input: HostInput) -> HostInput {
        let (dx, dy) = (
            self.requested.x - self.shown.x,
            self.requested.y - self.shown.y,
        );
        let inside = |x: i32, y: i32| {
            x >= self.shown.x
                && y >= self.shown.y
                && x < self.shown.x + self.shown.width
                && y < self.shown.y + self.shown.height
        };
        match input {
            HostInput::MouseMove {
                x,
                y,
                modifiers,
                leave,
            } if inside(x, y) => HostInput::MouseMove {
                x: x + dx,
                y: y + dy,
                modifiers,
                leave,
            },
            HostInput::MouseClick {
                x,
                y,
                modifiers,
                button,
                up,
                count,
            } if inside(x, y) => HostInput::MouseClick {
                x: x + dx,
                y: y + dy,
                modifiers,
                button,
                up,
                count,
            },
            HostInput::MouseWheel {
                x,
                y,
                modifiers,
                dx: wheel_x,
                dy: wheel_y,
            } if inside(x, y) => HostInput::MouseWheel {
                x: x + dx,
                y: y + dy,
                modifiers,
                dx: wheel_x,
                dy: wheel_y,
            },
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::MouseButton;

    fn rect(x: i32, y: i32, width: i32, height: i32) -> PixelRect {
        PixelRect {
            x,
            y,
            width,
            height,
        }
    }

    fn click(x: i32, y: i32) -> HostInput {
        HostInput::MouseClick {
            x,
            y,
            modifiers: 0,
            button: MouseButton::Left,
            up: false,
            count: 1,
        }
    }

    #[test]
    fn a_popup_past_the_view_edge_is_shown_inside_and_input_follows_it() {
        let popup = Popup::place(rect(700, 550, 200, 100), 800, 600);
        assert_eq!(popup.shown, rect(600, 500, 200, 100));

        assert_eq!(popup.route(click(610, 510)), click(710, 560));
        assert_eq!(popup.route(click(10, 10)), click(10, 10));
    }

    #[test]
    fn a_popup_that_fits_keeps_its_place_and_larger_ones_start_at_the_origin() {
        let fits = Popup::place(rect(10, 20, 100, 50), 800, 600);
        assert_eq!(fits.shown, fits.requested);
        assert_eq!(fits.route(click(15, 25)), click(15, 25));

        let large = Popup::place(rect(-5, 30, 900, 700), 800, 600);
        assert_eq!((large.shown.x, large.shown.y), (0, 0));
    }
}
