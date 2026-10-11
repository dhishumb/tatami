//! tatami's key model: chords, the keycode table, the keymap with its
//! modes and which-key rows, the which-key clock, and the dispatcher that
//! answers the event tap.
//!
//! Pure and generic: nothing here knows macOS or tatami's `Action`, so
//! every rule is unit-tested with string actions and a fake clock. The
//! keymap is built once at config load (`tatami-config`); a key press is
//! a table lookup and, for a bound chord, one clone of the action list.

pub mod chord;
pub mod keycode;
pub mod keymap;
pub mod state;
pub mod whichkey;

pub use chord::{KeyChord, KeyName, Mods};
pub use keycode::keyname_from_keycode;
pub use keymap::{Classify, Describe, Keymap, ModeMap, WhichKeyEntry};
pub use state::{Disposition, KeyInput, KeyState, Level, ModeAction, OverlayChange};
pub use whichkey::WhichKey;
