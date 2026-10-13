//! Double-tap detection for a single modifier key (IntelliJ "Search Everywhere"
//! style: tap Shift twice, quickly, with nothing else pressed in between).
//!
//! The chord/keybinding model can't express a double-tap, so this small state
//! machine sits in the window. It is time-injected (release takes the timestamp)
//! so the logic is unit-testable without real timing.

use std::time::{Duration, Instant};

pub struct TapDetector {
    /// Max gap between the two taps' releases to count as a double-tap.
    window: Duration,
    /// The modifier is held with no other key pressed since it went down.
    armed: bool,
    /// Release time of the previous clean lone tap, if still within the window.
    last_tap: Option<Instant>,
}

impl TapDetector {
    pub fn new(window: Duration) -> Self {
        TapDetector {
            window,
            armed: false,
            last_tap: None,
        }
    }

    /// The tracked modifier was pressed (ignore auto-repeat presses at the call
    /// site). Arms a potential lone tap.
    pub fn press(&mut self) {
        self.armed = true;
    }

    /// Any other key was pressed: this breaks the lone-tap requirement and the
    /// double-tap sequence.
    pub fn interrupt(&mut self) {
        self.armed = false;
        self.last_tap = None;
    }

    /// The tracked modifier was released. Returns `true` when this completes a
    /// double-tap (two clean lone taps within the window).
    pub fn release(&mut self, now: Instant) -> bool {
        if !self.armed {
            // Released after an interrupted/never-armed press: not a lone tap.
            return false;
        }
        self.armed = false;
        if let Some(prev) = self.last_tap {
            if now.duration_since(prev) <= self.window {
                self.last_tap = None;
                return true;
            }
        }
        self.last_tap = Some(now);
        false
    }
}

/// A key whose press already did its job and must not act twice: its
/// auto-repeats are dropped until it is released or another key is pressed.
/// A terminal's "press any key to close" arms it, so a held key doesn't also
/// type into the tab that becomes active.
pub struct HeldKey<K> {
    key: Option<K>,
}

impl<K> Default for HeldKey<K> {
    fn default() -> Self {
        HeldKey { key: None }
    }
}

impl<K: PartialEq> HeldKey<K> {
    /// `key`'s press just acted: drop its repeats.
    pub fn hold(&mut self, key: K) {
        self.key = Some(key);
    }

    /// Whether this key event is a repeat to drop. A release of the held key,
    /// or a fresh press of any other key, ends the hold. Pressing a modifier
    /// (`modifier_only`) does not: it starts a chord, not a new key press, and
    /// the held key may still be down.
    pub fn swallows(&mut self, key: &K, pressed: bool, repeat: bool, modifier_only: bool) -> bool {
        let Some(held) = &self.key else { return false };
        if held == key && pressed && repeat {
            return true;
        }
        if held == key && !pressed || pressed && !repeat && !modifier_only {
            self.key = None;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeats_of_the_held_key_are_dropped_until_it_is_released() {
        let mut h = HeldKey::default();
        assert!(!h.swallows(&'a', true, true, false), "nothing held yet");
        h.hold('a');
        assert!(h.swallows(&'a', true, true, false));
        assert!(h.swallows(&'a', true, true, false));
        assert!(!h.swallows(&'b', true, true, false), "another key's repeat is not ours");
        assert!(h.swallows(&'a', true, true, false), "still held");
        assert!(!h.swallows(&'a', false, false, false), "the release passes");
        assert!(!h.swallows(&'a', true, true, false), "and ends the hold");
    }

    #[test]
    fn a_fresh_press_of_another_key_ends_the_hold() {
        let mut h = HeldKey::default();
        h.hold('a');
        assert!(!h.swallows(&'b', true, false, false), "a new key is delivered");
        assert!(!h.swallows(&'a', true, true, false), "the hold is over");
    }

    #[test]
    fn pressing_a_modifier_does_not_end_the_hold() {
        let mut h = HeldKey::default();
        h.hold('a');
        assert!(!h.swallows(&'s', true, false, true), "the modifier is delivered");
        assert!(h.swallows(&'a', true, true, false), "the held key's repeats still drop");
    }

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn two_quick_lone_taps_trigger() {
        let mut d = TapDetector::new(Duration::from_millis(300));
        let t0 = Instant::now();
        d.press();
        assert!(!d.release(at(t0, 0)), "first tap never triggers");
        d.press();
        assert!(d.release(at(t0, 150)), "second quick tap triggers");
    }

    #[test]
    fn a_key_between_taps_cancels() {
        let mut d = TapDetector::new(Duration::from_millis(300));
        let t0 = Instant::now();
        d.press();
        assert!(!d.release(at(t0, 0)));
        d.press();
        d.interrupt(); // another key pressed while Shift held
        assert!(!d.release(at(t0, 100)), "an interrupted second tap must not trigger");
    }

    #[test]
    fn too_slow_does_not_trigger_but_re_arms() {
        let mut d = TapDetector::new(Duration::from_millis(300));
        let t0 = Instant::now();
        d.press();
        assert!(!d.release(at(t0, 0)));
        d.press();
        assert!(!d.release(at(t0, 500)), "gap beyond the window is not a double-tap");
        // That slow second tap becomes the new first tap; a quick third triggers.
        d.press();
        assert!(d.release(at(t0, 600)));
    }

    #[test]
    fn shift_plus_letter_is_not_a_tap() {
        // Holding Shift and pressing a letter (interrupt) then releasing Shift
        // must not count, even repeated.
        let mut d = TapDetector::new(Duration::from_millis(300));
        let t0 = Instant::now();
        d.press();
        d.interrupt();
        assert!(!d.release(at(t0, 20)));
        d.press();
        d.interrupt();
        assert!(!d.release(at(t0, 40)));
    }
}
