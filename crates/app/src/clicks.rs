//! Multi-click counting for mouse presses. Time and position are passed in,
//! so the counting is testable without real timing.

use std::time::{Duration, Instant};

use crate::plugin::MouseButton;

/// One press, as a multi-click counter sees it.
#[derive(Clone, Copy, Debug)]
pub struct Click {
    pub button: MouseButton,
    pub at: Instant,
    pub pos: (f32, f32),
    /// The longest gap after the last press that still continues it.
    pub window: Duration,
    /// How far from the last press it may land and still continue it.
    pub slop: f32,
}

/// Counts presses of one button that follow each other quickly at about the
/// same spot.
#[derive(Debug, Default)]
pub struct ClickCounter {
    /// The last press (button, time, position, click count).
    last: Option<(MouseButton, Instant, (f32, f32), u32)>,
}

impl ClickCounter {
    /// Count `c` as the next of a multi-click when it follows the last press,
    /// of the same button, within its window and slop; else it starts a new
    /// one (1).
    pub fn count(&mut self, c: &Click) -> u32 {
        let (x, y) = c.pos;
        let clicks = match self.last {
            Some((b, t, (px, py), n))
                if b == c.button
                    && c.at.duration_since(t) < c.window
                    && (x - px).abs() <= c.slop
                    && (y - py).abs() <= c.slop =>
            {
                n + 1
            }
            _ => 1,
        };
        self.last = Some((c.button, c.at, c.pos, clicks));
        clicks
    }

    /// A press went elsewhere: the next one starts a new multi-click.
    pub fn reset(&mut self) {
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::{Click, ClickCounter};
    use crate::plugin::MouseButton;
    use std::time::{Duration, Instant};

    #[test]
    fn quick_presses_at_one_spot_count_up_and_a_late_or_distant_one_starts_over() {
        let t0 = Instant::now();
        let at = |ms, pos| Click {
            button: MouseButton::Left,
            at: t0 + Duration::from_millis(ms),
            pos,
            window: Duration::from_millis(400),
            slop: 4.0,
        };
        let mut c = ClickCounter::default();
        assert_eq!(c.count(&at(0, (10.0, 10.0))), 1);
        assert_eq!(c.count(&at(100, (13.0, 6.0))), 2, "within the slop");
        assert_eq!(c.count(&at(200, (13.0, 6.0))), 3);
        assert_eq!(c.count(&at(700, (13.0, 6.0))), 1, "too late");
        assert_eq!(c.count(&at(750, (20.0, 6.0))), 1, "too far");
        assert_eq!(c.count(&at(800, (20.0, 6.0))), 2);
        c.reset();
        assert_eq!(c.count(&at(850, (20.0, 6.0))), 1, "after a press elsewhere");
    }

    #[test]
    fn a_press_of_another_button_starts_over() {
        let t0 = Instant::now();
        let at = |ms, button| Click {
            button,
            at: t0 + Duration::from_millis(ms),
            pos: (10.0, 10.0),
            window: Duration::from_millis(400),
            slop: 4.0,
        };
        let mut c = ClickCounter::default();
        assert_eq!(c.count(&at(0, MouseButton::Left)), 1);
        assert_eq!(c.count(&at(50, MouseButton::Right)), 1, "a right press after a left one");
        assert_eq!(c.count(&at(100, MouseButton::Right)), 2);
        assert_eq!(c.count(&at(150, MouseButton::Middle)), 1);
        assert_eq!(c.count(&at(200, MouseButton::Left)), 1);
    }
}
