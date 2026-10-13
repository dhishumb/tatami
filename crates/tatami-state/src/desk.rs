//! A desk is the set of monitors connected right now, named by their
//! uuids. Each desk keeps its own layout file (`desks/<id>.json`, the
//! `SavedState` shape), so a laptop that is docked to two monitors at work
//! and three at home comes back to the arrangement it had on each. The id
//! is a hash of the sorted uuids: stable across reboots and independent
//! of the order macOS lists the displays in.

use std::path::Path;

use crate::schema::SavedState;
use crate::store;

/// Fingerprint of a monitor set: FNV-1a over the sorted uuids, 16 hex
/// digits. Std only; the hash needs to be stable, not fast or secret.
pub fn desk_id_of<S: AsRef<str>>(uuids: &[S]) -> String {
    let mut v: Vec<&str> = uuids.iter().map(AsRef::as_ref).collect();
    v.sort_unstable();
    v.dedup();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for u in v {
        for b in u.bytes().chain(std::iter::once(b'\n')) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("{h:016x}")
}

/// A desk as the menu and `desk list` show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeskInfo {
    pub id: String,
    /// `3 monitors: A, B, C`, left to right.
    pub name: String,
    pub monitors: usize,
    pub saved_at: String,
}

impl SavedState {
    /// The desk this layout belongs to.
    pub fn desk_id(&self) -> String {
        let uuids: Vec<&str> = self.monitors.iter().map(|m| m.uuid.as_str()).collect();
        desk_id_of(&uuids)
    }

    /// `2 monitors: Built-in Retina Display, DELL U2723QE`, left to right.
    pub fn desk_name(&self) -> String {
        let mut mons: Vec<_> = self.monitors.iter().collect();
        mons.sort_by(|a, b| a.frame.x.total_cmp(&b.frame.x));
        let names: Vec<&str> = mons
            .iter()
            .map(|m| {
                if m.name.is_empty() {
                    "?"
                } else {
                    m.name.as_str()
                }
            })
            .collect();
        desk_name(&names)
    }

    pub fn desk_info(&self) -> DeskInfo {
        DeskInfo {
            id: self.desk_id(),
            name: self.desk_name(),
            monitors: self.monitors.len(),
            saved_at: self.saved_at.clone(),
        }
    }
}

pub fn desk_name(names: &[&str]) -> String {
    let n = names.len();
    format!(
        "{n} monitor{}: {}",
        if n == 1 { "" } else { "s" },
        names.join(", ")
    )
}

/// Every saved desk in `dir`, most monitors first then by name; a file
/// that does not parse is skipped (and logged by the daemon's load path
/// when it is actually used).
pub fn list_desks(dir: &Path) -> Vec<DeskInfo> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<DeskInfo> = rd
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension()?.to_str()? != "json" {
                return None;
            }
            let s = store::load(&p).ok()??;
            let mut info = s.desk_info();
            // The file name is the id the daemon wrote it under; trust it
            // over a recomputation so a hand-copied file still lists.
            info.id = p.file_stem()?.to_str()?.to_string();
            Some(info)
        })
        .collect();
    out.sort_by(|a, b| {
        b.monitors
            .cmp(&a.monitors)
            .then_with(|| a.name.cmp(&b.name))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::SavedMonitor;
    use tatami_protocol::Rect;

    fn mon(uuid: &str, name: &str, x: f64) -> SavedMonitor {
        SavedMonitor {
            uuid: uuid.into(),
            name: name.into(),
            frame: Rect::new(x, 0.0, 100.0, 100.0),
            active: "1".into(),
            workspaces: Vec::new(),
        }
    }

    #[test]
    fn the_id_ignores_order_and_the_name_reads_left_to_right() {
        assert_eq!(desk_id_of(&["b", "a"]), desk_id_of(&["a", "b"]));
        assert_ne!(desk_id_of(&["a"]), desk_id_of(&["a", "b"]));
        assert_eq!(desk_id_of(&["a", "b"]).len(), 16);
        let s = SavedState {
            version: 1,
            saved_at: String::new(),
            model: "per-monitor".into(),
            focused_monitor: None,
            monitors: vec![mon("u2", "Right", 100.0), mon("u1", "Left", 0.0)],
        };
        assert_eq!(s.desk_name(), "2 monitors: Left, Right");
        assert_eq!(s.desk_id(), desk_id_of(&["u1", "u2"]));
    }

    #[test]
    fn listing_reads_the_directory() {
        let dir = std::env::temp_dir().join(format!("tatami-desks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let one = SavedState {
            version: 1,
            saved_at: "t".into(),
            model: "per-monitor".into(),
            focused_monitor: None,
            monitors: vec![mon("u1", "Only", 0.0)],
        };
        let two = SavedState {
            monitors: vec![mon("u1", "Left", 0.0), mon("u2", "Right", 100.0)],
            ..one.clone()
        };
        store::save(&dir.join(format!("{}.json", one.desk_id())), &one).unwrap();
        store::save(&dir.join(format!("{}.json", two.desk_id())), &two).unwrap();
        std::fs::write(dir.join("junk.json"), "{").unwrap();
        let list = list_desks(&dir);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "2 monitors: Left, Right");
        assert_eq!(list[0].id, two.desk_id());
        assert_eq!(list[1].monitors, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
