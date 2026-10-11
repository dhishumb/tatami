//! Restoring a saved layout and taking the snapshot it comes from.
//!
//! A restore rebuilds every saved workspace tree with `Pending` leaves, one
//! per saved window ("spec"), so the layout has its final shape before the
//! windows exist. Live windows are matched to specs by `matcher::score`
//! (global best-first; ties go to the earlier spec in tree order), missing
//! apps are launched, and each `WindowCreated` during the session is tried
//! against the remaining specs. An app's specs expire `restore.timeout_s`
//! after the last sign of it; their placeholders are pruned.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use tatami_protocol::{IpcEvent, Rect};
use tatami_state::{
    MatchInput, STATE_VERSION, SavedFloating, SavedMonitor, SavedNode, SavedState, SavedWindow,
    SavedWorkspace, score,
};

use super::{Outcome, Reactor, fit_into};
use crate::ids::{MonitorId, NodeId, WindowId, WorkspaceId};
use crate::platform::Platform;
use crate::tree::{Leaf, NodeKind};
use crate::window::{Window, WindowKind};

#[derive(Debug, Clone)]
pub(super) struct Spec {
    saved: SavedWindow,
    ws: WorkspaceId,
    /// The placeholder leaf of a tiled spec; `None` for a floating one.
    leaf: Option<NodeId>,
    float: Option<Rect>,
    /// The saved workspace's focused window.
    focus: bool,
    /// Bundle id, or app name for apps without one.
    key: String,
}

#[derive(Debug, Default)]
pub(super) struct RestoreSession {
    /// Ordered by id, which is tree order: ties go to the first.
    specs: BTreeMap<u32, Spec>,
    deadlines: HashMap<String, Instant>,
    launched: HashSet<String>,
    pub(super) total: usize,
}

impl RestoreSession {
    pub(super) fn pending(&self) -> usize {
        self.specs.len()
    }

    pub(super) fn next_deadline(&self) -> Option<Instant> {
        self.deadlines.values().min().copied()
    }

    /// `from` was merged into `into`.
    pub(super) fn retarget(&mut self, from: WorkspaceId, into: WorkspaceId) {
        for s in self.specs.values_mut() {
            if s.ws == from {
                s.ws = into;
            }
        }
    }

    /// Best-scoring spec for a live window; ties go to the earliest spec.
    fn best(&self, input: &MatchInput<'_>) -> Option<u32> {
        let mut best: Option<(u32, u32)> = None;
        for (&id, s) in &self.specs {
            if let Some(sc) = score(&s.saved, input)
                && best.is_none_or(|(b, _)| sc > b)
            {
                best = Some((sc, id));
            }
        }
        best.map(|b| b.1)
    }
}

fn spec_key(w: &SavedWindow) -> String {
    w.bundle_id.clone().unwrap_or_else(|| w.app.clone())
}

fn match_input(w: &Window) -> MatchInput<'_> {
    MatchInput {
        bundle_id: w.bundle_id.as_deref(),
        app: &w.app_name,
        title: &w.title,
        subrole: &w.subrole,
        pid: w.pid,
        cg_id: w.handle.0,
        size: (w.frame.w, w.frame.h),
    }
}

impl<P: Platform> Reactor<P> {
    /// Run a restore session against the live state (`checkpoint restore`):
    /// rebuild the saved trees with placeholders, re-slot every managed
    /// window that matches a spec, launch missing apps, and put windows that
    /// match nothing on their monitor's visible workspace.
    pub fn restore(&mut self, saved: SavedState, now: Instant) -> Vec<Outcome> {
        self.now = now;
        self.restore_inner(saved);
        self.finish()
    }

    /// The live layout as a `SavedState`; `saved_at` is left for the daemon.
    pub fn snapshot(&self) -> SavedState {
        let monitors = self
            .order
            .iter()
            .map(|&m| {
                let info = &self.monitors[m].info;
                SavedMonitor {
                    uuid: info.uuid.clone(),
                    name: info.name.clone(),
                    frame: info.frame,
                    active: self
                        .visible_ws(m)
                        .map(|w| self.ws_name(w).to_string())
                        .unwrap_or_default(),
                    workspaces: self
                        .model
                        .workspaces_on(m)
                        .into_iter()
                        .map(|ws| self.save_ws(ws))
                        .collect(),
                }
            })
            .collect();
        SavedState {
            version: STATE_VERSION,
            saved_at: String::new(),
            model: self.model_name().to_string(),
            focused_monitor: self
                .monitors
                .get(self.focused_monitor)
                .map(|m| m.info.uuid.clone()),
            monitors,
        }
    }

    fn save_ws(&self, ws: WorkspaceId) -> SavedWorkspace {
        let w = &self.workspaces[ws];
        let focused = w
            .focused
            .and_then(|f| self.tree.leaves(w.root).iter().position(|&l| l == f));
        let mut floating: Vec<SavedFloating> = w
            .floating
            .iter()
            .map(|&id| {
                let m = &self.wins[id];
                SavedFloating {
                    window: self.saved_window(id),
                    frame: m.float_frame.unwrap_or(m.w.frame),
                }
            })
            .collect();
        if let Some(s) = &self.restore {
            floating.extend(s.specs.values().filter(|sp| sp.ws == ws).filter_map(|sp| {
                sp.float.map(|frame| SavedFloating {
                    window: sp.saved.clone(),
                    frame,
                })
            }));
        }
        SavedWorkspace {
            name: self.ws_name(ws).to_string(),
            root: self.save_node(w.root),
            floating,
            focused,
        }
    }

    fn save_node(&self, n: NodeId) -> SavedNode {
        let node = self.tree.node(n);
        let weight = node.weight;
        match &node.kind {
            NodeKind::Container {
                layout, children, ..
            } => SavedNode::Container {
                layout: *layout,
                weight,
                children: children.iter().map(|&c| self.save_node(c)).collect(),
            },
            NodeKind::Leaf(Leaf::Window(id)) => SavedNode::Window {
                weight,
                window: self.saved_window(*id),
            },
            NodeKind::Leaf(Leaf::Pending(s)) => SavedNode::Window {
                weight,
                window: self
                    .restore
                    .as_ref()
                    .and_then(|r| r.specs.get(s))
                    .map(|sp| sp.saved.clone())
                    .unwrap_or(SavedWindow {
                        bundle_id: None,
                        app: String::new(),
                        title: String::new(),
                        subrole: String::new(),
                        pid: None,
                        cg_id: None,
                        size: None,
                    }),
            },
        }
    }

    fn saved_window(&self, id: WindowId) -> SavedWindow {
        let w = &self.wins[id].w;
        let size = self.wins[id].float_frame.unwrap_or(w.frame);
        SavedWindow {
            bundle_id: w.bundle_id.clone(),
            app: w.app_name.clone(),
            title: w.title.clone(),
            subrole: w.subrole.clone(),
            pid: Some(w.pid),
            cg_id: Some(w.handle.0),
            size: Some((size.w, size.h)),
        }
    }

    /// uuid, else name, else frame size, else the focused/main monitor.
    fn map_monitor(&self, sm: &SavedMonitor) -> MonitorId {
        let find = |f: &dyn Fn(&crate::platform::MonitorInfo) -> bool| {
            self.order
                .iter()
                .copied()
                .find(|&m| f(&self.monitors[m].info))
        };
        find(&|i| i.uuid == sm.uuid)
            .or_else(|| find(&|i| !sm.name.is_empty() && i.name == sm.name))
            .or_else(|| {
                find(&|i| {
                    (i.frame.w - sm.frame.w).abs() < 1.0 && (i.frame.h - sm.frame.h).abs() < 1.0
                })
            })
            .or_else(|| find(&|i| i.is_main))
            .unwrap_or(self.focused_monitor)
    }

    fn drop_session(&mut self) {
        let Some(old) = self.restore.take() else {
            return;
        };
        for s in old.specs.values() {
            if let Some(l) = s.leaf
                && self.tree.contains(l)
            {
                let root = self.tree.root_of(l);
                self.tree.remove_leaf(l);
                self.tree.normalize(root);
            }
        }
    }

    /// Placeholders for a saved subtree under `parent`, in DFS order.
    fn build(
        &mut self,
        parent: NodeId,
        node: &SavedNode,
        ws: WorkspaceId,
        focus: Option<usize>,
        idx: &mut usize,
        session: &mut RestoreSession,
    ) {
        match node {
            SavedNode::Container {
                layout,
                weight,
                children,
            } => {
                let c = self.tree.push_container(parent, *layout, *weight);
                for ch in children {
                    self.build(c, ch, ws, focus, idx, session);
                }
            }
            SavedNode::Window { weight, window } => {
                let id = self.next_spec;
                self.next_spec = self.next_spec.wrapping_add(1);
                let leaf = self.tree.push_leaf(parent, Leaf::Pending(id), *weight);
                session.specs.insert(
                    id,
                    Spec {
                        saved: window.clone(),
                        ws,
                        leaf: Some(leaf),
                        float: None,
                        focus: focus == Some(*idx),
                        key: spec_key(window),
                    },
                );
                *idx += 1;
            }
        }
    }

    pub(super) fn restore_inner(&mut self, saved: SavedState) {
        if saved.model != self.model_name() {
            tracing::warn!(
                saved = %saved.model,
                running = self.model_name(),
                "saved layout uses another workspace model; not restoring"
            );
            return;
        }
        self.drop_session();

        // Every tiled window leaves its tree; matched ones get a slot, the
        // rest go back to their monitor's visible workspace.
        let mut loose: Vec<(WindowId, MonitorId)> = Vec::new();
        for ws in self.model.all() {
            let mon = self.ws_monitor(ws);
            for id in self.ws_windows(ws) {
                if self.wins[id].leaf.is_some() {
                    loose.push((id, mon));
                }
            }
        }
        for &(id, _) in &loose {
            self.detach(id);
        }

        let mut session = RestoreSession::default();
        for sm in &saved.monitors {
            let m = self.map_monitor(sm);
            for sw in &sm.workspaces {
                let Some(ws) = self.create_ws(m, &sw.name) else {
                    continue;
                };
                let root = self.workspaces[ws].root;
                let mut idx = 0;
                match &sw.root {
                    SavedNode::Container {
                        layout, children, ..
                    } if self.tree.children(root).is_empty() => {
                        self.tree.set_layout(root, *layout);
                        for c in children {
                            self.build(root, c, ws, sw.focused, &mut idx, &mut session);
                        }
                    }
                    other => self.build(root, other, ws, sw.focused, &mut idx, &mut session),
                }
                self.tree.normalize(root);
                for f in &sw.floating {
                    let id = self.next_spec;
                    self.next_spec = self.next_spec.wrapping_add(1);
                    session.specs.insert(
                        id,
                        Spec {
                            saved: f.window.clone(),
                            ws,
                            leaf: None,
                            float: Some(f.frame),
                            focus: false,
                            key: spec_key(&f.window),
                        },
                    );
                }
            }
        }
        session.total = session.specs.len();

        // Global best-first matching of the windows we already have.
        let mut cands: Vec<WindowId> = loose.iter().map(|l| l.0).collect();
        cands.extend(
            self.wins
                .iter()
                .filter(|(_, m)| m.w.kind == WindowKind::Floating && !m.w.minimized)
                .map(|(id, _)| id),
        );
        let mut pairs: Vec<(u32, u32, usize)> = Vec::new();
        for (ci, &id) in cands.iter().enumerate() {
            let input = match_input(&self.wins[id].w);
            for (&sid, s) in &session.specs {
                if let Some(sc) = score(&s.saved, &input) {
                    pairs.push((sc, sid, ci));
                }
            }
        }
        pairs.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        self.restore = Some(session);
        let mut used = vec![false; cands.len()];
        for (_, sid, ci) in pairs {
            if used[ci]
                || !self
                    .restore
                    .as_ref()
                    .is_some_and(|s| s.specs.contains_key(&sid))
            {
                continue;
            }
            used[ci] = true;
            self.fill_spec(cands[ci], sid);
        }
        for (ci, &(id, mon)) in loose.iter().enumerate() {
            if used[ci] {
                continue;
            }
            if let Some(ws) = self
                .visible_ws(mon)
                .or_else(|| self.visible_ws(self.focused_monitor))
            {
                self.attach(id, ws, false);
            }
        }

        for sm in &saved.monitors {
            let m = self.map_monitor(sm);
            if let Some(ws) = self.model.find(m, &sm.active) {
                // Visibility only; `relayout_all` below parks and shows.
                let _ = self.model.switch(m, ws);
            }
        }
        if let Some(u) = &saved.focused_monitor
            && let Some(&m) = self
                .order
                .iter()
                .find(|&&m| &self.monitors[m].info.uuid == u)
        {
            self.focused_monitor = m;
        }

        self.launch_missing();
        if let Some(s) = self.restore.as_mut() {
            let deadline = self.now + Duration::from_secs(self.cfg.restore.timeout_s);
            for sp in s.specs.values() {
                s.deadlines.insert(sp.key.clone(), deadline);
            }
        }

        self.relayout_all();
        if let Some(ws) = self.visible_ws(self.focused_monitor) {
            self.focus_ws(ws);
        }
        self.gc();
        self.emit_restore();
        self.dirty = true;
    }

    /// Launch apps with unmatched specs: once per bundle id, only when not
    /// running, never for `restore.ignore`.
    fn launch_missing(&mut self) {
        if !self.cfg.restore.launch_apps {
            return;
        }
        let Some(s) = self.restore.as_mut() else {
            return;
        };
        let mut want: Vec<String> = Vec::new();
        for sp in s.specs.values() {
            if let Some(b) = &sp.saved.bundle_id
                && !want.contains(b)
                && !s.launched.contains(b)
                && !self.cfg.restore.ignore.contains(b)
                && !self
                    .apps
                    .values()
                    .any(|a| a.info.bundle_id.as_deref() == Some(b.as_str()))
            {
                want.push(b.clone());
            }
        }
        for b in want {
            if let Err(e) = self.p.launch(&b) {
                tracing::warn!(bundle = %b, "restore: launch failed: {e}");
            }
            s.launched.insert(b);
        }
    }

    /// Put a window into a spec's slot (tiled placeholder or floating frame).
    /// Does not lay out.
    fn fill_spec(&mut self, id: WindowId, sid: u32) {
        let Some(spec) = self.restore.as_mut().and_then(|s| s.specs.remove(&sid)) else {
            return;
        };
        self.detach(id);
        let ws = spec.ws;
        if !self.workspaces.contains_key(ws) {
            return;
        }
        self.wins[id].ws = ws;
        match spec.leaf {
            Some(leaf) if self.tree.pending_of(leaf) == Some(sid) => {
                self.tree.replace_pending(leaf, id);
                let m = &mut self.wins[id];
                m.w.kind = WindowKind::Tiled;
                m.leaf = Some(leaf);
                let w = &mut self.workspaces[ws];
                if spec.focus || w.focused.is_none() {
                    w.focused = Some(leaf);
                    if spec.focus {
                        self.tree.note_focus(leaf);
                    }
                }
                if !self.model.is_visible(ws) {
                    self.park(id);
                }
            }
            Some(_) => {
                self.wins[id].w.kind = WindowKind::Tiled;
                self.attach(id, ws, false);
            }
            None => {
                let area = self
                    .monitors
                    .get(self.ws_monitor(ws))
                    .map(|m| m.info.visible)
                    .unwrap_or_default();
                let r = fit_into(spec.float.unwrap_or(self.wins[id].w.frame), &area);
                self.wins[id].w.kind = WindowKind::Floating;
                if self.model.is_visible(ws) {
                    self.request_frame(id, r);
                } else {
                    self.wins[id].float_frame = Some(r);
                }
                self.attach(id, ws, false);
            }
        }
        let timeout = Duration::from_secs(self.cfg.restore.timeout_s);
        let now = self.now;
        if let Some(s) = self.restore.as_mut() {
            if s.specs.values().any(|sp| sp.key == spec.key) {
                if let Some(d) = s.deadlines.get_mut(&spec.key) {
                    *d = now + timeout;
                }
            } else {
                s.deadlines.remove(&spec.key);
            }
        }
        self.dirty = true;
    }

    /// A fresh window during a session: take a matching slot if any.
    pub(super) fn try_restore_slot(&mut self, id: WindowId) -> bool {
        let Some(s) = &self.restore else {
            return false;
        };
        let Some(sid) = s.best(&match_input(&self.wins[id].w)) else {
            return false;
        };
        self.fill_spec(id, sid);
        let h = self.wins[id].w.handle;
        if self.pending_focus.take_if(|p| *p == h).is_some() {
            self.note_focused(id);
        }
        let ws = self.wins[id].ws;
        self.relayout(ws);
        self.emit_restore();
        true
    }

    /// Prune the placeholders of apps whose deadline passed.
    pub(super) fn expire_restore(&mut self) {
        let now = self.now;
        let Some(s) = self.restore.as_mut() else {
            return;
        };
        let expired: Vec<String> = s
            .deadlines
            .iter()
            .filter(|(_, d)| **d <= now)
            .map(|(k, _)| k.clone())
            .collect();
        if expired.is_empty() {
            return;
        }
        let mut touched: Vec<WorkspaceId> = Vec::new();
        for k in expired {
            tracing::info!(app = %k, "restore: gave up waiting for windows");
            s.deadlines.remove(&k);
            let ids: Vec<u32> = s
                .specs
                .iter()
                .filter(|(_, sp)| sp.key == k)
                .map(|(&i, _)| i)
                .collect();
            for i in ids {
                let Some(sp) = s.specs.remove(&i) else {
                    continue;
                };
                if let Some(l) = sp.leaf
                    && self.tree.contains(l)
                {
                    let root = self.tree.root_of(l);
                    self.tree.remove_leaf(l);
                    self.tree.normalize(root);
                }
                if !touched.contains(&sp.ws) {
                    touched.push(sp.ws);
                }
            }
        }
        for ws in touched {
            self.relayout(ws);
        }
        self.emit_restore();
        self.gc();
        self.dirty = true;
    }

    /// Broadcast progress; end the session when nothing is pending.
    fn emit_restore(&mut self) {
        let Some(s) = &self.restore else {
            return;
        };
        let (pending, total) = (s.pending(), s.total);
        self.emit(IpcEvent::Restore { pending, total });
        self.status_dirty = true;
        if pending == 0 {
            self.restore = None;
        }
    }
}
