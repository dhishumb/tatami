//! Chords: how a binding is spelled in a keymap file and what the event
//! tap reports.
//!
//! Keys are *physical* US-ANSI positions, because that is what a hardware
//! keycode is: the tap sees `shift` + the `/` key, never a `?`. So a
//! binding written with a shifted glyph is stored as its base key plus
//! shift (`"?"` is `shift+/`, `"+"` is `shift+=`, `"!"` is `shift+1`),
//! and `alt+shift+1` really is a different chord from `alt+1`. Letters
//! fold the same way: `"H"` is `shift+h`.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Mods {
    pub alt: bool,
    pub shift: bool,
    pub ctrl: bool,
    pub cmd: bool,
}

impl Mods {
    pub const NONE: Mods = Mods {
        alt: false,
        shift: false,
        ctrl: false,
        cmd: false,
    };
    pub const ALT: Mods = Mods {
        alt: true,
        ..Mods::NONE
    };
    pub const SHIFT: Mods = Mods {
        shift: true,
        ..Mods::NONE
    };
    pub const CTRL: Mods = Mods {
        ctrl: true,
        ..Mods::NONE
    };
    pub const CMD: Mods = Mods {
        cmd: true,
        ..Mods::NONE
    };

    pub fn any(self) -> bool {
        self.alt || self.shift || self.ctrl || self.cmd
    }

    /// Every modifier in `other` is held here too (others may be as well).
    pub fn contains(self, other: Mods) -> bool {
        (self.alt || !other.alt)
            && (self.shift || !other.shift)
            && (self.ctrl || !other.ctrl)
            && (self.cmd || !other.cmd)
    }

    pub fn without(self, other: Mods) -> Mods {
        Mods {
            alt: self.alt && !other.alt,
            shift: self.shift && !other.shift,
            ctrl: self.ctrl && !other.ctrl,
            cmd: self.cmd && !other.cmd,
        }
    }

    pub fn union(self, other: Mods) -> Mods {
        Mods {
            alt: self.alt || other.alt,
            shift: self.shift || other.shift,
            ctrl: self.ctrl || other.ctrl,
            cmd: self.cmd || other.cmd,
        }
    }

    /// One modifier word, case-insensitive. The aliases are the ones
    /// people actually type: `opt`, `option` and `meta` are all Alt.
    fn word(w: &str) -> Option<Mods> {
        Some(match w.trim().to_ascii_lowercase().as_str() {
            "ctrl" | "control" | "c" => Mods::CTRL,
            "alt" | "opt" | "option" | "meta" => Mods::ALT,
            "shift" | "s" => Mods::SHIFT,
            "cmd" | "super" | "command" | "win" => Mods::CMD,
            _ => return None,
        })
    }

    /// `"alt"`, `"ctrl+alt"`, `"ctrl-alt"`: the `leader` setting. `None`
    /// for an empty or unknown word, so a typo is reported, not ignored.
    pub fn parse(s: &str) -> Option<Mods> {
        let sep = if s.contains('+') { '+' } else { '-' };
        let mut out = Mods::NONE;
        for w in s.split(sep) {
            out = out.union(Mods::word(w)?);
        }
        out.any().then_some(out)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyName {
    Char(char),
    Enter,
    Tab,
    Escape,
    Space,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    F(u8),
}

impl KeyName {
    fn name(self) -> Option<&'static str> {
        Some(match self {
            KeyName::Char(_) | KeyName::F(_) => return None,
            KeyName::Enter => "enter",
            KeyName::Tab => "tab",
            KeyName::Escape => "esc",
            KeyName::Space => "space",
            KeyName::Backspace => "backspace",
            KeyName::Up => "up",
            KeyName::Down => "down",
            KeyName::Left => "left",
            KeyName::Right => "right",
            KeyName::PageUp => "pageup",
            KeyName::PageDown => "pagedown",
            KeyName::Home => "home",
            KeyName::End => "end",
        })
    }

    /// A named key, lowercase. Includes AeroSpace's spelled-out
    /// punctuation (`minus`, `leftSquareBracket`) so its configs port.
    fn parse_named(lower: &str) -> Option<KeyName> {
        Some(match lower {
            "enter" | "return" | "cr" => KeyName::Enter,
            "tab" => KeyName::Tab,
            "esc" | "escape" => KeyName::Escape,
            "space" => KeyName::Space,
            "backspace" | "bs" | "delete" => KeyName::Backspace,
            "up" | "arrowup" => KeyName::Up,
            "down" | "arrowdown" => KeyName::Down,
            "left" | "arrowleft" => KeyName::Left,
            "right" | "arrowright" => KeyName::Right,
            "pageup" | "pgup" => KeyName::PageUp,
            "pagedown" | "pgdn" | "pgdown" => KeyName::PageDown,
            "home" => KeyName::Home,
            "end" => KeyName::End,
            "minus" => KeyName::Char('-'),
            "equal" => KeyName::Char('='),
            "semicolon" => KeyName::Char(';'),
            "quote" => KeyName::Char('\''),
            "comma" => KeyName::Char(','),
            "period" => KeyName::Char('.'),
            "slash" => KeyName::Char('/'),
            "backslash" => KeyName::Char('\\'),
            "backtick" | "grave" => KeyName::Char('`'),
            "leftsquarebracket" => KeyName::Char('['),
            "rightsquarebracket" => KeyName::Char(']'),
            f if f.len() > 1 && f.starts_with('f') => KeyName::F(f[1..].parse().ok()?),
            _ => return None,
        })
    }
}

/// The US-ANSI key a shifted glyph lives on: `?` → `/`.
fn unshift(c: char) -> Option<char> {
    Some(match c {
        '!' => '1',
        '@' => '2',
        '#' => '3',
        '$' => '4',
        '%' => '5',
        '^' => '6',
        '&' => '7',
        '*' => '8',
        '(' => '9',
        ')' => '0',
        '_' => '-',
        '+' => '=',
        '{' => '[',
        '}' => ']',
        '|' => '\\',
        ':' => ';',
        '"' => '\'',
        '<' => ',',
        '>' => '.',
        '?' => '/',
        '~' => '`',
        _ => return None,
    })
}

/// The glyph shift puts on a punctuation key, for display: `shift+/` is
/// shown as `?`. Digits are left alone on purpose — `alt+shift+1` reads
/// as "move to 1", `alt+!` reads as nothing.
fn shifted(c: char) -> Option<char> {
    Some(match c {
        '-' => '_',
        '=' => '+',
        '[' => '{',
        ']' => '}',
        '\\' => '|',
        ';' => ':',
        '\'' => '"',
        ',' => '<',
        '.' => '>',
        '/' => '?',
        '`' => '~',
        _ => return None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyChord {
    pub key: KeyName,
    pub mods: Mods,
}

impl KeyChord {
    pub const fn new(key: KeyName, mods: Mods) -> Self {
        KeyChord { key, mods }
    }

    /// Parse `"alt+shift+h"`, `"alt-shift-h"` (AeroSpace), `"cmd+["`,
    /// `"alt+left"`, `"1"`, `"?"`, `"ctrl++"`, `"alt--"`. Uppercase
    /// letters and shifted glyphs carry their shift (see the module doc).
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        // `+` is the separator whenever one appears; AeroSpace's `-` only
        // when there is no `+` at all, so `alt+-` is alt and the minus key.
        let sep = if s.contains('+') { '+' } else { '-' };
        // A trailing separator is the separator key itself: `ctrl++`
        // splits into ["ctrl", "", ""], `-` into ["", ""].
        let mut parts: Vec<&str> = s.split(sep).collect();
        let mut key_str = parts.pop()?.trim();
        if key_str.is_empty() {
            key_str = if sep == '+' { "+" } else { "-" };
            if parts.last().is_some_and(|p| p.is_empty()) {
                parts.pop();
            }
        }
        let mut mods = Mods::NONE;
        for m in parts {
            mods = mods.union(Mods::word(m)?);
        }
        let key = match KeyName::parse_named(&key_str.to_ascii_lowercase()) {
            Some(k) => k,
            None => {
                let mut chars = key_str.chars();
                let c = chars.next()?;
                if chars.next().is_some() {
                    return None;
                }
                if c.is_alphabetic() {
                    if c.is_uppercase() {
                        mods.shift = true;
                    }
                    KeyName::Char(c.to_lowercase().next().unwrap_or(c))
                } else if let Some(base) = unshift(c) {
                    mods.shift = true;
                    KeyName::Char(base)
                } else {
                    KeyName::Char(c)
                }
            }
        };
        Some(KeyChord { key, mods })
    }

    /// The chord as the which-key sheet shows it: modifiers already
    /// under your finger dropped (with Alt held, `alt+h` reads `h`), and
    /// shift folded into the glyph it makes (`H`, `?`, `+`).
    pub fn display_relative(&self, strip: Mods) -> String {
        let mut s = String::new();
        // Writing into a String cannot fail.
        let _ = self.write(&mut s, strip, true);
        s
    }

    fn write(&self, f: &mut impl fmt::Write, strip: Mods, upper: bool) -> fmt::Result {
        let m = self.mods.without(strip);
        for (on, word) in [(m.cmd, "cmd+"), (m.ctrl, "ctrl+"), (m.alt, "alt+")] {
            if on {
                f.write_str(word)?;
            }
        }
        match self.key {
            KeyName::Char(c) if m.shift && upper && c.is_alphabetic() => {
                for u in c.to_uppercase() {
                    f.write_char(u)?;
                }
                Ok(())
            }
            KeyName::Char(c) => match shifted(c) {
                Some(g) if m.shift => f.write_char(g),
                _ => {
                    if m.shift {
                        f.write_str("shift+")?;
                    }
                    f.write_char(c)
                }
            },
            k => {
                if m.shift {
                    f.write_str("shift+")?;
                }
                match (k, k.name()) {
                    (KeyName::F(n), _) => write!(f, "f{n}"),
                    (_, Some(name)) => f.write_str(name),
                    _ => Ok(()),
                }
            }
        }
    }
}

/// The canonical spelling, which `parse` reads back: `alt+shift+h`,
/// `alt+?`, `ctrl+left`.
impl fmt::Display for KeyChord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write(f, Mods::NONE, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(s: &str) -> KeyChord {
        KeyChord::parse(s).unwrap_or_else(|| panic!("chord {s}"))
    }

    const fn k(ch: char, mods: Mods) -> KeyChord {
        KeyChord::new(KeyName::Char(ch), mods)
    }

    #[test]
    fn parses_chords() {
        let x = c("ctrl+shift+d");
        assert!(x.mods.ctrl && x.mods.shift && !x.mods.cmd);
        assert_eq!(x.key, KeyName::Char('d'));
        assert_eq!(c("D"), k('d', Mods::SHIFT));
        assert_eq!(c("alt+left").key, KeyName::Left);
        assert_eq!(
            c("ctrl++"),
            k(
                '=',
                Mods {
                    ctrl: true,
                    shift: true,
                    ..Mods::NONE
                }
            )
        );
        assert_eq!(c("f5").key, KeyName::F(5));
        assert_eq!(c("f").key, KeyName::Char('f'));
        assert_eq!(c("option+h"), k('h', Mods::ALT));
        assert_eq!(c("command+h"), k('h', Mods::CMD));
        assert!(KeyChord::parse("hyper+x").is_none());
        assert!(KeyChord::parse("ab").is_none());
        assert!(KeyChord::parse("").is_none());
    }

    #[test]
    fn keys_are_physical() {
        // `?` is the `/` key with shift; the tap can only ever report that.
        assert_eq!(
            c("cmd+?"),
            k(
                '/',
                Mods {
                    cmd: true,
                    shift: true,
                    ..Mods::NONE
                }
            )
        );
        assert_eq!(c("alt+shift+/"), c("alt+?"));
        assert_eq!(c("+"), k('=', Mods::SHIFT));
        assert_eq!(c("="), k('=', Mods::NONE));
        // Digits keep an explicit shift: alt+shift+1 is its own chord.
        assert_eq!(
            c("alt+shift+1"),
            k(
                '1',
                Mods {
                    alt: true,
                    shift: true,
                    ..Mods::NONE
                }
            )
        );
        assert_ne!(c("alt+shift+1"), c("alt+1"));
        assert_eq!(c("alt+!"), c("alt+shift+1"));
    }

    #[test]
    fn aerospace_dash_spelling() {
        assert_eq!(c("alt-shift-h"), c("alt+shift+h"));
        assert_eq!(c("alt-h"), k('h', Mods::ALT));
        assert_eq!(c("-"), k('-', Mods::NONE));
        assert_eq!(c("alt--"), k('-', Mods::ALT));
        assert_eq!(c("alt+-"), k('-', Mods::ALT));
        assert_eq!(c("alt-minus"), k('-', Mods::ALT));
        assert_eq!(c("alt-shift-semicolon"), c("alt+:"));
        assert_eq!(c("alt-leftSquareBracket"), k('[', Mods::ALT));
        assert_eq!(
            c("ctrl-alt-enter"),
            KeyChord::new(
                KeyName::Enter,
                Mods {
                    ctrl: true,
                    alt: true,
                    ..Mods::NONE
                }
            )
        );
    }

    #[test]
    fn display_round_trips() {
        for s in [
            "alt+h",
            "alt+shift+h",
            "alt+?",
            "alt+shift+1",
            "ctrl+left",
            "esc",
            "f12",
            "+",
            "-",
            "cmd+ctrl+alt+x",
        ] {
            assert_eq!(c(s).to_string(), s);
            assert_eq!(c(&c(s).to_string()), c(s));
        }
        assert_eq!(c("H").to_string(), "shift+h");
    }

    #[test]
    fn display_relative_drops_held_modifiers() {
        assert_eq!(c("alt+h").display_relative(Mods::ALT), "h");
        assert_eq!(c("alt+shift+h").display_relative(Mods::ALT), "H");
        assert_eq!(c("alt+shift+1").display_relative(Mods::ALT), "shift+1");
        assert_eq!(c("alt+/").display_relative(Mods::ALT), "/");
        assert_eq!(c("alt+tab").display_relative(Mods::ALT), "tab");
        assert_eq!(c("ctrl+alt+x").display_relative(Mods::ALT), "ctrl+x");
        assert_eq!(c("+").display_relative(Mods::NONE), "+");
    }

    #[test]
    fn mods_parse_and_set_ops() {
        assert_eq!(Mods::parse("alt"), Some(Mods::ALT));
        assert_eq!(
            Mods::parse("ctrl+alt"),
            Some(Mods {
                ctrl: true,
                alt: true,
                ..Mods::NONE
            })
        );
        assert_eq!(Mods::parse("ctrl-opt"), Mods::parse("control+alt"));
        assert_eq!(Mods::parse("hyper"), None);
        assert_eq!(Mods::parse(""), None);
        let hyper = Mods {
            ctrl: true,
            alt: true,
            ..Mods::NONE
        };
        assert!(hyper.contains(Mods::ALT) && !Mods::ALT.contains(hyper));
        assert_eq!(hyper.without(Mods::ALT), Mods::CTRL);
    }
}
