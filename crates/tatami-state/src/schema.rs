//! Version 1. Add fields with `#[serde(default)]`; never repurpose one.
//! `tatami checkpoint` writes exactly this shape too.

use serde::{Deserialize, Serialize};
use tatami_protocol::{Layout, Rect};

pub const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedState {
    pub version: u32,
    /// RFC 3339, informational only.
    #[serde(default)]
    pub saved_at: String,
    /// `per-monitor` or `shared`; a mismatch with the running config means
    /// the file is set aside, not merged.
    pub model: String,
    #[serde(default)]
    pub focused_monitor: Option<String>,
    pub monitors: Vec<SavedMonitor>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedMonitor {
    pub uuid: String,
    #[serde(default)]
    pub name: String,
    /// Full frame when saved; used as the last-resort identity for a
    /// monitor whose uuid changed.
    pub frame: Rect,
    /// Visible workspace name.
    pub active: String,
    pub workspaces: Vec<SavedWorkspace>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedWorkspace {
    pub name: String,
    pub root: SavedNode,
    #[serde(default)]
    pub floating: Vec<SavedFloating>,
    /// Index into the dfs order of `root`'s windows.
    #[serde(default)]
    pub focused: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SavedNode {
    Container {
        layout: Layout,
        #[serde(default = "one")]
        weight: f32,
        children: Vec<SavedNode>,
    },
    Window {
        #[serde(default = "one")]
        weight: f32,
        window: SavedWindow,
    },
}

fn one() -> f32 {
    1.0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedFloating {
    pub window: SavedWindow,
    pub frame: Rect,
}

/// Enough to recognise a window again. `pid`/`cg_id` only help across a
/// daemon restart (same boot); after a reboot the app and title do the work.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedWindow {
    #[serde(default)]
    pub bundle_id: Option<String>,
    #[serde(default)]
    pub app: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub subrole: String,
    #[serde(default)]
    pub pid: Option<i32>,
    #[serde(default)]
    pub cg_id: Option<u64>,
    #[serde(default)]
    pub size: Option<(f64, f64)>,
}

impl SavedNode {
    /// Windows in dfs order, the order `SavedWorkspace::focused` indexes.
    pub fn windows(&self) -> Vec<&SavedWindow> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect<'a>(&'a self, out: &mut Vec<&'a SavedWindow>) {
        match self {
            SavedNode::Window { window, .. } => out.push(window),
            SavedNode::Container { children, .. } => children.iter().for_each(|c| c.collect(out)),
        }
    }

    pub fn window_count(&self) -> usize {
        match self {
            SavedNode::Window { .. } => 1,
            SavedNode::Container { children, .. } => {
                children.iter().map(SavedNode::window_count).sum()
            }
        }
    }
}

impl SavedState {
    pub fn window_count(&self) -> usize {
        self.monitors
            .iter()
            .flat_map(|m| m.workspaces.iter())
            .map(|w| w.root.window_count() + w.floating.len())
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untagged_nodes_round_trip_and_tolerate_missing_fields() {
        let json = r#"{
          "version": 1, "model": "per-monitor",
          "monitors": [{ "uuid": "u1", "frame": {"x":0,"y":0,"w":100,"h":100}, "active": "1",
            "workspaces": [{ "name": "1", "root": { "layout": "h_tiles", "children": [
               { "window": { "bundle_id": "a.b", "title": "t" } },
               { "weight": 2.0, "layout": "v_accordion", "children": [] } ] } }] }] }"#;
        let s: SavedState = serde_json::from_str(json).unwrap();
        let ws = &s.monitors[0].workspaces[0];
        match &ws.root {
            SavedNode::Container { children, .. } => {
                assert!(matches!(children[0], SavedNode::Window { weight, .. } if weight == 1.0));
                assert!(
                    matches!(children[1], SavedNode::Container { weight, .. } if weight == 2.0)
                );
            }
            _ => panic!("root must be a container"),
        }
        assert_eq!(s.window_count(), 1);
        let back = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<SavedState>(&back).unwrap(), s);
    }
}
