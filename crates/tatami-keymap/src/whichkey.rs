//! The which-key sheet's clock, copied as is from plyterm. The sheet
//! itself is content derived from the keymap when it opens; this is only
//! *whether* it shows.
//!
//! doom's rule: a pending prefix opens the sheet after an idle delay, so
//! a practised `alt+w 3` never sees it and a pause at any level does.
//! Once open it stays open while you walk deeper (alt held, then `w`),
//! and closes the moment the prefix resolves — a chord completes, Esc
//! backs out, alt comes up with nothing pressed. `pin` is the `help`
//! action: open now, stay until something resolves.

use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct WhichKey {
    delay: Duration,
    /// When the pending prefix's panel is due to open.
    due: Option<Instant>,
    visible: bool,
    pinned: bool,
}

impl WhichKey {
    pub fn new(delay: Duration) -> Self {
        Self {
            delay,
            due: None,
            visible: false,
            pinned: false,
        }
    }

    pub fn set_delay(&mut self, delay: Duration) {
        self.delay = delay;
    }

    /// A prefix started, or the pending one moved a level deeper. An
    /// open panel stays open and just changes content; a closed one
    /// starts (or restarts) the idle clock.
    pub fn arm(&mut self, now: Instant) {
        if self.visible {
            return;
        }
        if self.delay.is_zero() {
            self.visible = true;
            self.due = None;
        } else {
            self.due = Some(now + self.delay);
        }
    }

    /// The prefix resolved: nothing is pending any more.
    pub fn clear(&mut self) {
        self.due = None;
        self.visible = false;
        self.pinned = false;
    }

    /// Open at once and hold, until `clear`.
    pub fn pin(&mut self) {
        self.due = None;
        self.visible = true;
        self.pinned = true;
    }

    /// Advance the clock. Returns true when the panel just opened.
    pub fn tick(&mut self, now: Instant) -> bool {
        match self.due {
            Some(t) if now >= t => {
                self.due = None;
                self.visible = true;
                true
            }
            _ => false,
        }
    }

    /// The next moment `tick` has something to do.
    pub fn due(&self) -> Option<Instant> {
        self.due
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    pub fn pinned(&self) -> bool {
        self.pinned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn opens_after_the_delay_not_before() {
        let t0 = Instant::now();
        let mut wk = WhichKey::new(ms(300));
        wk.arm(t0);
        assert!(!wk.visible());
        assert_eq!(wk.due(), Some(t0 + ms(300)));
        assert!(!wk.tick(t0 + ms(299)));
        assert!(!wk.visible());
        assert!(wk.tick(t0 + ms(300)));
        assert!(wk.visible());
        assert_eq!(wk.due(), None, "no wake-up left once it is open");
    }

    #[test]
    fn a_fast_chord_never_shows_it() {
        let t0 = Instant::now();
        let mut wk = WhichKey::new(ms(300));
        wk.arm(t0);
        wk.clear();
        assert!(!wk.tick(t0 + ms(1000)));
        assert!(!wk.visible());
        assert_eq!(wk.due(), None);
    }

    #[test]
    fn walking_deeper_restarts_a_pending_clock_but_keeps_an_open_panel() {
        let t0 = Instant::now();
        let mut wk = WhichKey::new(ms(300));
        // ⌘ down, then `p` 200ms later: the clock restarts from `p`.
        wk.arm(t0);
        wk.arm(t0 + ms(200));
        assert!(!wk.tick(t0 + ms(300)));
        assert!(wk.tick(t0 + ms(500)));
        // Open, then `r`: still open, no clock.
        wk.arm(t0 + ms(600));
        assert!(wk.visible());
        assert_eq!(wk.due(), None);
    }

    #[test]
    fn zero_delay_opens_at_once_and_pin_holds() {
        let t0 = Instant::now();
        let mut wk = WhichKey::new(ms(0));
        wk.arm(t0);
        assert!(wk.visible() && wk.due().is_none());
        let mut wk = WhichKey::new(ms(300));
        wk.pin();
        assert!(wk.visible() && wk.pinned());
        wk.arm(t0);
        assert!(wk.pinned(), "arming does not unpin");
        wk.clear();
        assert!(!wk.visible() && !wk.pinned());
    }
}
