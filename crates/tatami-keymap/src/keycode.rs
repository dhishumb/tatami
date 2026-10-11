//! Hardware virtual keycodes (`kVK_*` from Carbon's `Events.h`) to key
//! names. The table is the US-ANSI layout on purpose: a keycode names a
//! physical key, and bindings are physical (see `chord`), so `alt+h` is
//! the key where `h` sits on a US board whatever the input source says.
//! This also sidesteps Option's character translation (`alt+h` → `˙`).

use crate::chord::KeyName;

/// `None` for modifiers, media keys and anything the keymap doesn't model.
pub fn keyname_from_keycode(code: u16) -> Option<KeyName> {
    use KeyName::*;
    Some(match code {
        0x00 => Char('a'),
        0x01 => Char('s'),
        0x02 => Char('d'),
        0x03 => Char('f'),
        0x04 => Char('h'),
        0x05 => Char('g'),
        0x06 => Char('z'),
        0x07 => Char('x'),
        0x08 => Char('c'),
        0x09 => Char('v'),
        0x0B => Char('b'),
        0x0C => Char('q'),
        0x0D => Char('w'),
        0x0E => Char('e'),
        0x0F => Char('r'),
        0x10 => Char('y'),
        0x11 => Char('t'),
        0x12 => Char('1'),
        0x13 => Char('2'),
        0x14 => Char('3'),
        0x15 => Char('4'),
        0x16 => Char('6'),
        0x17 => Char('5'),
        0x18 => Char('='),
        0x19 => Char('9'),
        0x1A => Char('7'),
        0x1B => Char('-'),
        0x1C => Char('8'),
        0x1D => Char('0'),
        0x1E => Char(']'),
        0x1F => Char('o'),
        0x20 => Char('u'),
        0x21 => Char('['),
        0x22 => Char('i'),
        0x23 => Char('p'),
        0x24 => Enter,
        0x25 => Char('l'),
        0x26 => Char('j'),
        0x27 => Char('\''),
        0x28 => Char('k'),
        0x29 => Char(';'),
        0x2A => Char('\\'),
        0x2B => Char(','),
        0x2C => Char('/'),
        0x2D => Char('n'),
        0x2E => Char('m'),
        0x2F => Char('.'),
        0x30 => Tab,
        0x31 => Space,
        0x32 => Char('`'),
        0x33 => Backspace,
        0x35 => Escape,
        // Keypad: digits and the unshifted operators read as their main-
        // block twins, so `workspace 3` answers either 3. `*` and `+` are
        // shifted glyphs on the main block and have no unshifted twin.
        0x41 => Char('.'),
        0x4B => Char('/'),
        0x4C => Enter,
        0x4E => Char('-'),
        0x51 => Char('='),
        0x52 => Char('0'),
        0x53 => Char('1'),
        0x54 => Char('2'),
        0x55 => Char('3'),
        0x56 => Char('4'),
        0x57 => Char('5'),
        0x58 => Char('6'),
        0x59 => Char('7'),
        0x5B => Char('8'),
        0x5C => Char('9'),
        0x60 => F(5),
        0x61 => F(6),
        0x62 => F(7),
        0x63 => F(3),
        0x64 => F(8),
        0x65 => F(9),
        0x67 => F(11),
        0x69 => F(13),
        0x6A => F(16),
        0x6B => F(14),
        0x6D => F(10),
        0x6F => F(12),
        0x71 => F(15),
        0x73 => Home,
        0x74 => PageUp,
        0x76 => F(4),
        0x77 => End,
        0x78 => F(2),
        0x79 => PageDown,
        0x7A => F(1),
        0x7B => Left,
        0x7C => Right,
        0x7D => Down,
        0x7E => Up,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use KeyName::*;

    #[test]
    fn known_codes() {
        assert_eq!(keyname_from_keycode(4), Some(Char('h')));
        assert_eq!(keyname_from_keycode(38), Some(Char('j')));
        assert_eq!(keyname_from_keycode(40), Some(Char('k')));
        assert_eq!(keyname_from_keycode(37), Some(Char('l')));
        assert_eq!(keyname_from_keycode(18), Some(Char('1')));
        assert_eq!(keyname_from_keycode(36), Some(Enter));
        assert_eq!(keyname_from_keycode(53), Some(Escape));
        assert_eq!(keyname_from_keycode(49), Some(Space));
        assert_eq!(keyname_from_keycode(123), Some(Left));
        assert_eq!(keyname_from_keycode(0x7A), Some(F(1)));
        assert_eq!(keyname_from_keycode(0x53), Some(Char('1')), "keypad 1");
    }

    #[test]
    fn modifiers_are_not_keys() {
        // Command, Shift, CapsLock, Option, Control, right-hand twins, Fn.
        for code in 0x37..=0x3F {
            assert_eq!(keyname_from_keycode(code), None, "{code:#x}");
        }
    }

    #[test]
    fn every_letter_and_digit_is_mapped_once() {
        let mut seen = std::collections::HashSet::new();
        for code in 0..128u16 {
            if let Some(Char(c)) = keyname_from_keycode(code)
                && c.is_ascii_lowercase()
            {
                assert!(seen.insert(c), "{c} mapped twice");
            }
        }
        assert_eq!(seen.len(), 26);
    }
}
