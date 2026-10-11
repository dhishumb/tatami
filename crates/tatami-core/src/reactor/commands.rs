//! Actions: what a key binding or a CLI request runs.

use serde_json::json;
use tatami_keymap::KeyChord;
use tatami_protocol::action::{ResizeDelta, SplitArg, Toggle};
use tatami_protocol::{
    Action, Axis, Dir, Layout, LayoutArg, MonitorTarget, Rect, ResizeDim, WindowHandle,
    WorkspaceTarget,
};

use super::{CmdResult, Outcome, Reactor, Reply, creator, fit_into};
use crate::ids::{MonitorId, NodeId, WindowId, WorkspaceId};
use crate::layout;
use crate::platform::{Platform, SetOrder};
use crate::window::WindowKind;
use crate::workspace::{ModelKind, match_monitor_pattern, monitor_in_dir, monitor_next_prev};

/// Bound on tree walks that should end on their own.
const WALK_LIMIT: usize = 256;

fn frame_str(r: &Rect) -> String {
    format!("{}x{}+{}+{}", r.w, r.h, r.x, r.y)
}

fn kind_str(k: WindowKind) -> &'static str {
    match k {
        WindowKind::Tiled => "tiled",
        WindowKind::Floating => "floating",
        WindowKind::Fullscreen => "fullscreen",
        WindowKind::NativeFullscreen => "native-fullscreen",
        WindowKind::Unmanaged => "unmanaged",
    }
}

fn ok() -> CmdResult {
    Ok(Reply::default())
}

impl<P: Platform> Reactor<P> {
    pub(super) fn run_one(&mut self, a: &Action) -> CmdResult {
        match a {
            Action::Focus { dir, wrap } => self.cmd_focus(*dir, *wrap),
            Action::FocusDfs { next } => {
                // "Next tile" cycles: from the last one, wrap to the first.
                let (_, leaf) = self.focused_tiled()?;
                let t = self
                    .dfs_window(leaf, *next, true)
                    .ok_or("no other window")?;
                self.focus_window(t);
                ok()
            }
            Action::FocusBackAndForth => {
                let p = self
                    .prev_focused
                    .filter(|&p| self.wins.contains_key(p))
                    .ok_or("no previous window")?;
                self.focus_window(p);
                ok()
            }
            Action::Move { dir } => self.cmd_move(*dir),
            Action::Swap { dir, wrap } => {
                let (id, leaf) = self.focused_tiled()?;
                let frames =
                    |w: WindowId| self.wins.get(w).map(|m| m.w.expected.unwrap_or(m.w.frame));
                let t = self
                    .tree
                    .neighbor(leaf, *dir, &frames)
                    .or_else(|| wrap.then(|| self.extreme(leaf, *dir)).flatten())
                    .ok_or("nothing to swap with")?;
                self.tree.swap_leaves(leaf, t);
                self.relayout_of(id);
                ok()
            }
            Action::SwapDfs { next } => {
                let (id, leaf) = self.focused_tiled()?;
                let t = self
                    .tree
                    .dfs_next(leaf, *next, true)
                    .ok_or("nothing to swap with")?;
                self.tree.swap_leaves(leaf, t);
                self.relayout_of(id);
                ok()
            }
            Action::Resize { dim, delta } => self.cmd_resize(*dim, *delta),
            Action::Layout(args) => self.cmd_layout(args),
            Action::Fullscreen(t) => self.cmd_fullscreen(*t),
            Action::Workspace { target, wrap } => {
                let ws = self.resolve_ws(target, *wrap)?;
                let sw = self.model.switch(self.focused_monitor, ws);
                self.apply_switch(sw, true);
                ok()
            }
            Action::WorkspaceBackAndForth => {
                let ws = self
                    .model
                    .back_and_forth(self.focused_monitor)
                    .ok_or("no previous workspace")?;
                let sw = self.model.switch(self.focused_monitor, ws);
                self.apply_switch(sw, true);
                ok()
            }
            Action::MoveNodeToWorkspace {
                target,
                focus_follows,
                wrap,
            } => {
                let id = self.focused_win()?;
                let to = self.resolve_ws(target, *wrap)?;
                self.move_node(id, to, *focus_follows)
            }
            Action::MoveNodeToMonitor {
                target,
                focus_follows,
                wrap,
            } => {
                let id = self.focused_win()?;
                let m = self.resolve_monitor(target, *wrap)?;
                let to = self.visible_ws(m).ok_or("monitor shows no workspace")?;
                self.move_node(id, to, *focus_follows)
            }
            Action::MoveWorkspaceToMonitor { target, wrap } => {
                let ws = self
                    .visible_ws(self.focused_monitor)
                    .ok_or("no workspace on the focused monitor")?;
                let m = self.resolve_monitor(target, *wrap)?;
                let layout = self.cfg.layout.default;
                let sw = self.model.move_workspace_to_monitor(
                    ws,
                    m,
                    &mut creator(&mut self.tree, &mut self.workspaces, layout),
                );
                self.apply_switch(sw, true);
                ok()
            }
            Action::SummonWorkspace(name) => {
                if self.model.kind() != ModelKind::Shared {
                    return Err("summon-workspace needs workspaces.model = \"shared\"".to_string());
                }
                let ws = self.resolve_ws(&WorkspaceTarget::Name(name.clone()), false)?;
                let fm = self.focused_monitor;
                let sw = if self.model.is_visible(ws) && self.ws_monitor(ws) != fm {
                    let layout = self.cfg.layout.default;
                    self.model.move_workspace_to_monitor(
                        ws,
                        fm,
                        &mut creator(&mut self.tree, &mut self.workspaces, layout),
                    )
                } else {
                    self.model.switch(fm, ws)
                };
                self.apply_switch(sw, true);
                ok()
            }
            Action::FocusMonitor { target, wrap } => {
                let m = self.resolve_monitor(target, *wrap)?;
                self.focus_monitor(m);
                ok()
            }
            Action::FlattenWorkspaceTree => {
                let ws = self.current_ws()?;
                let root = self.workspaces[ws].root;
                self.tree.flatten(root);
                self.relayout(ws);
                self.dirty = true;
                ok()
            }
            Action::JoinWith(dir) => {
                let (id, leaf) = self.focused_tiled()?;
                if !self.tree.join_with(leaf, *dir) {
                    return Err(format!("nothing to join with {dir}"));
                }
                self.relayout_of(id);
                ok()
            }
            Action::Flip => self.cmd_flip(),
            Action::Split(arg) => {
                let (id, leaf) = self.focused_tiled()?;
                if !self.tree.split(leaf, *arg) {
                    let what = match arg {
                        SplitArg::Horizontal => "horizontal",
                        SplitArg::Vertical => "vertical",
                        SplitArg::Opposite => "opposite",
                    };
                    return Err(format!("split {what}: already split that way"));
                }
                self.relayout_of(id);
                ok()
            }
            Action::BalanceSizes => {
                let ws = self.current_ws()?;
                let root = self.workspaces[ws].root;
                self.tree.balance(root);
                self.relayout(ws);
                self.dirty = true;
                ok()
            }
            Action::Close => {
                let id = self.focused_win()?;
                self.p
                    .close(self.wins[id].w.handle)
                    .map_err(|e| e.to_string())?;
                ok()
            }
            Action::CloseAllWindowsButCurrent => {
                let ws = self.current_ws()?;
                let keep = self.focused;
                for id in self.ws_windows(ws) {
                    if Some(id) != keep {
                        let _ = self.p.close(self.wins[id].w.handle);
                    }
                }
                ok()
            }
            Action::Mode(m) => {
                if !self.keymap.has_mode(m) {
                    return Err(format!("no mode `{m}`"));
                }
                let ch = self.keys.enter_mode(Some(m.clone()), self.now);
                self.set_overlay(ch);
                ok()
            }
            Action::Normal => {
                let ch = self.keys.enter_mode(None, self.now);
                self.set_overlay(ch);
                ok()
            }
            Action::Help => {
                let ch = self.keys.pin();
                self.set_overlay(ch);
                ok()
            }
            Action::Exec(cmd) => {
                self.out.push(Outcome::Exec(cmd.clone()));
                ok()
            }
            Action::ReloadConfig => {
                self.out.push(Outcome::ReloadConfig);
                ok()
            }
            Action::Enable(t) => {
                let on = match t {
                    Toggle::On => true,
                    Toggle::Off => false,
                    Toggle::Toggle => !self.enabled,
                };
                self.set_enabled(on);
                Ok(Reply::text(if on { "enabled" } else { "disabled" }))
            }
            Action::ListWindows { json, all } => Ok(self.list_windows(*json, *all)),
            Action::ListWorkspaces { json, all } => Ok(self.list_workspaces(*json, *all)),
            Action::ListMonitors { json } => Ok(self.list_monitors(*json)),
            Action::ListApps { json } => Ok(self.list_apps(*json)),
            Action::ListModes { json } => Ok(self.list_modes(*json)),
            Action::TriggerBinding { binding, mode } => {
                let chord =
                    KeyChord::parse(binding).ok_or_else(|| format!("bad chord `{binding}`"))?;
                let actions = match mode {
                    Some(m) => self.keymap.lookup_mode(m, &chord),
                    None => self.keymap.lookup_direct(&chord),
                }
                .ok_or_else(|| format!("`{binding}` is not bound"))?
                .to_vec();
                let mut text = String::new();
                for a in &actions {
                    if matches!(a, Action::TriggerBinding { .. }) {
                        return Err("trigger-binding cannot nest".to_string());
                    }
                    let r = self.run_one(a)?;
                    text.push_str(&r.text);
                }
                Ok(Reply::text(text))
            }
            Action::Checkpoint(_) => Err("checkpoint is handled by the daemon".to_string()),
            Action::Subscribe(_) => Err("subscribe is handled by the daemon".to_string()),
            Action::Quit => {
                self.out.push(Outcome::Exit);
                ok()
            }
            Action::Ping => Ok(Reply {
                text: "pong".to_string(),
                data: Some(json!({
                    "trusted": self.trusted,
                    "version": env!("CARGO_PKG_VERSION"),
                    "windows": self.wins.len(),
                    "workspaces": self.workspaces.len(),
                })),
            }),
            Action::Version => Ok(Reply::text(env!("CARGO_PKG_VERSION"))),
            Action::DebugSetFrame { handle, rect } => {
                let h = WindowHandle(*handle);
                self.p
                    .set_window_frame(h, *rect, SetOrder::SizePos)
                    .map_err(|e| e.to_string())?;
                let got = self.p.window_frame(h).ok_or("window vanished")?;
                Ok(Reply::text(frame_str(&got)))
            }
        }
    }

    // ---- helpers -----------------------------------------------------------

    fn focused_win(&self) -> Result<WindowId, String> {
        self.focused
            .filter(|&f| self.wins.contains_key(f))
            .ok_or_else(|| "no focused window".to_string())
    }

    fn focused_tiled(&self) -> Result<(WindowId, NodeId), String> {
        let id = self.focused_win()?;
        self.wins[id]
            .leaf
            .map(|l| (id, l))
            .ok_or_else(|| "the focused window is not tiled".to_string())
    }

    /// The focused window's workspace, else the focused monitor's.
    fn current_ws(&self) -> Result<WorkspaceId, String> {
        self.focused
            .and_then(|f| self.wins.get(f))
            .map(|m| m.ws)
            .or_else(|| self.visible_ws(self.focused_monitor))
            .ok_or_else(|| "no workspace".to_string())
    }

    fn relayout_of(&mut self, id: WindowId) {
        let ws = self.wins[id].ws;
        self.relayout(ws);
        self.dirty = true;
    }

    /// Tiled windows in tree order, then floating ones.
    pub(super) fn ws_windows(&self, ws: WorkspaceId) -> Vec<WindowId> {
        let Some(w) = self.workspaces.get(ws) else {
            return Vec::new();
        };
        let mut v = Vec::new();
        self.tree.for_each_leaf(w.root, &mut |l| {
            if let Some(id) = self.tree.window_of(l) {
                v.push(id);
            }
        });
        v.extend_from_slice(&w.floating);
        v
    }

    /// Next window leaf in DFS order, skipping restore placeholders.
    fn dfs_window(&self, leaf: NodeId, forward: bool, wrap: bool) -> Option<WindowId> {
        let mut cur = leaf;
        for _ in 0..WALK_LIMIT {
            cur = self.tree.dfs_next(cur, forward, wrap)?;
            if cur == leaf {
                return None;
            }
            if let Some(w) = self.tree.window_of(cur) {
                return Some(w);
            }
        }
        None
    }

    /// The leaf farthest in the opposite direction (`--wrap-around`).
    fn extreme(&self, leaf: NodeId, dir: Dir) -> Option<NodeId> {
        let frames = |w: WindowId| self.wins.get(w).map(|m| m.w.expected.unwrap_or(m.w.frame));
        let mut cur = leaf;
        for _ in 0..WALK_LIMIT {
            match self.tree.neighbor(cur, dir.opposite(), &frames) {
                Some(n) if n != cur => cur = n,
                _ => break,
            }
        }
        (cur != leaf).then_some(cur)
    }

    fn monitor_in(&self, dir: Dir) -> Option<MonitorId> {
        let here = self.monitors.get(self.focused_monitor)?.info.frame;
        let others: Vec<(MonitorId, Rect)> = self
            .order
            .iter()
            .filter(|&&m| m != self.focused_monitor)
            .map(|&m| (m, self.monitors[m].info.frame))
            .collect();
        monitor_in_dir(&here, &others, dir)
    }

    fn resolve_monitor(&self, t: &MonitorTarget, wrap: bool) -> Result<MonitorId, String> {
        let found = match t {
            MonitorTarget::Dir(d) => self.monitor_in(*d),
            MonitorTarget::Next => monitor_next_prev(&self.order, self.focused_monitor, true, wrap),
            MonitorTarget::Prev => {
                monitor_next_prev(&self.order, self.focused_monitor, false, wrap)
            }
            MonitorTarget::Pattern(p) => {
                let list: Vec<_> = self
                    .order
                    .iter()
                    .map(|&m| (m, &self.monitors[m].info))
                    .collect();
                match_monitor_pattern(p, &list, &self.order)
            }
        };
        found.ok_or_else(|| "no such monitor".to_string())
    }

    fn resolve_ws(&mut self, t: &WorkspaceTarget, wrap: bool) -> Result<WorkspaceId, String> {
        let layout = self.cfg.layout.default;
        self.model
            .resolve(
                self.focused_monitor,
                t,
                wrap,
                &mut creator(&mut self.tree, &mut self.workspaces, layout),
            )
            .ok_or_else(|| "no such workspace".to_string())
    }

    fn focus_monitor(&mut self, m: MonitorId) {
        if let Some(ws) = self.visible_ws(m) {
            self.focus_ws(ws);
        }
        if self.focused_monitor != m {
            self.focused_monitor = m;
            self.broadcast_focus();
        }
        self.status_dirty = true;
    }

    // ---- focus / move ----------------------------------------------------------

    fn cmd_focus(&mut self, dir: Dir, wrap: bool) -> CmdResult {
        let target = match self
            .focused
            .and_then(|f| self.wins.get(f).map(|m| (f, m.leaf)))
        {
            Some((_, Some(leaf))) => {
                let frames =
                    |w: WindowId| self.wins.get(w).map(|m| m.w.expected.unwrap_or(m.w.frame));
                self.tree
                    .neighbor(leaf, dir, &frames)
                    .or_else(|| wrap.then(|| self.extreme(leaf, dir)).flatten())
                    .and_then(|l| self.tree.window_of(l))
            }
            Some((id, None)) => self.geometric_neighbor(id, dir),
            // Nothing focused yet (fresh start, or focus left to an app we
            // don't manage): the first key just picks a window.
            None => self.default_focus(),
        };
        if let Some(t) = target {
            self.focus_window(t);
            return ok();
        }
        match self.monitor_in(dir) {
            Some(m) => {
                self.focus_monitor(m);
                ok()
            }
            None => Err(format!("no window to the {dir}")),
        }
    }

    /// Nearest window (by centre) in `dir` among the workspace's windows.
    fn geometric_neighbor(&self, id: WindowId, dir: Dir) -> Option<WindowId> {
        let m = &self.wins[id];
        let c = m.w.expected.unwrap_or(m.w.frame).center();
        self.ws_windows(m.ws)
            .into_iter()
            .filter(|&o| o != id)
            .filter_map(|o| {
                let w = &self.wins[o].w;
                let oc = w.expected.unwrap_or(w.frame).center();
                let ahead = match dir {
                    Dir::Left => oc.x < c.x,
                    Dir::Right => oc.x > c.x,
                    Dir::Up => oc.y < c.y,
                    Dir::Down => oc.y > c.y,
                };
                ahead.then(|| (o, (oc.x - c.x).hypot(oc.y - c.y)))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(o, _)| o)
    }

    fn cmd_move(&mut self, dir: Dir) -> CmdResult {
        let (id, leaf) = self.focused_tiled()?;
        if self.tree.move_leaf(leaf, dir) {
            self.relayout_of(id);
            return ok();
        }
        let m = self
            .monitor_in(dir)
            .ok_or_else(|| format!("cannot move {dir}"))?;
        let to = self.visible_ws(m).ok_or("monitor shows no workspace")?;
        self.move_node(id, to, true)
    }

    /// Move a window to another workspace (created on demand by the caller).
    fn move_node(&mut self, id: WindowId, to: WorkspaceId, follow: bool) -> CmdResult {
        let from = self.wins[id].ws;
        if from == to {
            return ok();
        }
        let was_focused = self.focused == Some(id);
        let next = self.detach(id);
        let (fm, tm) = (self.ws_monitor(from), self.ws_monitor(to));
        if self.wins[id].w.kind == WindowKind::Floating && fm != tm {
            // Keep the offset from the monitor's corner.
            let (Some(a), Some(b)) = (self.monitors.get(fm), self.monitors.get(tm)) else {
                return Err("monitor vanished".to_string());
            };
            let (a, b) = (a.info.frame, b.info.visible);
            let m = &mut self.wins[id];
            let base = m.float_frame.unwrap_or(m.w.expected.unwrap_or(m.w.frame));
            let moved = fit_into(
                Rect::new(base.x - a.x + b.x, base.y - a.y + b.y, base.w, base.h),
                &b,
            );
            if m.float_frame.is_some() {
                m.float_frame = Some(moved);
            } else if self.model.is_visible(to) {
                self.request_frame(id, moved);
            } else {
                self.wins[id].float_frame = Some(moved);
            }
        }
        self.attach(id, to, true);
        self.relayout(from);
        self.relayout(to);
        if follow {
            self.focus_window(id);
        } else if was_focused {
            match next {
                Some(n) => self.focus_window(n),
                None => self.focus_ws(from),
            }
        }
        self.dirty = true;
        self.gc();
        ok()
    }

    // ---- layout ------------------------------------------------------------

    fn cmd_layout(&mut self, args: &[LayoutArg]) -> CmdResult {
        let id = self.focused_win()?;
        for &arg in args {
            if self.apply_layout_arg(id, arg) {
                self.relayout_of(id);
                return ok();
            }
        }
        ok()
    }

    fn apply_layout_arg(&mut self, id: WindowId, arg: LayoutArg) -> bool {
        let m = &self.wins[id];
        let (kind, leaf, ws) = (m.w.kind, m.leaf, m.ws);
        match arg {
            LayoutArg::Floating => {
                if !kind.in_tree() {
                    return false;
                }
                self.detach(id);
                self.wins[id].w.kind = WindowKind::Floating;
                self.attach(id, ws, false);
                let _ = self.p.raise(self.wins[id].w.handle);
                true
            }
            LayoutArg::Tiling => {
                if kind != WindowKind::Floating {
                    return false;
                }
                self.detach(id);
                self.wins[id].w.kind = WindowKind::Tiled;
                self.attach(id, ws, true);
                true
            }
            _ => {
                let Some(leaf) = leaf else {
                    return false;
                };
                let cur = self.tree.layout_of(self.tree.container_of(leaf));
                let new = match arg {
                    LayoutArg::Tiles => cur
                        .is_accordion()
                        .then(|| Layout::HTiles.with_axis(cur.axis())),
                    LayoutArg::Accordion => {
                        (!cur.is_accordion()).then(|| Layout::HAccordion.with_axis(cur.axis()))
                    }
                    LayoutArg::Horizontal => {
                        (cur.axis() != Axis::H).then(|| cur.with_axis(Axis::H))
                    }
                    LayoutArg::Vertical => (cur.axis() != Axis::V).then(|| cur.with_axis(Axis::V)),
                    LayoutArg::Exact(l) => (cur != l).then_some(l),
                    LayoutArg::Floating | LayoutArg::Tiling => None,
                };
                match new {
                    Some(l) => {
                        self.tree.set_layout(leaf, l);
                        true
                    }
                    None => false,
                }
            }
        }
    }

    /// `flip`: the one-key split. In a row of windows, the focused one and
    /// its neighbour (right, else left; down, else up in a column) become
    /// a pair stacked the other way; the rest of the row is untouched. On
    /// a pair, flip its orientation; a pair turned back along its parent's
    /// axis merges into the row again, so two presses are a round trip.
    fn cmd_flip(&mut self) -> CmdResult {
        let (id, leaf) = self.focused_tiled()?;
        let parent = self.tree.container_of(leaf);
        let n = self.tree.children(parent).len();
        let layout = self.tree.layout_of(parent);
        let changed = if n == 2 {
            self.tree
                .set_layout(parent, layout.with_axis(layout.axis().other()));
            let root = self.tree.root_of(parent);
            self.tree.normalize(root);
            true
        } else {
            let dirs = match layout.axis() {
                Axis::H => [Dir::Right, Dir::Left],
                Axis::V => [Dir::Down, Dir::Up],
            };
            dirs.into_iter().any(|d| self.tree.join_with(leaf, d))
        };
        if !changed {
            return Err("nothing to flip with".into());
        }
        self.relayout_of(id);
        ok()
    }

    fn cmd_fullscreen(&mut self, t: Toggle) -> CmdResult {
        let (id, _) = self.focused_tiled()?;
        let ws = self.wins[id].ws;
        let cur = self.workspaces[ws].fullscreen;
        let on = match t {
            Toggle::On => true,
            Toggle::Off => false,
            Toggle::Toggle => cur != Some(id),
        };
        if let Some(old) = cur {
            self.wins[old].w.kind = WindowKind::Tiled;
        }
        self.workspaces[ws].fullscreen = on.then_some(id);
        self.wins[id].w.kind = if on {
            WindowKind::Fullscreen
        } else {
            WindowKind::Tiled
        };
        self.relayout_of(id);
        ok()
    }

    fn cmd_resize(&mut self, dim: ResizeDim, delta: ResizeDelta) -> CmdResult {
        let (id, leaf) = self.focused_tiled()?;
        let ws = self.wins[id].ws;
        let w = &self.workspaces[ws];
        let (root, focus) = (w.root, w.focused);
        let area = self
            .monitors
            .get(self.ws_monitor(ws))
            .ok_or("monitor vanished")?
            .info
            .visible;
        let parent_axis = self.tree.layout_of(self.tree.container_of(leaf)).axis();
        let axis = match dim {
            ResizeDim::Width => Axis::H,
            ResizeDim::Height => Axis::V,
            ResizeDim::Smart => parent_axis,
            ResizeDim::SmartOpposite => parent_axis.other(),
        };
        let len = |r: Rect| match axis {
            Axis::H => r.w,
            Axis::V => r.h,
        };
        // Lengths of the leaf and its ancestors: what `Tree::resize` asks for.
        let mut sizes: Vec<(NodeId, f64)> = Vec::new();
        let mut cur = Some(leaf);
        let wins = &self.wins;
        let min = |w: WindowId| wins.get(w).and_then(|m| m.w.stubborn);
        let params = layout::Params {
            area,
            gaps: &self.gaps,
            focused: focus,
            accordion_padding: self.pad,
            min: &min,
        };
        while let Some(n) = cur {
            if let Some(r) = layout::node_rect(&self.tree, root, &params, n) {
                sizes.push((n, len(r)));
            }
            cur = self.tree.parent(n);
        }
        let d = match delta {
            ResizeDelta::Add(x) => x,
            ResizeDelta::Sub(x) => -x,
            ResizeDelta::Set(x) => x - sizes.first().map(|s| s.1).unwrap_or(x),
        };
        let lookup = |n: NodeId| sizes.iter().find(|s| s.0 == n).map(|s| s.1);
        if !self.tree.resize(leaf, axis, d, &lookup) {
            return Err("cannot resize further".to_string());
        }
        self.relayout_of(id);
        ok()
    }

    // ---- listings ------------------------------------------------------------

    fn list_monitors(&self, as_json: bool) -> Reply {
        let mut text = String::new();
        let mut rows = Vec::new();
        for (i, &m) in self.order.iter().enumerate() {
            let info = &self.monitors[m].info;
            let focused = m == self.focused_monitor;
            let ws = self.visible_ws(m).map(|w| self.ws_name(w)).unwrap_or("");
            if as_json {
                rows.push(json!({
                    "index": i + 1, "uuid": info.uuid, "name": info.name,
                    "frame": info.frame, "visible": info.visible, "main": info.is_main,
                    "focused": focused, "workspace": ws,
                }));
            } else {
                text.push_str(&format!(
                    "{} {} {} {}{}\n",
                    i + 1,
                    info.uuid,
                    info.name,
                    frame_str(&info.frame),
                    if focused { " focused" } else { "" }
                ));
            }
        }
        finish_list(text, rows, as_json)
    }

    fn list_workspaces(&self, as_json: bool, all: bool) -> Reply {
        let mut text = String::new();
        let mut rows = Vec::new();
        for (i, &m) in self.order.iter().enumerate() {
            for ws in self.model.workspaces_on(m) {
                let visible = self.model.is_visible(ws);
                let focused = visible && m == self.focused_monitor;
                let n = self.ws_windows(ws).len();
                if !all && n == 0 && !visible {
                    continue;
                }
                let name = self.ws_name(ws);
                if as_json {
                    rows.push(json!({
                        "name": name, "monitor": self.monitors[m].info.uuid,
                        "monitor_index": i + 1, "visible": visible, "focused": focused,
                        "windows": n,
                    }));
                } else {
                    text.push_str(&format!(
                        "{name} monitor={} windows={n}{}{}\n",
                        i + 1,
                        if visible { " visible" } else { "" },
                        if focused { " focused" } else { "" }
                    ));
                }
            }
        }
        finish_list(text, rows, as_json)
    }

    fn list_windows(&self, as_json: bool, all: bool) -> Reply {
        let wss: Vec<WorkspaceId> = if all {
            self.model.all()
        } else {
            self.visible_ws(self.focused_monitor).into_iter().collect()
        };
        let mut text = String::new();
        let mut rows = Vec::new();
        for ws in wss {
            let name = self.ws_name(ws);
            let mut ids = self.ws_windows(ws);
            if all {
                ids.extend(
                    self.wins
                        .iter()
                        .filter(|(id, m)| m.ws == ws && !ids.contains(id))
                        .map(|(id, _)| id)
                        .collect::<Vec<_>>(),
                );
            }
            for id in ids {
                let w = &self.wins[id].w;
                if as_json {
                    rows.push(json!({
                        "handle": w.handle.0, "app": w.app_name, "bundle_id": w.bundle_id,
                        "title": w.title, "workspace": name,
                        "monitor": self.mon_uuid(self.ws_monitor(ws)),
                        "frame": w.frame, "kind": kind_str(w.kind),
                        "focused": self.focused == Some(id), "minimized": w.minimized,
                    }));
                } else {
                    text.push_str(&format!(
                        "{} {} {} ws={name} {}\n",
                        w.handle.0,
                        w.app_name,
                        w.title,
                        frame_str(&w.frame)
                    ));
                }
            }
        }
        finish_list(text, rows, as_json)
    }

    fn list_apps(&self, as_json: bool) -> Reply {
        let mut apps: Vec<_> = self.apps.values().map(|a| &a.info).collect();
        apps.sort_by_key(|a| a.pid);
        let mut text = String::new();
        let mut rows = Vec::new();
        for a in apps {
            if as_json {
                rows.push(json!({ "pid": a.pid, "bundle_id": a.bundle_id, "name": a.name }));
            } else {
                text.push_str(&format!(
                    "{} {} {}\n",
                    a.pid,
                    a.bundle_id.as_deref().unwrap_or("-"),
                    a.name
                ));
            }
        }
        finish_list(text, rows, as_json)
    }

    fn list_modes(&self, as_json: bool) -> Reply {
        let current = self.keys.mode();
        if as_json {
            return Reply {
                text: String::new(),
                data: Some(json!({ "modes": self.keymap.modes(), "current": current })),
            };
        }
        let mut text = String::new();
        for m in self.keymap.modes() {
            text.push_str(m);
            if Some(m.as_str()) == current {
                text.push_str(" *");
            }
            text.push('\n');
        }
        Reply::text(text)
    }
}

fn finish_list(mut text: String, rows: Vec<serde_json::Value>, as_json: bool) -> Reply {
    if as_json {
        return Reply {
            text: String::new(),
            data: Some(serde_json::Value::Array(rows)),
        };
    }
    if text.ends_with('\n') {
        text.pop();
    }
    Reply::text(text)
}
