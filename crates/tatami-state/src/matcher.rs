//! Which live window is which saved slot. Bundle id is a hard requirement;
//! the rest is a score so that two Ghostty windows go back where their
//! titles say, and an app with no stable titles still fills its slots in
//! order. Pure, so the reactor's restore session can be tested without
//! windows.

use crate::schema::SavedWindow;

/// The live side of the comparison.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchInput<'a> {
    pub bundle_id: Option<&'a str>,
    pub app: &'a str,
    pub title: &'a str,
    pub subrole: &'a str,
    pub pid: i32,
    pub cg_id: u64,
    pub size: (f64, f64),
}

/// `None` = cannot be this window (different app). `Some(0)` = same app,
/// nothing else matches; higher is better.
pub fn score(saved: &SavedWindow, live: &MatchInput<'_>) -> Option<u32> {
    let same_app = match (&saved.bundle_id, live.bundle_id) {
        (Some(a), Some(b)) => a == b,
        // Apps without a bundle id (bare binaries) fall back to the name.
        _ => !saved.app.is_empty() && saved.app == live.app,
    };
    if !same_app {
        return None;
    }
    let mut s = 0;
    // Same boot, same process: the window id is authoritative.
    if saved.pid == Some(live.pid) && saved.cg_id == Some(live.cg_id) {
        s += 8;
    }
    if !saved.title.is_empty() && saved.title == live.title {
        s += 3;
    } else if !saved.title.is_empty()
        && normalize_title(&saved.title) == normalize_title(live.title)
    {
        s += 2;
    }
    if !saved.subrole.is_empty() && saved.subrole == live.subrole {
        s += 1;
    }
    if let Some((w, h)) = saved.size
        && (w - live.size.0).abs() < 2.0
        && (h - live.size.1).abs() < 2.0
    {
        s += 1;
    }
    Some(s)
}

/// Strip the parts of a title that change between sessions: edited
/// markers, counts in brackets, trailing " — <app>" suffixes.
fn normalize_title(t: &str) -> String {
    let mut s = t.trim().to_string();
    for marker in [" — Edited", " - Edited", " •", "•"] {
        if let Some(stripped) = s.strip_suffix(marker) {
            s = stripped.to_string();
        }
    }
    // "(3)" unread counts and similar.
    while let (Some(open), Some(close)) = (s.rfind('('), s.rfind(')')) {
        if open < close && s[open + 1..close].chars().all(|c| c.is_ascii_digit()) {
            s.replace_range(open..=close, "");
            s = s.trim_end().to_string();
        } else {
            break;
        }
    }
    // "<document> — <app>": keep the document.
    if let Some(idx) = s.rfind(" — ") {
        s.truncate(idx);
    } else if let Some(idx) = s.rfind(" - ") {
        s.truncate(idx);
    }
    s.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(bundle: &str, title: &str) -> SavedWindow {
        SavedWindow {
            bundle_id: Some(bundle.into()),
            app: "App".into(),
            title: title.into(),
            subrole: "AXStandardWindow".into(),
            pid: None,
            cg_id: None,
            size: None,
        }
    }

    fn live<'a>(bundle: &'a str, title: &'a str) -> MatchInput<'a> {
        MatchInput {
            bundle_id: Some(bundle),
            app: "App",
            title,
            subrole: "AXStandardWindow",
            pid: 1,
            cg_id: 1,
            size: (0.0, 0.0),
        }
    }

    #[test]
    fn different_app_never_matches() {
        assert_eq!(score(&saved("a", "x"), &live("b", "x")), None);
    }

    #[test]
    fn exact_title_beats_normalized_beats_nothing() {
        let s = saved("a", "notes.md — Edited");
        let exact = score(&s, &live("a", "notes.md — Edited")).unwrap();
        let norm = score(&s, &live("a", "notes.md")).unwrap();
        let none = score(&s, &live("a", "other")).unwrap();
        assert!(exact > norm && norm > none);
        assert_eq!(none, 1, "subrole still counts");
    }

    #[test]
    fn same_boot_window_id_dominates() {
        let mut s = saved("a", "x");
        s.pid = Some(1);
        s.cg_id = Some(1);
        let by_id = score(&s, &live("a", "completely different")).unwrap();
        let by_title = score(&saved("a", "x"), &live("a", "x")).unwrap();
        assert!(by_id > by_title);
    }

    #[test]
    fn title_normalization() {
        assert_eq!(normalize_title("Inbox (12) — Mail"), "inbox");
        assert_eq!(
            normalize_title("main.rs — tatami — Code"),
            "main.rs — tatami"
        );
        assert_eq!(normalize_title("Draft •"), "draft");
    }

    #[test]
    fn apps_without_bundle_match_by_name() {
        let mut s = saved("", "t");
        s.bundle_id = None;
        let mut l = live("", "t");
        l.bundle_id = None;
        assert!(score(&s, &l).is_some());
        l.app = "Other";
        assert!(score(&s, &l).is_none());
    }
}
