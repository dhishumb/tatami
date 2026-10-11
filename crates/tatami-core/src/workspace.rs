//! Which workspace is shown where. Two models:
//!
//! - `PerMonitor`: every monitor has its own namespace, so `1` on the left
//!   display and `1` on the right are different workspaces, both visible.
//! - `Shared` (AeroSpace): one global namespace; a workspace is assigned to
//!   one monitor at a time and switching to one visible elsewhere focuses
//!   that monitor instead of stealing it.
//!
//! The model only tracks ids, names and placement. The caller owns the
//! `Workspace` structs (trees, focus), creating them through the `create`
//! callbacks, and acts on the returned `Switch`.

use slotmap::SecondaryMap;
use tatami_protocol::{Dir, Rect, WorkspaceTarget};

use crate::ids::{MonitorId, NodeId, WindowId, WorkspaceId};
use crate::platform::MonitorInfo;

#[derive(Debug, Clone, PartialEq)]
pub struct Workspace {
    pub name: String,
    pub monitor: MonitorId,
    pub root: NodeId,
    pub floating: Vec<WindowId>,
    pub focused: Option<NodeId>,
    pub fullscreen: Option<WindowId>,
    pub prev_focused: Option<NodeId>,
}

impl Workspace {
    pub fn new(name: &str, monitor: MonitorId, root: NodeId) -> Self {
        Self {
            name: name.to_string(),
            monitor,
            root,
            floating: Vec::new(),
            focused: None,
            fullscreen: None,
            prev_focused: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelKind {
    PerMonitor,
    Shared,
}

/// What a workspace change requires of the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Switch {
    /// Park these workspaces' windows.
    pub hide: Vec<WorkspaceId>,
    /// Lay these out on their monitors.
    pub show: Vec<WorkspaceId>,
    pub focus_monitor: MonitorId,
    pub target: WorkspaceId,
    /// Nothing changes, not even the focused monitor.
    pub noop: bool,
    /// `target` no longer exists in the model: move its windows into this
    /// workspace, then drop it (per-monitor name clash on a move).
    pub merge_into: Option<WorkspaceId>,
}

/// A workspace displaced by a monitor removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rehome {
    pub ws: WorkspaceId,
    /// Per-monitor only: the fallback monitor already had this name; move
    /// `ws`'s windows here and drop `ws`.
    pub merge_into: Option<WorkspaceId>,
}

#[derive(Debug, Clone)]
struct Entry {
    name: String,
    monitor: MonitorId,
}

#[derive(Debug, Clone)]
pub struct WorkspaceModel {
    kind: ModelKind,
    /// Configured order, for next/prev and sorting.
    names: Vec<String>,
    entries: SecondaryMap<WorkspaceId, Entry>,
    monitors: Vec<MonitorId>,
    visible: SecondaryMap<MonitorId, WorkspaceId>,
    prev: SecondaryMap<MonitorId, WorkspaceId>,
    /// Shared model: workspace name -> monitor, already resolved from the
    /// config's patterns by the caller (`match_monitor_pattern`).
    forced: Vec<(String, MonitorId)>,
}

impl WorkspaceModel {
    pub fn new(kind: ModelKind, names: Vec<String>) -> Self {
        Self {
            kind,
            names,
            entries: SecondaryMap::new(),
            monitors: Vec::new(),
            visible: SecondaryMap::new(),
            prev: SecondaryMap::new(),
            forced: Vec::new(),
        }
    }

    pub fn kind(&self) -> ModelKind {
        self.kind
    }

    pub fn monitors(&self) -> &[MonitorId] {
        &self.monitors
    }

    /// Replace the force-assignment table (re-resolve after screen changes).
    pub fn set_forced(&mut self, forced: Vec<(String, MonitorId)>) {
        self.forced = forced;
    }

    /// The monitor `name` is pinned to, if that monitor is connected.
    fn forced_monitor(&self, name: &str) -> Option<MonitorId> {
        if self.kind != ModelKind::Shared {
            return None;
        }
        self.forced
            .iter()
            .find(|(n, m)| n == name && self.monitors.contains(m))
            .map(|(_, m)| *m)
    }

    fn order_key<'a>(&self, name: &'a str) -> (usize, &'a str) {
        (
            self.names
                .iter()
                .position(|n| n == name)
                .unwrap_or(usize::MAX),
            name,
        )
    }

    fn insert(
        &mut self,
        name: &str,
        m: MonitorId,
        create: &mut impl FnMut(&str, MonitorId) -> WorkspaceId,
    ) -> WorkspaceId {
        let id = create(name, m);
        self.entries.insert(
            id,
            Entry {
                name: name.to_string(),
                monitor: m,
            },
        );
        id
    }

    /// Drop a workspace from the model and every reference to it.
    fn forget(&mut self, ws: WorkspaceId) {
        self.entries.remove(ws);
        self.visible.retain(|_, w| *w != ws);
        self.prev.retain(|_, w| *w != ws);
    }

    // ---- queries -------------------------------------------------------

    pub fn visible_on(&self, m: MonitorId) -> Option<WorkspaceId> {
        self.visible.get(m).copied()
    }

    pub fn is_visible(&self, ws: WorkspaceId) -> bool {
        self.entries
            .get(ws)
            .is_some_and(|e| self.visible.get(e.monitor) == Some(&ws))
    }

    /// The monitor a workspace is shown on, or would be shown on.
    pub fn monitor_of(&self, ws: WorkspaceId) -> Option<MonitorId> {
        self.entries.get(ws).map(|e| e.monitor)
    }

    pub fn name_of(&self, ws: WorkspaceId) -> Option<&str> {
        self.entries.get(ws).map(|e| e.name.as_str())
    }

    /// Per-monitor: `name` on `m`. Shared: `name` anywhere (`m` ignored).
    pub fn find(&self, m: MonitorId, name: &str) -> Option<WorkspaceId> {
        let per = self.kind == ModelKind::PerMonitor;
        self.entries
            .iter()
            .find(|(_, e)| e.name == name && (!per || e.monitor == m))
            .map(|(id, _)| id)
    }

    /// Sorted by configured order, then lexically.
    pub fn workspaces_on(&self, m: MonitorId) -> Vec<WorkspaceId> {
        let mut v: Vec<WorkspaceId> = self
            .entries
            .iter()
            .filter(|(_, e)| e.monitor == m)
            .map(|(id, _)| id)
            .collect();
        v.sort_by(|a, b| {
            self.order_key(&self.entries[*a].name)
                .cmp(&self.order_key(&self.entries[*b].name))
        });
        v
    }

    /// Every workspace, grouped by monitor in connection order.
    pub fn all(&self) -> Vec<WorkspaceId> {
        let mut v: Vec<WorkspaceId> = self
            .monitors
            .iter()
            .flat_map(|&m| self.workspaces_on(m))
            .collect();
        // Workspaces whose monitor went away without `remove_monitor`.
        v.extend(
            self.entries
                .keys()
                .filter(|id| !self.monitors.contains(&self.entries[*id].monitor)),
        );
        v
    }

    pub fn back_and_forth(&self, m: MonitorId) -> Option<WorkspaceId> {
        let p = self.prev.get(m).copied()?;
        (self.entries.contains_key(p) && self.visible_on(m) != Some(p)).then_some(p)
    }

    // ---- monitors ------------------------------------------------------

    /// A workspace for a monitor that needs something to show: a hidden one
    /// already living there, else the first configured name free for it,
    /// else the lowest unused number.
    fn pick_replacement(
        &mut self,
        m: MonitorId,
        create: &mut impl FnMut(&str, MonitorId) -> WorkspaceId,
    ) -> WorkspaceId {
        if let Some(ws) = self
            .workspaces_on(m)
            .into_iter()
            .find(|&w| !self.is_visible(w))
        {
            return ws;
        }
        let free = |me: &Self, name: &str| match me.kind {
            ModelKind::PerMonitor => me.find(m, name).is_none(),
            ModelKind::Shared => {
                me.find(m, name).is_none() && me.forced_monitor(name).is_none_or(|f| f == m)
            }
        };
        let name = self
            .names
            .iter()
            .find(|n| free(self, n))
            .cloned()
            .unwrap_or_else(|| {
                (1..)
                    .map(|i| i.to_string())
                    .find(|n| free(self, n))
                    .expect("unbounded")
            });
        self.insert(&name, m, create)
    }

    /// Register a monitor and make sure it shows a workspace.
    pub fn add_monitor(
        &mut self,
        m: MonitorId,
        create: &mut impl FnMut(&str, MonitorId) -> WorkspaceId,
    ) -> WorkspaceId {
        if !self.monitors.contains(&m) {
            self.monitors.push(m);
        }
        if let Some(ws) = self.visible_on(m) {
            return ws;
        }
        let ws = self.pick_replacement(m, create);
        self.visible.insert(m, ws);
        ws
    }

    /// Unregister `m`; its workspaces move (hidden) to `fallback`. The
    /// caller parks their windows and performs any merges.
    pub fn remove_monitor(&mut self, m: MonitorId, fallback: MonitorId) -> Vec<Rehome> {
        self.monitors.retain(|&x| x != m);
        self.visible.remove(m);
        self.prev.remove(m);
        let mut out = Vec::new();
        for ws in self.workspaces_on(m) {
            let clash = match self.kind {
                ModelKind::PerMonitor => self.find(fallback, &self.entries[ws].name),
                ModelKind::Shared => None,
            };
            match clash {
                Some(_) => self.forget(ws),
                None => self.entries[ws].monitor = fallback,
            }
            out.push(Rehome {
                ws,
                merge_into: clash,
            });
        }
        out
    }

    // ---- switching -----------------------------------------------------

    /// Names `workspace next/prev` cycles through on `m`: per-monitor, the
    /// configured names plus any ad-hoc ones there; shared, the workspaces
    /// assigned to `m` plus configured names not yet created (and not
    /// pinned to another monitor).
    fn cycle_names(&self, m: MonitorId) -> Vec<String> {
        let mut v: Vec<String> = self
            .workspaces_on(m)
            .into_iter()
            .map(|w| self.entries[w].name.clone())
            .collect();
        for n in &self.names {
            let include = match self.kind {
                ModelKind::PerMonitor => true,
                ModelKind::Shared => {
                    self.find(m, n).is_none() && self.forced_monitor(n).is_none_or(|f| f == m)
                }
            };
            if include && !v.contains(n) {
                v.push(n.clone());
            }
        }
        v.sort_by(|a, b| self.order_key(a).cmp(&self.order_key(b)));
        v
    }

    /// Turn a target into a workspace id, creating a named one on demand.
    pub fn resolve(
        &mut self,
        focused: MonitorId,
        target: &WorkspaceTarget,
        wrap: bool,
        create: &mut impl FnMut(&str, MonitorId) -> WorkspaceId,
    ) -> Option<WorkspaceId> {
        match target {
            WorkspaceTarget::Name(n) => {
                if let Some(ws) = self.find(focused, n) {
                    return Some(ws);
                }
                let m = self.forced_monitor(n).unwrap_or(focused);
                Some(self.insert(n, m, create))
            }
            WorkspaceTarget::Next | WorkspaceTarget::Prev => {
                let cur = self.visible_on(focused)?;
                let names = self.cycle_names(focused);
                let i = names.iter().position(|n| *n == self.entries[cur].name)?;
                let n = names.len();
                let j = match (matches!(target, WorkspaceTarget::Next), wrap) {
                    (true, _) if i + 1 < n => i + 1,
                    (true, true) => 0,
                    (false, _) if i > 0 => i - 1,
                    (false, true) => n - 1,
                    _ => return None,
                };
                let name = names[j].clone();
                self.resolve(focused, &WorkspaceTarget::Name(name), false, create)
            }
        }
    }

    /// Make `target` visible on `m`; `focused` decides `noop`.
    fn show_on(&mut self, m: MonitorId, target: WorkspaceId, focused: MonitorId) -> Switch {
        let old = self.visible_on(m);
        let mut sw = Switch {
            hide: Vec::new(),
            show: Vec::new(),
            focus_monitor: m,
            target,
            noop: false,
            merge_into: None,
        };
        if old == Some(target) {
            sw.noop = m == focused;
            return sw;
        }
        self.entries[target].monitor = m;
        self.visible.insert(m, target);
        if let Some(o) = old {
            self.prev.insert(m, o);
            sw.hide.push(o);
        }
        sw.show.push(target);
        sw
    }

    pub fn switch(&mut self, focused: MonitorId, target: WorkspaceId) -> Switch {
        let Some(e) = self.entries.get(target) else {
            return Switch {
                hide: Vec::new(),
                show: Vec::new(),
                focus_monitor: focused,
                target,
                noop: true,
                merge_into: None,
            };
        };
        let home = match self.kind {
            ModelKind::PerMonitor => e.monitor,
            ModelKind::Shared => {
                if self.is_visible(target) {
                    // Visible elsewhere: focus it there rather than steal it.
                    e.monitor
                } else {
                    self.forced_monitor(&e.name).unwrap_or(focused)
                }
            }
        };
        self.show_on(home, target, focused)
    }

    /// Move `ws` to monitor `to` and show it there; focus follows. The
    /// monitor it leaves gets a replacement so it never shows nothing.
    pub fn move_workspace_to_monitor(
        &mut self,
        ws: WorkspaceId,
        to: MonitorId,
        create: &mut impl FnMut(&str, MonitorId) -> WorkspaceId,
    ) -> Switch {
        let Some(e) = self.entries.get(ws) else {
            return self.switch(to, ws);
        };
        let from = e.monitor;
        if from == to {
            return self.show_on(to, ws, to);
        }
        let was_visible = self.is_visible(ws);
        let clash = match self.kind {
            ModelKind::PerMonitor => self.find(to, &e.name),
            ModelKind::Shared => None,
        };
        let mut sw = match clash {
            Some(into) => {
                self.forget(ws);
                let mut sw = self.show_on(to, into, to);
                sw.target = ws;
                sw.merge_into = Some(into);
                sw.noop = false;
                sw
            }
            None => {
                if was_visible {
                    self.visible.remove(from);
                }
                self.show_on(to, ws, to)
            }
        };
        if was_visible && self.visible_on(from).is_none() && self.monitors.contains(&from) {
            let r = self.pick_replacement(from, create);
            self.visible.insert(from, r);
            sw.show.push(r);
        }
        sw
    }

    /// Remove workspaces that are empty, hidden and not persistent. Returns
    /// them so the caller can free their trees.
    pub fn gc(
        &mut self,
        is_empty: impl Fn(WorkspaceId) -> bool,
        persistent: &[String],
    ) -> Vec<WorkspaceId> {
        let dead: Vec<WorkspaceId> = self
            .entries
            .iter()
            .filter(|(id, e)| {
                !self.is_visible(*id) && !persistent.contains(&e.name) && is_empty(*id)
            })
            .map(|(id, _)| id)
            .collect();
        for &d in &dead {
            self.forget(d);
        }
        dead
    }
}

// ---- monitor helpers ---------------------------------------------------

/// The monitor in `dir` from `from`: only monitors whose center lies in
/// that half-plane qualify; the nearest center wins.
pub fn monitor_in_dir(from: &Rect, others: &[(MonitorId, Rect)], dir: Dir) -> Option<MonitorId> {
    let c = from.center();
    others
        .iter()
        .filter_map(|(id, r)| {
            let o = r.center();
            let ahead = match dir {
                Dir::Left => o.x < c.x,
                Dir::Right => o.x > c.x,
                Dir::Up => o.y < c.y,
                Dir::Down => o.y > c.y,
            };
            ahead.then(|| (*id, (o.x - c.x).hypot(o.y - c.y)))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(id, _)| id)
}

pub fn monitor_next_prev(
    order: &[MonitorId],
    cur: MonitorId,
    forward: bool,
    wrap: bool,
) -> Option<MonitorId> {
    let i = order.iter().position(|&m| m == cur)?;
    let n = order.len();
    let j = match (forward, wrap) {
        (true, _) if i + 1 < n => i + 1,
        (true, true) => 0,
        (false, _) if i > 0 => i - 1,
        (false, true) => n - 1,
        _ => return None,
    };
    (order[j] != cur).then_some(order[j])
}

/// `main`; `secondary` (only defined with exactly two monitors, as in
/// AeroSpace); a 1-based index into `ordered_left_to_right`; otherwise a
/// case-insensitive substring of the name, with optional `^`/`$` anchors.
pub fn match_monitor_pattern(
    pattern: &str,
    monitors: &[(MonitorId, &MonitorInfo)],
    ordered_left_to_right: &[MonitorId],
) -> Option<MonitorId> {
    let p = pattern.trim();
    match p {
        "main" => monitors.iter().find(|(_, i)| i.is_main).map(|(id, _)| *id),
        "secondary" => {
            if monitors.len() == 2 {
                monitors.iter().find(|(_, i)| !i.is_main).map(|(id, _)| *id)
            } else {
                None
            }
        }
        _ => {
            if let Ok(n) = p.parse::<usize>() {
                return n
                    .checked_sub(1)
                    .and_then(|i| ordered_left_to_right.get(i))
                    .copied();
            }
            let lower = p.to_lowercase();
            let (start, rest) = match lower.strip_prefix('^') {
                Some(r) => (true, r),
                None => (false, lower.as_str()),
            };
            let (end, core) = match rest.strip_suffix('$') {
                Some(c) => (true, c),
                None => (false, rest),
            };
            monitors
                .iter()
                .find(|(_, i)| {
                    let name = i.name.to_lowercase();
                    match (start, end) {
                        (true, true) => name == core,
                        (true, false) => name.starts_with(core),
                        (false, true) => name.ends_with(core),
                        (false, false) => name.contains(core),
                    }
                })
                .map(|(id, _)| *id)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slotmap::SlotMap;

    /// Owns the caller-side state the model refers to.
    struct World {
        ws: SlotMap<WorkspaceId, (String, MonitorId)>,
        mons: Vec<MonitorId>,
    }

    impl World {
        fn new(n: usize) -> Self {
            let mut m: SlotMap<MonitorId, ()> = SlotMap::with_key();
            Self {
                ws: SlotMap::with_key(),
                mons: (0..n).map(|_| m.insert(())).collect(),
            }
        }
        fn create(&mut self) -> impl FnMut(&str, MonitorId) -> WorkspaceId + '_ {
            |name, m| self.ws.insert((name.to_string(), m))
        }
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn model(kind: ModelKind, w: &mut World) -> WorkspaceModel {
        let mut m = WorkspaceModel::new(kind, names(&["1", "2", "3"]));
        for &mon in &w.mons.clone() {
            m.add_monitor(mon, &mut w.create());
        }
        m
    }

    fn name(m: &WorkspaceModel, ws: Option<WorkspaceId>) -> &str {
        m.name_of(ws.unwrap()).unwrap()
    }

    fn by_name(m: &mut WorkspaceModel, mon: MonitorId, w: &mut World, n: &str) -> WorkspaceId {
        m.resolve(
            mon,
            &WorkspaceTarget::Name(n.into()),
            false,
            &mut w.create(),
        )
        .unwrap()
    }

    #[test]
    fn per_monitor_same_name_visible_on_two_monitors() {
        let mut w = World::new(2);
        let m = model(ModelKind::PerMonitor, &mut w);
        let (a, b) = (
            m.visible_on(w.mons[0]).unwrap(),
            m.visible_on(w.mons[1]).unwrap(),
        );
        assert_ne!(a, b);
        assert_eq!(m.name_of(a), Some("1"));
        assert_eq!(m.name_of(b), Some("1"));
        assert!(m.is_visible(a) && m.is_visible(b));
    }

    #[test]
    fn per_monitor_resolve_creates_on_focused_monitor_and_switch_swaps() {
        let mut w = World::new(2);
        let mut m = model(ModelKind::PerMonitor, &mut w);
        let one = m.visible_on(w.mons[1]).unwrap();
        let two = by_name(&mut m, w.mons[1], &mut w, "2");
        assert_eq!(m.monitor_of(two), Some(w.mons[1]));
        assert_eq!(
            by_name(&mut m, w.mons[1], &mut w, "2"),
            two,
            "found, not recreated"
        );
        let sw = m.switch(w.mons[1], two);
        assert_eq!(sw.hide, vec![one]);
        assert_eq!(sw.show, vec![two]);
        assert_eq!(sw.focus_monitor, w.mons[1]);
        assert!(!sw.noop);
        assert!(m.switch(w.mons[1], two).noop);
        assert_eq!(
            name(&m, m.visible_on(w.mons[0])),
            "1",
            "other monitor untouched"
        );
    }

    #[test]
    fn per_monitor_switch_to_other_monitors_workspace_focuses_it() {
        let mut w = World::new(2);
        let mut m = model(ModelKind::PerMonitor, &mut w);
        let left_one = m.visible_on(w.mons[0]).unwrap();
        let sw = m.switch(w.mons[1], left_one);
        assert_eq!(sw.focus_monitor, w.mons[0]);
        assert!(sw.hide.is_empty() && sw.show.is_empty() && !sw.noop);
    }

    #[test]
    fn per_monitor_next_prev_walks_configured_names() {
        let mut w = World::new(1);
        let mut m = model(ModelKind::PerMonitor, &mut w);
        let mon = w.mons[0];
        let next = m.resolve(mon, &WorkspaceTarget::Next, false, &mut w.create());
        assert_eq!(
            name(&m, next),
            "2",
            "configured but not yet created still counts"
        );
        m.switch(mon, next.unwrap());
        let three = by_name(&mut m, mon, &mut w, "3");
        m.switch(mon, three);
        assert_eq!(
            m.resolve(mon, &WorkspaceTarget::Next, false, &mut w.create()),
            None
        );
        let wrapped = m.resolve(mon, &WorkspaceTarget::Next, true, &mut w.create());
        assert_eq!(name(&m, wrapped), "1");
        let prev = m.resolve(mon, &WorkspaceTarget::Prev, false, &mut w.create());
        assert_eq!(name(&m, prev), "2");
    }

    #[test]
    fn per_monitor_ad_hoc_names_sort_after_configured() {
        let mut w = World::new(1);
        let mut m = model(ModelKind::PerMonitor, &mut w);
        let mon = w.mons[0];
        let mail = by_name(&mut m, mon, &mut w, "mail");
        let b = by_name(&mut m, mon, &mut w, "b");
        let two = by_name(&mut m, mon, &mut w, "2");
        let one = m.visible_on(mon).unwrap();
        assert_eq!(m.workspaces_on(mon), vec![one, two, b, mail]);
    }

    #[test]
    fn per_monitor_move_to_monitor_with_same_name_merges() {
        let mut w = World::new(2);
        let mut m = model(ModelKind::PerMonitor, &mut w);
        let (l, r) = (w.mons[0], w.mons[1]);
        let left1 = m.visible_on(l).unwrap();
        let right1 = m.visible_on(r).unwrap();
        let sw = m.move_workspace_to_monitor(left1, r, &mut w.create());
        assert_eq!(sw.merge_into, Some(right1));
        assert_eq!(sw.target, left1);
        assert_eq!(sw.focus_monitor, r);
        assert!(m.name_of(left1).is_none(), "merged away");
        let replacement = m.visible_on(l).unwrap();
        assert_eq!(m.name_of(replacement), Some("1"), "left gets a fresh 1");
        assert!(sw.show.contains(&replacement));
    }

    #[test]
    fn per_monitor_move_to_monitor_without_clash_relocates() {
        let mut w = World::new(2);
        let mut m = model(ModelKind::PerMonitor, &mut w);
        let (l, r) = (w.mons[0], w.mons[1]);
        let two = by_name(&mut m, l, &mut w, "2");
        m.switch(l, two);
        let sw = m.move_workspace_to_monitor(two, r, &mut w.create());
        assert_eq!(sw.merge_into, None);
        assert_eq!(m.monitor_of(two), Some(r));
        assert_eq!(m.visible_on(r), Some(two));
        assert_eq!(sw.hide.len(), 1, "right's old workspace hidden");
        assert_eq!(
            name(&m, m.visible_on(l)),
            "1",
            "left falls back to its hidden 1"
        );
    }

    #[test]
    fn per_monitor_remove_monitor_merges_clashes() {
        let mut w = World::new(2);
        let mut m = model(ModelKind::PerMonitor, &mut w);
        let (l, r) = (w.mons[0], w.mons[1]);
        let right1 = m.visible_on(r).unwrap();
        let right9 = by_name(&mut m, r, &mut w, "9");
        let left1 = m.visible_on(l).unwrap();
        let moved = m.remove_monitor(r, l);
        assert!(moved.contains(&Rehome {
            ws: right1,
            merge_into: Some(left1)
        }));
        assert!(moved.contains(&Rehome {
            ws: right9,
            merge_into: None
        }));
        assert_eq!(m.monitor_of(right9), Some(l));
        assert!(!m.is_visible(right9));
        assert_eq!(m.monitors(), &[l]);
    }

    #[test]
    fn per_monitor_back_and_forth_and_gc() {
        let mut w = World::new(1);
        let mut m = model(ModelKind::PerMonitor, &mut w);
        let mon = w.mons[0];
        let one = m.visible_on(mon).unwrap();
        assert_eq!(m.back_and_forth(mon), None);
        let tmp = by_name(&mut m, mon, &mut w, "tmp");
        m.switch(mon, tmp);
        assert_eq!(m.back_and_forth(mon), Some(one));
        m.switch(mon, one);
        assert_eq!(m.back_and_forth(mon), Some(tmp));
        let dead = m.gc(|_| true, &names(&["1"]));
        assert_eq!(dead, vec![tmp], "visible 1 survives; hidden empty tmp goes");
        assert_eq!(m.back_and_forth(mon), None, "prev cleared with it");
    }

    #[test]
    fn gc_keeps_persistent_and_non_empty() {
        let mut w = World::new(1);
        let mut m = model(ModelKind::Shared, &mut w);
        let mon = w.mons[0];
        let two = by_name(&mut m, mon, &mut w, "2");
        let busy = by_name(&mut m, mon, &mut w, "busy");
        let gone = by_name(&mut m, mon, &mut w, "gone");
        let dead = m.gc(|ws| ws != busy, &names(&["2"]));
        assert_eq!(dead, vec![gone]);
        assert!(m.name_of(two).is_some() && m.name_of(busy).is_some());
    }

    #[test]
    fn shared_switch_to_visible_elsewhere_focuses_that_monitor() {
        let mut w = World::new(2);
        let mut m = model(ModelKind::Shared, &mut w);
        let (l, r) = (w.mons[0], w.mons[1]);
        let left = m.visible_on(l).unwrap();
        let right = m.visible_on(r).unwrap();
        assert_eq!(m.name_of(left), Some("1"));
        assert_eq!(
            m.name_of(right),
            Some("2"),
            "names are global: second monitor takes the next free one"
        );
        let sw = m.switch(l, right);
        assert_eq!(sw.focus_monitor, r);
        assert!(sw.hide.is_empty() && sw.show.is_empty() && !sw.noop);
        assert_eq!(m.visible_on(l), Some(left), "not stolen");
        assert!(m.switch(r, right).noop);
    }

    #[test]
    fn shared_switch_to_hidden_reassigns_to_focused_monitor() {
        let mut w = World::new(2);
        let mut m = model(ModelKind::Shared, &mut w);
        let (l, r) = (w.mons[0], w.mons[1]);
        let three = by_name(&mut m, l, &mut w, "3");
        assert_eq!(m.monitor_of(three), Some(l));
        let old_r = m.visible_on(r).unwrap();
        let sw = m.switch(r, three);
        assert_eq!(sw.hide, vec![old_r]);
        assert_eq!(sw.show, vec![three]);
        assert_eq!(sw.focus_monitor, r);
        assert_eq!(m.monitor_of(three), Some(r));
        assert_eq!(m.back_and_forth(r), Some(old_r));
    }

    #[test]
    fn shared_force_assignment_creates_and_shows_on_pinned_monitor() {
        let mut w = World::new(2);
        let mut m = WorkspaceModel::new(ModelKind::Shared, names(&["1", "2", "mail"]));
        let (l, r) = (w.mons[0], w.mons[1]);
        m.set_forced(vec![("mail".into(), r)]);
        m.add_monitor(l, &mut w.create());
        m.add_monitor(r, &mut w.create());
        let mail = by_name(&mut m, l, &mut w, "mail");
        assert_eq!(m.monitor_of(mail), Some(r));
        let sw = m.switch(l, mail);
        assert_eq!(sw.focus_monitor, r);
        assert_eq!(m.visible_on(r), Some(mail));
        assert_eq!(name(&m, m.visible_on(l)), "1");
    }

    #[test]
    fn shared_next_prev_walks_the_focused_monitors_workspaces() {
        let mut w = World::new(2);
        let mut m = model(ModelKind::Shared, &mut w);
        let (l, r) = (w.mons[0], w.mons[1]);
        // 1 on left, 2 on right; 3 is configured and not yet created.
        let prev = m.resolve(r, &WorkspaceTarget::Prev, true, &mut w.create());
        assert_eq!(
            name(&m, prev),
            "3",
            "wraps over the right monitor's list [2, 3]"
        );
        assert_eq!(m.monitor_of(prev.unwrap()), Some(r));
        assert_eq!(
            m.resolve(l, &WorkspaceTarget::Next, false, &mut w.create()),
            None,
            "2 and 3 live on the other monitor and are skipped"
        );
    }

    #[test]
    fn shared_move_workspace_to_monitor_leaves_a_replacement() {
        let mut w = World::new(2);
        let mut m = model(ModelKind::Shared, &mut w);
        let (l, r) = (w.mons[0], w.mons[1]);
        let one = m.visible_on(l).unwrap();
        let two = m.visible_on(r).unwrap();
        let sw = m.move_workspace_to_monitor(one, r, &mut w.create());
        assert_eq!(sw.hide, vec![two]);
        assert_eq!(sw.focus_monitor, r);
        assert_eq!(m.visible_on(r), Some(one));
        let repl = m.visible_on(l).unwrap();
        assert_eq!(m.name_of(repl), Some("3"));
        assert!(sw.show.contains(&one) && sw.show.contains(&repl));
    }

    #[test]
    fn shared_remove_monitor_rehomes_hidden_and_add_reuses_them() {
        let mut w = World::new(2);
        let mut m = model(ModelKind::Shared, &mut w);
        let (l, r) = (w.mons[0], w.mons[1]);
        let two = m.visible_on(r).unwrap();
        let moved = m.remove_monitor(r, l);
        assert_eq!(
            moved,
            vec![Rehome {
                ws: two,
                merge_into: None
            }]
        );
        assert_eq!(m.monitor_of(two), Some(l));
        assert!(!m.is_visible(two));
        let one = m.visible_on(l).unwrap();
        assert_eq!(m.switch(l, two).hide, vec![one]);
        // Reconnect: the right monitor picks a workspace that is not visible.
        let shown = m.add_monitor(r, &mut w.create());
        assert_ne!(Some(shown), m.visible_on(l));
    }

    #[test]
    fn add_monitor_is_idempotent() {
        let mut w = World::new(1);
        let mut m = model(ModelKind::Shared, &mut w);
        let before = m.visible_on(w.mons[0]);
        assert_eq!(Some(m.add_monitor(w.mons[0], &mut w.create())), before);
        assert_eq!(m.all().len(), 1);
    }

    fn rect_mons() -> (Vec<MonitorId>, Vec<(MonitorId, Rect)>) {
        let w = World::new(4);
        let r = vec![
            (w.mons[0], Rect::new(-1688.0, -1080.0, 1920.0, 1080.0)),
            (w.mons[1], Rect::new(232.0, -1440.0, 2560.0, 1440.0)),
            (w.mons[2], Rect::new(2792.0, -1080.0, 1920.0, 1080.0)),
            (w.mons[3], Rect::new(0.0, 0.0, 3024.0, 1964.0)),
        ];
        (w.mons, r)
    }

    #[test]
    fn monitor_in_dir_uses_half_plane_and_nearest_center() {
        let (ids, rects) = rect_mons();
        let laptop = rects[3].1;
        assert_eq!(monitor_in_dir(&laptop, &rects[..3], Dir::Up), Some(ids[1]));
        assert_eq!(monitor_in_dir(&rects[1].1, &rects, Dir::Left), Some(ids[0]));
        assert_eq!(
            monitor_in_dir(&rects[1].1, &rects, Dir::Right),
            Some(ids[2])
        );
        assert_eq!(monitor_in_dir(&rects[1].1, &rects, Dir::Down), Some(ids[3]));
        assert_eq!(monitor_in_dir(&laptop, &rects[..3], Dir::Down), None);
    }

    #[test]
    fn monitor_next_prev_wraps_on_request() {
        let (ids, _) = rect_mons();
        assert_eq!(monitor_next_prev(&ids, ids[0], true, false), Some(ids[1]));
        assert_eq!(monitor_next_prev(&ids, ids[3], true, false), None);
        assert_eq!(monitor_next_prev(&ids, ids[3], true, true), Some(ids[0]));
        assert_eq!(monitor_next_prev(&ids, ids[0], false, true), Some(ids[3]));
        assert_eq!(monitor_next_prev(&ids[..1], ids[0], true, true), None);
    }

    #[test]
    fn monitor_patterns() {
        let (ids, rects) = rect_mons();
        let info = |name: &str, main: bool, r: Rect| MonitorInfo {
            uuid: name.into(),
            name: name.into(),
            frame: r,
            visible: r,
            is_main: main,
        };
        let infos = [
            info("DELL U2720Q", false, rects[0].1),
            info("LG HDR 4K", false, rects[1].1),
            info("DELL P2419H", false, rects[2].1),
            info("Built-in Retina Display", true, rects[3].1),
        ];
        let mons: Vec<(MonitorId, &MonitorInfo)> = ids.iter().copied().zip(infos.iter()).collect();
        let ltr = [ids[0], ids[3], ids[1], ids[2]];
        assert_eq!(match_monitor_pattern("main", &mons, &ltr), Some(ids[3]));
        assert_eq!(
            match_monitor_pattern("secondary", &mons, &ltr),
            None,
            "ambiguous with four"
        );
        assert_eq!(
            match_monitor_pattern("secondary", &mons[2..], &ltr),
            Some(ids[2])
        );
        assert_eq!(match_monitor_pattern("2", &mons, &ltr), Some(ids[3]));
        assert_eq!(match_monitor_pattern("5", &mons, &ltr), None);
        assert_eq!(match_monitor_pattern("0", &mons, &ltr), None);
        assert_eq!(match_monitor_pattern("lg", &mons, &ltr), Some(ids[1]));
        assert_eq!(match_monitor_pattern("^dell", &mons, &ltr), Some(ids[0]));
        assert_eq!(match_monitor_pattern("p2419h$", &mons, &ltr), Some(ids[2]));
        assert_eq!(
            match_monitor_pattern("^built-in retina display$", &mons, &ltr),
            Some(ids[3])
        );
        assert_eq!(match_monitor_pattern("samsung", &mons, &ltr), None);
    }
}
