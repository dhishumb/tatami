//! The key dispatcher: plyterm's mode + which-key wiring, adapted to a
//! window manager that sees every key on the machine.
//!
//! The event tap answers synchronously, so this is a small state machine
//! with no I/O and no allocation on the common path:
//!
//! * **Normal mode.** Only chords holding the leader (Alt) are looked up.
//!   Bound → swallowed and run. Unbound → the app gets it, so
//!   `alt+enter` in a terminal keeps working. Plain typing never reaches
//!   the map at all.
//! * **Leader held alone** arms the top-level sheet; a key press resolves
//!   it, releasing the leader closes it. A practised `alt+h` never sees it.
//! * **In a mode** every key is ours: bound keys run (with or without the
//!   leader still held, so `alt+w 3` and `alt+w alt+3` both work), unbound
//!   keys are swallowed unless the mode is passthrough. A mode is sticky
//!   until a list ends in `normal`; nothing times it out.
//! * **Key-ups** of swallowed key-downs are swallowed once, so an app
//!   never sees a stray release for a press it didn't get.
//!
//! Actions are returned whole, mode changes included; the key state has
//! already applied those, and the caller skips them.

use std::time::{Duration, Instant};

use crate::chord::{KeyChord, KeyName, Mods};
use crate::keymap::Keymap;
use crate::whichkey::WhichKey;

/// What the tap saw, already mapped from a keycode to a key name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyInput {
    Down {
        chord: KeyChord,
        repeat: bool,
    },
    Up(KeyChord),
    /// A modifier changed; this is the new state.
    Flags(Mods),
}

/// Which sheet is pending or showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Level {
    /// The leader is held with nothing else pressed.
    Top,
    Mode(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayChange {
    /// The sheet just opened, or its level changed while open.
    Show(Level),
    Hide,
    Keep,
}

/// The actions the key state applies itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModeAction {
    Enter(String),
    Normal,
    /// Pin the sheet for the current level (again: unpin it).
    Help,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Disposition<A> {
    pub swallow: bool,
    pub actions: Vec<A>,
    pub overlay: OverlayChange,
}

impl<A> Disposition<A> {
    /// Hand the event to the app untouched. `Vec::new` doesn't allocate,
    /// so the common case costs nothing.
    pub fn pass() -> Self {
        Disposition {
            swallow: false,
            actions: Vec::new(),
            overlay: OverlayChange::Keep,
        }
    }
}

/// Keys pressed at once whose release we still owe a swallow. Rolling
/// `alt+h alt+j` overlaps two; four is generous.
const HELD_SLOTS: usize = 4;

#[derive(Debug)]
pub struct KeyState {
    mode: Option<String>,
    /// Whether the leader is down, so only its press (not every flags
    /// change while it is held) arms the top-level sheet.
    leader_down: bool,
    which_key: WhichKey,
    leader: Mods,
    held: [Option<KeyName>; HELD_SLOTS],
    /// Set when the mode changed during the current call, so an open
    /// sheet is re-shown with the new level's rows.
    level_dirty: bool,
}

impl KeyState {
    pub fn new(delay: Duration, leader: Mods) -> Self {
        KeyState {
            mode: None,
            leader_down: false,
            which_key: WhichKey::new(delay),
            leader,
            held: [None; HELD_SLOTS],
            level_dirty: false,
        }
    }

    pub fn leader(&self) -> Mods {
        self.leader
    }

    /// Config reload: new delay and leader, current mode kept.
    pub fn reconfigure(&mut self, delay: Duration, leader: Mods) {
        self.which_key.set_delay(delay);
        self.leader = leader;
    }

    pub fn mode(&self) -> Option<&str> {
        self.mode.as_deref()
    }

    pub fn level(&self) -> Level {
        match &self.mode {
            Some(m) => Level::Mode(m.clone()),
            None => Level::Top,
        }
    }

    pub fn sheet_visible(&self) -> bool {
        self.which_key.visible()
    }

    /// When the event loop must wake to call `tick`.
    pub fn due(&self) -> Option<Instant> {
        self.which_key.due()
    }

    /// The clock fired: the sheet for the current level opens.
    pub fn tick(&mut self, now: Instant) -> Option<OverlayChange> {
        self.which_key
            .tick(now)
            .then(|| OverlayChange::Show(self.level()))
    }

    pub fn on_input<A: Clone>(
        &mut self,
        km: &Keymap<A>,
        input: KeyInput,
        now: Instant,
        is_mode_action: impl Fn(&A) -> Option<ModeAction>,
    ) -> Disposition<A> {
        let was = self.which_key.visible();
        self.level_dirty = false;
        match input {
            KeyInput::Flags(m) => {
                self.on_flags(m, now);
                Disposition {
                    overlay: self.change(was),
                    ..Disposition::pass()
                }
            }
            KeyInput::Up(chord) => Disposition {
                swallow: self.release(chord.key),
                ..Disposition::pass()
            },
            KeyInput::Down { chord, .. } => {
                // Fast path: plain typing with nothing pending touches
                // neither the map nor the allocator.
                if self.mode.is_none()
                    && !chord.mods.contains(self.leader)
                    && !was
                    && self.which_key.due().is_none()
                {
                    return Disposition::pass();
                }
                let (swallow, actions) = match self.mode.is_some() {
                    true => self.down_in_mode(km, chord, now, &is_mode_action),
                    false => self.down_normal(km, chord, now, &is_mode_action),
                };
                if swallow {
                    self.hold(chord.key);
                }
                Disposition {
                    swallow,
                    actions,
                    overlay: self.change(was),
                }
            }
        }
    }

    /// Enter a mode (or leave, with `None`) from outside the keyboard:
    /// the CLI's `mode resize`, a `trigger-binding`.
    pub fn enter_mode(&mut self, name: Option<String>, now: Instant) -> OverlayChange {
        let was = self.which_key.visible();
        self.level_dirty = self.mode != name;
        self.mode = name;
        if self.mode.is_some() {
            self.which_key.arm(now);
        } else {
            self.which_key.clear();
        }
        self.change(was)
    }

    /// Open the current level's sheet now and hold it (the CLI's `help`).
    pub fn pin(&mut self) -> OverlayChange {
        let was = self.which_key.visible();
        self.level_dirty = false;
        self.which_key.pin();
        self.change(was)
    }

    /// The tap went away (disabled by timeout, secure input): releases we
    /// were waiting for will never come, so start over in normal mode
    /// rather than swallow keys behind a sheet nobody can see.
    pub fn focus_lost(&mut self) -> OverlayChange {
        let was = self.which_key.visible();
        self.level_dirty = self.mode.take().is_some();
        self.leader_down = false;
        self.held = [None; HELD_SLOTS];
        self.which_key.clear();
        self.change(was)
    }

    fn on_flags(&mut self, m: Mods, now: Instant) {
        let had = self.leader_down;
        self.leader_down = m.contains(self.leader);
        if self.mode.is_some() {
            return;
        }
        if m == self.leader && !had {
            self.which_key.arm(now);
        } else if had && !self.leader_down && !self.which_key.pinned() {
            // Leader up with nothing pressed: the top level resolves. A
            // pinned sheet (alt+/) stays so it can be read hands-free.
            self.which_key.clear();
        }
    }

    fn down_normal<A: Clone>(
        &mut self,
        km: &Keymap<A>,
        chord: KeyChord,
        now: Instant,
        f: &impl Fn(&A) -> Option<ModeAction>,
    ) -> (bool, Vec<A>) {
        if !chord.mods.contains(self.leader) {
            // Only a pinned top-level sheet outlives the leader. Any key
            // resolves it; Esc closes it without reaching the app.
            self.which_key.clear();
            return (
                chord.key == KeyName::Escape && !chord.mods.any(),
                Vec::new(),
            );
        }
        let Some(actions) = km.lookup_direct(&chord) else {
            // Not ours: the app gets it, and the pending top level is
            // resolved all the same.
            self.which_key.clear();
            return (false, Vec::new());
        };
        let helped = self.apply(km, actions, now, f);
        if self.mode.is_none() && !helped {
            self.which_key.clear();
        }
        (true, actions.to_vec())
    }

    fn down_in_mode<A: Clone>(
        &mut self,
        km: &Keymap<A>,
        chord: KeyChord,
        now: Instant,
        f: &impl Fn(&A) -> Option<ModeAction>,
    ) -> (bool, Vec<A>) {
        let Some(mm) = km.mode(self.mode.as_deref().unwrap_or_default()) else {
            // The keymap was reloaded without this mode. Swallowing every
            // key of a mode nobody can leave would lock the keyboard.
            tracing::warn!(mode = ?self.mode, "keys: active mode vanished; back to normal");
            self.leave();
            return self.down_normal(km, chord, now, f);
        };
        // The leader may still be down from the chord that entered the
        // mode (`alt+w` then `alt+3`), or a binding may spell it out.
        let other = match chord.mods.contains(self.leader) {
            true => chord.mods.without(self.leader),
            false => chord.mods.union(self.leader),
        };
        let found = mm
            .get(&chord)
            .or_else(|| mm.get(&KeyChord::new(chord.key, other)));
        match found {
            Some(actions) => {
                let helped = self.apply(km, actions, now, f);
                // Every key restarts a closed sheet's clock (doom), so a
                // fluent resize run never flashes it.
                if self.mode.is_some() && !helped {
                    self.which_key.arm(now);
                }
                (true, actions.to_vec())
            }
            // Esc always leaves, bound or not: a keymap that forgot it
            // must not trap the keyboard.
            None if chord.key == KeyName::Escape && !chord.mods.without(self.leader).any() => {
                self.leave();
                (true, Vec::new())
            }
            None => (!mm.passthrough, Vec::new()),
        }
    }

    /// Apply the mode-changing actions in a bound list, in order. Returns
    /// whether `help` was among them.
    fn apply<A>(
        &mut self,
        km: &Keymap<A>,
        actions: &[A],
        now: Instant,
        f: &impl Fn(&A) -> Option<ModeAction>,
    ) -> bool {
        let mut helped = false;
        for a in actions {
            match f(a) {
                Some(ModeAction::Enter(m)) => {
                    if !km.has_mode(&m) {
                        tracing::warn!(mode = %m, "keys: no such mode; staying put");
                        continue;
                    }
                    if self.mode.as_deref() != Some(m.as_str()) {
                        self.mode = Some(m);
                        self.level_dirty = true;
                    }
                    self.which_key.arm(now);
                }
                Some(ModeAction::Normal) => self.leave(),
                Some(ModeAction::Help) => {
                    helped = true;
                    if self.which_key.pinned() {
                        self.which_key.clear();
                    } else {
                        self.which_key.pin();
                    }
                }
                None => {}
            }
        }
        helped
    }

    fn leave(&mut self) {
        if self.mode.take().is_some() {
            self.level_dirty = true;
        }
        self.which_key.clear();
    }

    fn change(&self, was: bool) -> OverlayChange {
        match (was, self.which_key.visible()) {
            (false, true) => OverlayChange::Show(self.level()),
            (true, true) if self.level_dirty => OverlayChange::Show(self.level()),
            (true, false) => OverlayChange::Hide,
            _ => OverlayChange::Keep,
        }
    }

    fn hold(&mut self, key: KeyName) {
        if self.held.contains(&Some(key)) {
            return;
        }
        match self.held.iter_mut().find(|s| s.is_none()) {
            Some(slot) => *slot = Some(key),
            None => self.held[0] = Some(key),
        }
    }

    fn release(&mut self, key: KeyName) -> bool {
        match self.held.iter_mut().find(|s| **s == Some(key)) {
            Some(slot) => {
                *slot = None;
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::tests::{classify_str, sample};

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn c(s: &str) -> KeyChord {
        KeyChord::parse(s).unwrap_or_else(|| panic!("chord {s}"))
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    struct T {
        km: Keymap<String>,
        ks: KeyState,
        t0: Instant,
    }

    impl T {
        fn new(delay: u64) -> T {
            T {
                km: sample(),
                ks: KeyState::new(ms(delay), Mods::ALT),
                t0: Instant::now(),
            }
        }
        fn send(&mut self, input: KeyInput, at: u64) -> Disposition<String> {
            self.ks
                .on_input(&self.km, input, self.t0 + ms(at), |a| classify_str(a))
        }
        fn down(&mut self, chord: &str, at: u64) -> Disposition<String> {
            self.send(
                KeyInput::Down {
                    chord: c(chord),
                    repeat: false,
                },
                at,
            )
        }
        fn up(&mut self, chord: &str, at: u64) -> Disposition<String> {
            self.send(KeyInput::Up(c(chord)), at)
        }
        fn flags(&mut self, m: Mods, at: u64) -> Disposition<String> {
            self.send(KeyInput::Flags(m), at)
        }
        fn tick(&mut self, at: u64) -> Option<OverlayChange> {
            self.ks.tick(self.t0 + ms(at))
        }
    }

    fn ws() -> Level {
        Level::Mode("workspaces".into())
    }

    #[test]
    fn a_fast_chord_never_shows_the_sheet() {
        let mut t = T::new(300);
        assert_eq!(t.flags(Mods::ALT, 0), Disposition::pass());
        let d = t.down("alt+h", 50);
        assert!(d.swallow);
        assert_eq!(d.actions, strs(&["focus left"]));
        assert_eq!(d.overlay, OverlayChange::Keep);
        assert!(t.up("h", 70).swallow);
        assert_eq!(t.flags(Mods::NONE, 80).overlay, OverlayChange::Keep);
        assert_eq!(t.tick(1000), None);
        assert_eq!(t.ks.due(), None);
    }

    #[test]
    fn leader_held_alone_shows_the_top_level() {
        let mut t = T::new(300);
        t.flags(Mods::ALT, 0);
        assert_eq!(t.ks.due(), Some(t.t0 + ms(300)));
        assert_eq!(t.tick(299), None);
        assert_eq!(t.tick(300), Some(OverlayChange::Show(Level::Top)));
        // Another modifier joining and leaving doesn't restart anything.
        assert_eq!(
            t.flags(Mods::ALT.union(Mods::SHIFT), 400).overlay,
            OverlayChange::Keep
        );
        assert_eq!(t.flags(Mods::ALT, 450).overlay, OverlayChange::Keep);
        assert_eq!(t.flags(Mods::NONE, 500).overlay, OverlayChange::Hide);
        assert!(!t.ks.sheet_visible());
    }

    #[test]
    fn a_chord_from_an_open_top_level_closes_it() {
        let mut t = T::new(300);
        t.flags(Mods::ALT, 0);
        t.tick(300);
        let d = t.down("alt+l", 400);
        assert_eq!((d.swallow, d.overlay), (true, OverlayChange::Hide));
        // Still holding alt: no second sheet until alt comes up and down.
        assert_eq!(t.tick(2000), None);
    }

    #[test]
    fn mode_chord_then_pause_shows_the_mode_sheet() {
        let mut t = T::new(300);
        t.flags(Mods::ALT, 0);
        let d = t.down("alt+w", 50);
        assert!(d.swallow);
        assert_eq!(d.actions, strs(&["mode workspaces"]));
        assert_eq!(t.ks.mode(), Some("workspaces"));
        // Releasing alt doesn't end the mode.
        assert_eq!(t.flags(Mods::NONE, 60).overlay, OverlayChange::Keep);
        // The clock restarted at `w`, not at alt.
        assert_eq!(t.tick(349), None);
        assert_eq!(t.tick(350), Some(OverlayChange::Show(ws())));
        let d = t.down("3", 500);
        assert!(d.swallow);
        assert_eq!(d.actions, strs(&["workspace 3", "normal"]));
        assert_eq!(d.overlay, OverlayChange::Hide);
        assert_eq!(t.ks.mode(), None);
    }

    #[test]
    fn walking_into_a_mode_keeps_an_open_sheet_and_changes_its_level() {
        let mut t = T::new(300);
        t.flags(Mods::ALT, 0);
        assert_eq!(t.tick(300), Some(OverlayChange::Show(Level::Top)));
        assert_eq!(t.down("alt+w", 400).overlay, OverlayChange::Show(ws()));
        assert_eq!(
            t.down("r", 450).overlay,
            OverlayChange::Show(Level::Mode("resize".into()))
        );
    }

    #[test]
    fn zero_delay_shows_a_mode_at_once() {
        let mut t = T::new(0);
        assert_eq!(t.down("alt+w", 0).overlay, OverlayChange::Show(ws()));
    }

    #[test]
    fn unbound_leader_chords_and_plain_keys_pass() {
        let mut t = T::new(300);
        t.km.bind(None, c("h"), strs(&["close"]));
        let d = t.down("alt+enter", 0);
        assert_eq!(d, Disposition::pass());
        assert!(!t.up("enter", 10).swallow);
        // Plain keys are never looked up, even when something binds them.
        assert_eq!(t.down("h", 20), Disposition::pass());
        assert_eq!(t.down("shift+h", 30), Disposition::pass());
    }

    #[test]
    fn an_unbound_leader_chord_resolves_a_pending_top_level() {
        let mut t = T::new(300);
        t.flags(Mods::ALT, 0);
        assert!(!t.down("alt+enter", 50).swallow);
        assert_eq!(t.tick(1000), None);
    }

    #[test]
    fn inside_a_mode_unbound_keys_are_swallowed() {
        let mut t = T::new(300);
        t.down("alt+w", 0);
        let d = t.down("x", 10);
        assert!(d.swallow && d.actions.is_empty());
        assert!(t.up("x", 20).swallow);
        assert_eq!(t.ks.mode(), Some("workspaces"));
        // Modifier changes still go through.
        assert!(!t.flags(Mods::SHIFT, 30).swallow);
    }

    #[test]
    fn inside_a_mode_the_leader_is_optional() {
        let mut t = T::new(300);
        t.down("alt+w", 0);
        assert_eq!(
            t.down("alt+3", 10).actions,
            strs(&["workspace 3", "normal"])
        );
        assert_eq!(t.ks.mode(), None);
    }

    #[test]
    fn sticky_modes_repeat_until_esc() {
        let mut t = T::new(300);
        t.down("alt+r", 0);
        for i in 0..3 {
            let d = t.send(
                KeyInput::Down {
                    chord: c("h"),
                    repeat: i > 0,
                },
                10 + i,
            );
            assert!(d.swallow);
            assert_eq!(d.actions, strs(&["resize width -50"]));
        }
        assert_eq!(t.ks.mode(), Some("resize"));
        // Each key restarted the closed sheet's clock.
        assert_eq!(t.ks.due(), Some(t.t0 + ms(12 + 300)));
        assert_eq!(t.down("esc", 20).actions, strs(&["normal"]));
        assert_eq!(t.ks.mode(), None);
        assert_eq!(t.ks.due(), None);
    }

    #[test]
    fn esc_always_leaves_even_when_unbound() {
        let mut t = T::new(300);
        t.km.bind(Some("bare"), c("x"), strs(&["close"]));
        t.ks.enter_mode(Some("bare".into()), t.t0);
        let d = t.down("esc", 10);
        assert!(d.swallow && d.actions.is_empty());
        assert_eq!(t.ks.mode(), None);
    }

    #[test]
    fn passthrough_modes_let_unbound_keys_through() {
        let mut t = T::new(300);
        t.km.bind(None, c("alt+g"), strs(&["mode locked"]));
        t.km.bind(Some("locked"), c("alt+g"), strs(&["normal"]));
        t.km.set_passthrough("locked");
        t.down("alt+g", 0);
        assert_eq!(t.down("x", 10), Disposition::pass());
        assert!(t.down("alt+g", 20).swallow);
        assert_eq!(t.ks.mode(), None);
    }

    #[test]
    fn the_release_of_a_swallowed_press_is_swallowed_once() {
        let mut t = T::new(300);
        t.down("alt+h", 0);
        // Alt may come up first; the release is matched by key alone.
        assert!(t.up("h", 10).swallow);
        assert!(!t.up("h", 20).swallow);
        // Rolled chords overlap.
        t.down("alt+h", 30);
        t.down("alt+j", 31);
        assert!(t.up("alt+h", 32).swallow);
        assert!(t.up("alt+j", 33).swallow);
    }

    #[test]
    fn help_pins_past_the_leader_and_esc_closes_it_quietly() {
        let mut t = T::new(300);
        t.flags(Mods::ALT, 0);
        let d = t.down("alt+/", 10);
        assert!(d.swallow);
        assert_eq!(d.overlay, OverlayChange::Show(Level::Top));
        assert_eq!(t.flags(Mods::NONE, 20).overlay, OverlayChange::Keep);
        assert!(t.ks.sheet_visible());
        let d = t.down("esc", 30);
        assert!(d.swallow, "esc closing the sheet never reaches the app");
        assert_eq!(d.overlay, OverlayChange::Hide);
        assert!(t.up("esc", 40).swallow);
        // Any other key closes it and goes through.
        t.down("alt+/", 50);
        let d = t.down("x", 60);
        assert_eq!((d.swallow, d.overlay), (false, OverlayChange::Hide));
        // And help again toggles it off.
        t.down("alt+/", 70);
        assert_eq!(t.down("alt+/", 80).overlay, OverlayChange::Hide);
    }

    #[test]
    fn a_mode_the_keymap_lacks_is_never_entered() {
        let mut t = T::new(300);
        t.km.bind(None, c("alt+z"), strs(&["mode ghost"]));
        assert!(t.down("alt+z", 0).swallow);
        assert_eq!(t.ks.mode(), None);
        // A mode that vanished on reload drops back to normal.
        t.ks.enter_mode(Some("ghost".into()), t.t0);
        assert_eq!(t.down("x", 10), Disposition::pass());
        assert_eq!(t.ks.mode(), None);
    }

    #[test]
    fn focus_loss_hides_and_resets() {
        let mut t = T::new(300);
        t.flags(Mods::ALT, 0);
        t.down("alt+w", 10);
        t.tick(400);
        assert!(t.ks.sheet_visible());
        assert_eq!(t.ks.focus_lost(), OverlayChange::Hide);
        assert_eq!(t.ks.mode(), None);
        assert_eq!(t.ks.focus_lost(), OverlayChange::Keep);
    }

    #[test]
    fn enter_mode_and_pin_from_outside() {
        let mut t = T::new(300);
        assert_eq!(
            t.ks.enter_mode(Some("resize".into()), t.t0),
            OverlayChange::Keep
        );
        assert_eq!(
            t.ks.pin(),
            OverlayChange::Show(Level::Mode("resize".into()))
        );
        assert_eq!(
            t.ks.enter_mode(Some("workspaces".into()), t.t0),
            OverlayChange::Show(ws())
        );
        assert_eq!(t.ks.enter_mode(None, t.t0), OverlayChange::Hide);
    }
}
