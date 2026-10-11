//! Bindings: chords → actions, with key modes on top.
//!
//! Two shapes coexist, as in plyterm:
//!
//! * **Direct chords** (`alt+h`, `alt+shift+3`) run their actions from
//!   normal mode. Only chords holding the leader are ever looked up there,
//!   so plain typing never touches this map.
//! * **Key modes** (`alt+w` → "workspaces", then `3`): a chord switches
//!   the mode, plain keys inside it run actions, and `normal` leaves.
//!   While a mode is active unbound keys are swallowed, except in a
//!   *passthrough* mode, where they reach the app.
//!
//! The map is generic over the action type so the dispatcher and the
//! which-key rows are tested with plain strings; tatami binds
//! `tatami_protocol::Action`. Everything here is built once at config
//! load; a key press is one `HashMap` probe.

use std::collections::HashMap;

use crate::chord::{KeyChord, KeyName, Mods};
use crate::state::{Level, ModeAction};

/// Renders a bound action list as a which-key description
/// (`focus left`, `+workspaces`).
pub type Describe<A> = fn(&[A]) -> String;
/// Which actions change the key mode; the rest are the caller's business.
pub type Classify<A> = fn(&A) -> Option<ModeAction>;

#[derive(Debug, Clone)]
pub struct ModeMap<A> {
    binds: HashMap<KeyChord, Vec<A>>,
    /// Insertion order, for stable help listings and "first chord wins"
    /// when aliases fold into one which-key row.
    order: Vec<KeyChord>,
    /// Unbound keys go to the app instead of being swallowed.
    pub passthrough: bool,
}

impl<A> Default for ModeMap<A> {
    fn default() -> Self {
        ModeMap {
            binds: HashMap::new(),
            order: Vec::new(),
            passthrough: false,
        }
    }
}

impl<A> ModeMap<A> {
    fn bind(&mut self, chord: KeyChord, actions: Vec<A>) {
        if self.binds.insert(chord, actions).is_none() {
            self.order.push(chord);
        }
    }

    fn unbind(&mut self, chord: &KeyChord) {
        if self.binds.remove(chord).is_some() {
            self.order.retain(|c| c != chord);
        }
    }

    pub fn get(&self, chord: &KeyChord) -> Option<&[A]> {
        self.binds.get(chord).map(Vec::as_slice)
    }

    /// Bindings in definition order.
    pub fn iter(&self) -> impl Iterator<Item = (&KeyChord, &[A])> {
        self.order
            .iter()
            .filter_map(|c| self.binds.get(c).map(|a| (c, a.as_slice())))
    }

    pub fn len(&self) -> usize {
        self.binds.len()
    }

    pub fn is_empty(&self) -> bool {
        self.binds.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct Keymap<A> {
    direct: ModeMap<A>,
    modes: HashMap<String, ModeMap<A>>,
    /// Mode names in definition order (help, `list-modes`).
    mode_order: Vec<String>,
    /// The held modifier that makes a chord a leader chord. Stripped from
    /// the top-level sheet, since it is already under your finger.
    leader: Mods,
    describe: Describe<A>,
    classify: Classify<A>,
}

impl<A> Keymap<A> {
    /// An empty map with Alt as the leader.
    pub fn new(describe: Describe<A>, classify: Classify<A>) -> Self {
        Keymap {
            direct: ModeMap::default(),
            modes: HashMap::new(),
            mode_order: Vec::new(),
            leader: Mods::ALT,
            describe,
            classify,
        }
    }

    pub fn leader(&self) -> Mods {
        self.leader
    }

    pub fn set_leader(&mut self, leader: Mods) {
        self.leader = leader;
    }

    pub fn classify(&self, action: &A) -> Option<ModeAction> {
        (self.classify)(action)
    }

    pub fn describe(&self, actions: &[A]) -> String {
        (self.describe)(actions)
    }

    fn mode_mut(&mut self, mode: &str) -> &mut ModeMap<A> {
        if !self.modes.contains_key(mode) {
            self.mode_order.push(mode.to_string());
        }
        self.modes.entry(mode.to_string()).or_default()
    }

    /// Bind (or rebind) a chord, directly (`None`) or inside a mode. A
    /// mode comes into existence the first time something binds into it.
    pub fn bind(&mut self, mode: Option<&str>, chord: KeyChord, actions: Vec<A>) {
        match mode {
            None => self.direct.bind(chord, actions),
            Some(m) => self.mode_mut(m).bind(chord, actions),
        }
    }

    /// Forget a chord, so it reaches the app again (`"alt+f" = []`).
    pub fn unbind(&mut self, mode: Option<&str>, chord: &KeyChord) {
        match mode {
            None => self.direct.unbind(chord),
            Some(m) => {
                if let Some(mm) = self.modes.get_mut(m) {
                    mm.unbind(chord);
                }
            }
        }
    }

    pub fn set_passthrough(&mut self, mode: &str) {
        self.mode_mut(mode).passthrough = true;
    }

    /// Declare a mode without binding anything into it yet.
    pub fn add_mode(&mut self, mode: &str) {
        self.mode_mut(mode);
    }

    pub fn has_mode(&self, mode: &str) -> bool {
        self.modes.contains_key(mode)
    }

    pub fn is_passthrough(&self, mode: &str) -> bool {
        self.modes.get(mode).is_some_and(|m| m.passthrough)
    }

    /// Mode names in definition order.
    pub fn modes(&self) -> &[String] {
        &self.mode_order
    }

    pub fn direct(&self) -> &ModeMap<A> {
        &self.direct
    }

    pub fn mode(&self, mode: &str) -> Option<&ModeMap<A>> {
        self.modes.get(mode)
    }

    pub fn lookup_direct(&self, chord: &KeyChord) -> Option<&[A]> {
        self.direct.get(chord)
    }

    pub fn lookup_mode(&self, mode: &str, chord: &KeyChord) -> Option<&[A]> {
        self.modes.get(mode)?.get(chord)
    }

    /// Every binding: direct ones (`None`) first, then each mode's.
    pub fn bindings(&self) -> impl Iterator<Item = (Option<&str>, &KeyChord, &[A])> {
        let direct = self.direct.iter().map(|(c, a)| (None, c, a));
        let modes = self
            .mode_order
            .iter()
            .filter_map(|m| self.modes.get(m).map(|mm| (m.as_str(), mm)))
            .flat_map(|(m, mm)| mm.iter().map(move |(c, a)| (Some(m), c, a)));
        direct.chain(modes)
    }

    /// The which-key sheet for one level: what a key does from here,
    /// alphabetised the way doom's popup is (WHICH_KEY.md 1.4).
    ///
    /// At the top level the leader is held, so it is dropped from the
    /// chords — the sheet says `h`, not `alt+h` — and chords without it
    /// are left out, since normal mode never looks them up. A key that
    /// opens another level is a prefix and reads `+workspaces`. Esc /
    /// Enter → normal are chrome, not rows. Aliases fold into one row
    /// showing the first chord, and `workspace 1` … `workspace 9` fold
    /// into `1-9 → workspace 1-9`.
    pub fn which_key(&self, level: &Level) -> Vec<WhichKeyEntry> {
        let (map, strip, here) = match level {
            Level::Top => (&self.direct, self.leader, None),
            Level::Mode(m) => match self.modes.get(m) {
                Some(mm) => (mm, Mods::NONE, Some(m.as_str())),
                None => return Vec::new(),
            },
        };
        let mut rows: Vec<(SortKey, WhichKeyEntry)> = Vec::new();
        // (modifiers, description stem, row index, lowest digit, highest digit)
        let mut folds: Vec<(Mods, String, usize, char, char)> = Vec::new();
        for (chord, actions) in map.iter() {
            if here.is_none() && !chord.mods.contains(self.leader) {
                continue;
            }
            // A row either does something or opens another level; a list
            // that only leaves (or re-enters) this one is chrome.
            let mut does = false;
            let mut opens = false;
            for a in actions {
                match (self.classify)(a) {
                    Some(ModeAction::Enter(m)) => opens |= Some(m.as_str()) != here,
                    Some(ModeAction::Normal) => {}
                    Some(ModeAction::Help) | None => does = true,
                }
            }
            if !does && !opens {
                continue;
            }
            let desc = (self.describe)(actions);
            if let KeyName::Char(d @ '0'..='9') = chord.key
                && let Some(stem) = desc.strip_suffix(d)
                && stem.ends_with(' ')
            {
                if let Some(f) = folds.iter_mut().find(|f| f.0 == chord.mods && f.1 == stem) {
                    f.3 = f.3.min(d);
                    f.4 = f.4.max(d);
                } else {
                    folds.push((chord.mods, stem.to_string(), rows.len(), d, d));
                    rows.push((
                        sort_key(chord, strip),
                        WhichKeyEntry {
                            key: String::new(),
                            desc,
                            prefix: false,
                        },
                    ));
                }
                continue;
            }
            if rows.iter().any(|(_, e)| e.desc == desc) {
                continue;
            }
            rows.push((
                sort_key(chord, strip),
                WhichKeyEntry {
                    key: chord.display_relative(strip),
                    desc,
                    prefix: opens && !does,
                },
            ));
        }
        for (mods, stem, i, lo, hi) in folds {
            let first = KeyChord::new(KeyName::Char(lo), mods);
            let (sort, e) = &mut rows[i];
            *sort = sort_key(&first, strip);
            e.key = first.display_relative(strip);
            e.desc = stem;
            e.desc.push(lo);
            if hi != lo {
                for s in [&mut e.key, &mut e.desc] {
                    s.push('-');
                    s.push(hi);
                }
            }
        }
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows.into_iter().map(|(_, e)| e).collect()
    }

    /// The chords that lead to `mode`, shallowest first: `[alt+w]` for
    /// workspaces, `[alt+s, r]` for a mode entered from the service mode.
    /// Empty when nothing binds it (a mode entered from the CLI).
    pub fn mode_path(&self, mode: &str) -> Vec<KeyChord> {
        let enters = |map: &ModeMap<A>, target: &str| -> Option<KeyChord> {
            map.iter()
                .find(|(_, acts)| {
                    acts.iter().any(
                        |a| matches!((self.classify)(a), Some(ModeAction::Enter(m)) if m == target),
                    )
                })
                .map(|(c, _)| *c)
        };
        if let Some(c) = enters(&self.direct, mode) {
            return vec![c];
        }
        for parent in &self.mode_order {
            if let Some(mm) = self.modes.get(parent)
                && let Some(c) = enters(mm, mode)
                && let Some(p) = enters(&self.direct, parent)
            {
                return vec![p, c];
            }
        }
        Vec::new()
    }
}

/// One row of the which-key sheet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhichKeyEntry {
    pub key: String,
    pub desc: String,
    /// Pressing it opens another level instead of doing something.
    pub prefix: bool,
}

/// (group, name, shifted): doom's `which-key-key-order-alpha`, lowercase
/// first. Group 0 is plain keys (`a`, `H`, `?`, `1-9`), 1 named keys
/// (`space`, `left`), 2 chords that still carry a modifier at this level.
type SortKey = (u8, String, bool);

fn sort_key(chord: &KeyChord, strip: Mods) -> SortKey {
    let m = chord.mods.without(strip);
    let shown = KeyChord::new(chord.key, m).display_relative(Mods::NONE);
    // Shifted letters and punctuation show as one glyph (`H`, `?`) and
    // sort as plain keys; a shifted digit or named key still reads
    // `shift+…` and sorts with the modified chords.
    let group = if m.cmd || m.ctrl || m.alt || shown.starts_with("shift+") {
        2
    } else if matches!(chord.key, KeyName::Char(_)) {
        0
    } else {
        1
    };
    (group, shown.to_lowercase(), m.shift)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn c(s: &str) -> KeyChord {
        KeyChord::parse(s).unwrap_or_else(|| panic!("chord {s}"))
    }

    fn describe(actions: &[String]) -> String {
        let doing: Vec<&str> = actions
            .iter()
            .map(String::as_str)
            .filter(|a| classify_str(a).is_none())
            .collect();
        if let Some(first) = doing.first() {
            return first.to_string();
        }
        match actions
            .first()
            .map(String::as_str)
            .and_then(|a| a.strip_prefix("mode "))
        {
            Some(m) => format!("+{m}"),
            None => actions.first().cloned().unwrap_or_default(),
        }
    }

    pub(crate) fn classify_str(a: &str) -> Option<ModeAction> {
        match a {
            "normal" => Some(ModeAction::Normal),
            "help" => Some(ModeAction::Help),
            _ => a
                .strip_prefix("mode ")
                .map(|m| ModeAction::Enter(m.to_string())),
        }
    }

    /// A small tatami-shaped map in plain strings.
    pub(crate) fn sample() -> Keymap<String> {
        let mut km = Keymap::new(describe, |a: &String| classify_str(a));
        let b = |km: &mut Keymap<String>, mode: Option<&str>, chord: &str, acts: &[&str]| {
            km.bind(mode, c(chord), acts.iter().map(|s| s.to_string()).collect())
        };
        for (k, d) in [("h", "left"), ("j", "down"), ("k", "up"), ("l", "right")] {
            b(&mut km, None, &format!("alt+{k}"), &[&format!("focus {d}")]);
            b(
                &mut km,
                None,
                &format!("alt+shift+{k}"),
                &[&format!("move {d}")],
            );
        }
        b(&mut km, None, "alt+left", &["focus left"]);
        for n in 1..=9 {
            b(
                &mut km,
                None,
                &format!("alt+{n}"),
                &[&format!("workspace {n}")],
            );
            b(
                &mut km,
                None,
                &format!("alt+shift+{n}"),
                &[
                    &format!("move-node-to-workspace {n}"),
                    &format!("workspace {n}"),
                ],
            );
        }
        b(&mut km, None, "alt+tab", &["workspace-back-and-forth"]);
        b(&mut km, None, "alt+/", &["help"]);
        b(&mut km, None, "alt+w", &["mode workspaces"]);
        b(&mut km, None, "alt+r", &["mode resize"]);
        b(&mut km, None, "cmd+x", &["close"]);
        b(&mut km, None, "ctrl+alt+x", &["close"]);
        for mode in ["workspaces", "resize"] {
            b(&mut km, Some(mode), "esc", &["normal"]);
            b(&mut km, Some(mode), "enter", &["normal"]);
        }
        for n in 1..=9 {
            b(
                &mut km,
                Some("workspaces"),
                &n.to_string(),
                &[&format!("workspace {n}"), "normal"],
            );
        }
        b(
            &mut km,
            Some("workspaces"),
            "l",
            &["workspace next", "normal"],
        );
        b(&mut km, Some("workspaces"), "r", &["mode resize"]);
        b(&mut km, Some("resize"), "h", &["resize width -50"]);
        b(&mut km, Some("resize"), "H", &["resize width -200"]);
        b(&mut km, Some("resize"), "=", &["balance-sizes"]);
        b(&mut km, Some("resize"), "+", &["resize smart +50"]);
        b(&mut km, Some("resize"), "space", &["resize smart +10"]);
        km
    }

    #[test]
    fn lookups() {
        let km = sample();
        assert_eq!(
            km.lookup_direct(&c("alt+h")),
            Some(&["focus left".to_string()][..])
        );
        assert_eq!(
            km.lookup_direct(&c("alt-shift-h")),
            Some(&["move left".to_string()][..])
        );
        assert!(km.lookup_direct(&c("h")).is_none());
        assert_eq!(
            km.lookup_mode("workspaces", &c("3")),
            Some(&["workspace 3".to_string(), "normal".to_string()][..])
        );
        assert!(km.lookup_mode("nowhere", &c("3")).is_none());
        assert!(km.has_mode("resize") && !km.is_passthrough("resize"));
        assert_eq!(km.modes(), ["workspaces", "resize"]);
        assert_eq!(
            km.bindings().count(),
            km.direct().len()
                + km.mode("workspaces").unwrap().len()
                + km.mode("resize").unwrap().len()
        );
    }

    #[test]
    fn rebind_keeps_order_and_unbind_forgets() {
        let mut km = sample();
        let first = *km.direct().iter().next().unwrap().0;
        km.bind(None, first, vec!["close".into()]);
        assert_eq!(*km.direct().iter().next().unwrap().0, first);
        km.unbind(None, &first);
        assert!(km.lookup_direct(&first).is_none());
        assert!(km.direct().iter().all(|(c, _)| *c != first));
    }

    #[test]
    fn which_key_top_level_drops_the_leader_and_marks_prefixes() {
        let km = sample();
        let top = km.which_key(&Level::Top);
        let find = |k: &str| top.iter().find(|e| e.key == k).cloned();
        let h = find("h").expect("alt+h shows as h");
        assert_eq!((h.desc.as_str(), h.prefix), ("focus left", false));
        assert_eq!(find("H").unwrap().desc, "move left");
        let w = find("w").expect("alt+w shows as w");
        assert_eq!((w.desc.as_str(), w.prefix), ("+workspaces", true));
        assert_eq!(find("/").unwrap().desc, "help");
        // Digit runs fold, plain and shifted separately.
        assert_eq!(find("1-9").unwrap().desc, "workspace 1-9");
        assert_eq!(
            find("shift+1-9").unwrap().desc,
            "move-node-to-workspace 1-9"
        );
        assert!(!top.iter().any(|e| e.desc == "workspace 3"));
        // alt+left folds into alt+h's row; cmd+x isn't a leader chord.
        assert!(find("left").is_none());
        assert_eq!(top.iter().filter(|e| e.desc == "close").count(), 1);
        assert_eq!(find("ctrl+x").unwrap().desc, "close");
        assert!(
            top.iter()
                .all(|e| !e.key.starts_with("alt+") && !e.key.starts_with("cmd+"))
        );
        // Alphabetical, lowercase before its shifted twin, plain before
        // named before modified.
        let keys: Vec<&str> = top.iter().map(|e| e.key.as_str()).collect();
        let pos = |k: &str| {
            keys.iter()
                .position(|x| *x == k)
                .unwrap_or_else(|| panic!("{k} in {keys:?}"))
        };
        assert!(pos("1-9") < pos("h") && pos("h") < pos("H") && pos("H") < pos("j"));
        assert!(pos("w") < pos("tab"));
        assert!(pos("tab") < pos("ctrl+x") && pos("tab") < pos("shift+1-9"));
    }

    #[test]
    fn which_key_mode_level_hides_chrome_and_keeps_prefixes() {
        let km = sample();
        let ws = km.which_key(&Level::Mode("workspaces".into()));
        assert!(ws.iter().all(|e| e.key != "esc" && e.key != "enter"));
        assert!(
            ws.iter()
                .any(|e| e.key == "1-9" && e.desc == "workspace 1-9" && !e.prefix)
        );
        let r = ws.iter().find(|e| e.key == "r").unwrap();
        assert_eq!((r.desc.as_str(), r.prefix), ("+resize", true));
        let rs = km.which_key(&Level::Mode("resize".into()));
        let keys: Vec<&str> = rs.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, ["+", "=", "h", "H", "space"]);
        assert!(km.which_key(&Level::Mode("nowhere".into())).is_empty());
    }

    #[test]
    fn mode_path_is_the_breadcrumb() {
        let km = sample();
        assert_eq!(km.mode_path("workspaces"), vec![c("alt+w")]);
        // resize is entered directly (alt+r) as well as from workspaces;
        // the shallowest path wins.
        assert_eq!(km.mode_path("resize"), vec![c("alt+r")]);
        let mut km = km;
        km.unbind(None, &c("alt+r"));
        assert_eq!(km.mode_path("resize"), vec![c("alt+w"), c("r")]);
        assert!(km.mode_path("nowhere").is_empty());
    }
}
