//! The reactor: owns the platform and every piece of window-manager state,
//! and turns platform events, key presses, CLI requests and timer ticks into
//! platform calls plus a list of `Outcome`s for the daemon.
//!
//! Cost model. Every entry point is O(windows in the touched workspace); the
//! platform is read only when a decision needs it (a frame verify, an app's
//! windows on launch). Layout reuses one placement buffer, frame requests are
//! skipped when the window already sits where it should, and an echo of our
//! own move is recognised by `expected` and dropped without a relayout.
//!
//! Time comes in through every entry point (`now`) and is kept in
//! `self.now`; nothing here reads the clock, so tests are deterministic.

mod commands;
mod restore;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use slotmap::SlotMap;
use tatami_config::Config;
use tatami_keymap::{
    KeyChord, KeyInput, KeyName, KeyState, Keymap, Level, OverlayChange, keyname_from_keycode,
};
use tatami_protocol::{Action, IpcEvent, Layout, Rect, Response, WindowHandle};
use tatami_state::SavedState;

use crate::ids::{MonitorId, NodeId, WindowId, WorkspaceId};
use crate::layout::{self, Gaps, Insets, Placement};
use crate::parking::{self, Corner};
use crate::platform::{
    AppInfo, KeyDisposition, MenuItem, Mods, MonitorInfo, OverlayContent, OverlayRow, Platform,
    PlatformEvent, RawKeyEvent, SetOrder, WindowInfo,
};
use crate::tree::Tree;
use crate::window::{Window, WindowKind};
use crate::workspace::{ModelKind, Switch, Workspace, WorkspaceModel, match_monitor_pattern};

use restore::RestoreSession;

/// What the daemon must do after a reactor step.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// `checkpoint …` from a key or the menu; the daemon owns the files.
    Checkpoint(tatami_protocol::CheckpointCmd),
    /// Layout state changed; persist it (the daemon debounces).
    Dirty,
    Broadcast(IpcEvent),
    /// `exec-and-forget`: run through `/bin/sh -c`.
    Exec(String),
    Exit,
    ReloadConfig,
}

/// Frame-transaction verify delays after attempt 1, 2 and 3.
/// How long a failed key's message stays in the menu bar.
/// Apps that appear without an `AppLaunched` (not registered with
/// LaunchServices) are found by scanning the window server this often.
const APP_SCAN_EVERY: Duration = Duration::from_secs(3);
/// A hand-dragged tile is acted on this long after its last move event.
const DRAG_SETTLE: Duration = Duration::from_millis(200);
const FLASH_FOR: Duration = Duration::from_millis(2500);
/// Read-back deadlines after each attempt. Short, because a window that is
/// going to refuse a size does so at once and every retry is a visible
/// twitch; a slow app (Electron) still gets three chances.
const VERIFY_AFTER: [Duration; 3] = [
    Duration::from_millis(40),
    Duration::from_millis(90),
    Duration::from_millis(250),
];
/// A frame request is skipped when the window is already this close.
const SKIP_EPS: f64 = 1.0;
/// A reported frame counts as the one we asked for within this.
const MATCH_EPS: f64 = 2.0;

/// A frame request awaiting verification.
#[derive(Debug, Clone, Copy)]
struct FrameTx {
    win: WindowId,
    target: Rect,
    /// 1-based.
    attempt: u8,
    deadline: Instant,
}

/// A window plus where the reactor keeps it.
#[derive(Debug, Clone)]
struct Managed {
    w: Window,
    ws: WorkspaceId,
    /// Leaf in `ws`'s tree for tiled/fullscreen windows that are not minimized.
    leaf: Option<NodeId>,
    /// Floating window's frame from before it was parked.
    float_frame: Option<Rect>,
}

#[derive(Debug, Clone)]
struct Mon {
    info: MonitorInfo,
    corner: Corner,
}

#[derive(Debug, Clone)]
struct App {
    info: AppInfo,
    observed: bool,
}

/// A command's result before it becomes a `Response`.
#[derive(Debug, Default)]
struct Reply {
    text: String,
    data: Option<serde_json::Value>,
}

impl Reply {
    fn text(s: impl Into<String>) -> Self {
        Reply {
            text: s.into(),
            data: None,
        }
    }
}

type CmdResult = Result<Reply, String>;

pub struct Reactor<P: Platform> {
    p: P,
    cfg: Config,
    keymap: Keymap<Action>,
    keys: KeyState,
    gaps: Gaps,
    pad: f64,
    now: Instant,

    tree: Tree,
    wins: SlotMap<WindowId, Managed>,
    by_handle: HashMap<WindowHandle, WindowId>,
    apps: HashMap<i32, App>,
    monitors: SlotMap<MonitorId, Mon>,
    /// Left to right by `frame.x`.
    order: Vec<MonitorId>,
    model: WorkspaceModel,
    workspaces: SlotMap<WorkspaceId, Workspace>,

    focused_monitor: MonitorId,
    focused: Option<WindowId>,
    prev_focused: Option<WindowId>,
    /// A `WindowFocused` for a handle we don't know yet (AX sends it before
    /// `WindowCreated`); applied when the window arrives.
    pending_focus: Option<WindowHandle>,
    /// Focus events our own z-order raises will echo; dropped once each.
    raise_echo: Vec<WindowHandle>,

    txs: Vec<FrameTx>,
    restore: Option<RestoreSession>,
    next_spec: u32,

    key_queue: Vec<Action>,
    overlay_pending: Option<OverlayChange>,
    reported_mode: Option<String>,
    status: String,
    menu: Vec<MenuItem>,
    status_dirty: bool,
    /// A short message in the status item (a key that failed), and when it expires.
    flash: Option<(String, Instant)>,
    /// The sheet is showing the desk overview instead of the keys.
    overview: bool,
    /// Window per letter on the overview (`a`, `b`, …), in display order.
    overview_targets: Vec<WindowId>,
    /// A window picked on the overview, focused on the next `drain`.
    overview_pick: Option<WindowId>,
    /// A tiled window the user is moving by hand, and when to act on it.
    drag: Option<(WindowId, Instant)>,
    /// Windows whose app refused a frame write outright; floated at the end of the step.
    unmanageable: Vec<WindowId>,
    /// Next window-server scan for apps NSWorkspace never told us about.
    next_app_scan: Instant,

    enabled: bool,
    trusted: bool,
    dirty: bool,
    out: Vec<Outcome>,
    scratch: Vec<Placement>,
    scratch_ids: Vec<WindowId>,
}

fn gaps_of(cfg: &Config) -> Gaps {
    let o = cfg.gaps.outer;
    Gaps {
        inner: f64::from(cfg.gaps.inner),
        outer: Insets {
            top: f64::from(o.top),
            bottom: f64::from(o.bottom),
            left: f64::from(o.left),
            right: f64::from(o.right),
        },
    }
}

fn model_kind(cfg: &Config) -> ModelKind {
    match cfg.workspaces.model {
        tatami_config::WorkspaceModel::PerMonitor => ModelKind::PerMonitor,
        tatami_config::WorkspaceModel::Shared => ModelKind::Shared,
    }
}

fn key_mods(m: Mods) -> tatami_keymap::Mods {
    tatami_keymap::Mods {
        alt: m.alt,
        shift: m.shift,
        ctrl: m.ctrl,
        cmd: m.cmd,
    }
}

/// Modifier words of a chord for the overlay title (`alt`, `ctrl alt`).
fn mods_words(m: tatami_keymap::Mods, out: &mut String) {
    for (on, w) in [(m.cmd, "cmd"), (m.ctrl, "ctrl"), (m.alt, "alt")] {
        if on {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(w);
        }
    }
}

fn chord_words(c: &KeyChord, out: &mut String) {
    let strip = c.mods.without(tatami_keymap::Mods::SHIFT);
    mods_words(strip, out);
    if !out.is_empty() {
        out.push(' ');
    }
    out.push_str(&c.display_relative(strip));
}

/// `^`/`$`-anchored, case-insensitive substring match. Rules carry regex
/// text but there is no regex crate here; anchors cover the common cases.
fn text_match(pat: &str, text: &str) -> bool {
    let pat = pat.to_lowercase();
    let text = text.to_lowercase();
    let (start, rest) = match pat.strip_prefix('^') {
        Some(r) => (true, r),
        None => (false, pat.as_str()),
    };
    let (end, core) = match rest.strip_suffix('$') {
        Some(c) => (true, c),
        None => (false, rest),
    };
    match (start, end) {
        (true, true) => text == core,
        (true, false) => text.starts_with(core),
        (false, true) => text.ends_with(core),
        (false, false) => text.contains(core),
    }
}

/// `frame` is where `target` asked, allowing for a size the app insists on.
fn satisfied(w: &Window, frame: &Rect, target: &Rect, eps: f64) -> bool {
    if frame.approx_eq(target, eps) {
        return true;
    }
    w.stubborn.is_some_and(|s| {
        (frame.x - target.x).abs() <= eps
            && (frame.y - target.y).abs() <= eps
            && (frame.w - s.w).abs() <= eps
            && (frame.h - s.h).abs() <= eps
    })
}

/// Workspace factory for `WorkspaceModel` callbacks.
fn creator<'a>(
    tree: &'a mut Tree,
    wss: &'a mut SlotMap<WorkspaceId, Workspace>,
    layout: Layout,
) -> impl FnMut(&str, MonitorId) -> WorkspaceId + 'a {
    move |name, m| {
        let root = tree.new_root(layout);
        wss.insert(Workspace::new(name, m, root))
    }
}

/// `r` re-centred inside `area` when its centre is outside, shrunk to fit.
/// Letters for the overview's windows: `a`…`z` then `A`…`Z`.
fn overview_letter(i: usize) -> Option<char> {
    match i {
        0..=25 => char::from_u32('a' as u32 + i as u32),
        26..=51 => char::from_u32('A' as u32 + (i - 26) as u32),
        _ => None,
    }
}

fn overview_index(c: char) -> Option<usize> {
    match c {
        'a'..='z' => Some(c as usize - 'a' as usize),
        'A'..='Z' => Some(26 + c as usize - 'A' as usize),
        _ => None,
    }
}

fn fit_into(r: Rect, area: &Rect) -> Rect {
    if area.contains(r.center()) {
        return r;
    }
    centered(r, area)
}

/// `r`'s size (shrunk to fit) centred in `area`.
fn centered(r: Rect, area: &Rect) -> Rect {
    let w = r.w.min(area.w);
    let h = r.h.min(area.h);
    Rect::new(
        area.x + (area.w - w) / 2.0,
        area.y + (area.h - h) / 2.0,
        w,
        h,
    )
}

impl<P: Platform> Reactor<P> {
    /// Reads screens/apps/windows from the platform, observes every regular
    /// app, adopts existing windows into workspace trees (first configured
    /// name per monitor), un-parks any stray parked windows, starts the
    /// restore session if `saved` is given and restore is enabled. Does NOT
    /// install the key tap (daemon does).
    pub fn new(platform: P, config: Config, saved: Option<SavedState>, now: Instant) -> Self {
        let (keymap, _warnings) = config.keymap();
        let keys = KeyState::new(config.general.which_key_delay(), config.general.leader);
        let model = WorkspaceModel::new(model_kind(&config), config.workspaces.names.clone());
        let mut r = Reactor {
            p: platform,
            gaps: gaps_of(&config),
            pad: f64::from(config.layout.accordion_padding),
            cfg: config,
            keymap,
            keys,
            now,
            tree: Tree::new(),
            wins: SlotMap::with_key(),
            by_handle: HashMap::new(),
            apps: HashMap::new(),
            monitors: SlotMap::with_key(),
            order: Vec::new(),
            model,
            workspaces: SlotMap::with_key(),
            focused_monitor: MonitorId::default(),
            focused: None,
            prev_focused: None,
            pending_focus: None,
            raise_echo: Vec::new(),
            txs: Vec::new(),
            restore: None,
            next_spec: 0,
            key_queue: Vec::new(),
            overlay_pending: None,
            reported_mode: None,
            status: String::new(),
            menu: Vec::new(),
            status_dirty: true,
            enabled: true,
            trusted: true,
            dirty: false,
            flash: None,
            overview: false,
            overview_targets: Vec::new(),
            overview_pick: None,
            drag: None,
            unmanageable: Vec::new(),
            next_app_scan: now + APP_SCAN_EVERY,
            out: Vec::new(),
            scratch: Vec::new(),
            scratch_ids: Vec::new(),
        };
        r.sync_screens();
        r.rescan();
        let focus =
            r.p.focused_window()
                .and_then(|h| r.by_handle.get(&h).copied())
                .or_else(|| r.default_focus());
        if let Some(id) = focus {
            r.note_focused(id);
        }
        r.relayout_all();
        if let Some(s) = saved
            && r.cfg.restore.enabled
        {
            r.restore_inner(s);
        }
        // Nobody is subscribed yet; the first `drain` sets the status.
        r.out.clear();
        r.dirty = false;
        r
    }

    pub fn platform(&mut self) -> &mut P {
        &mut self.p
    }

    pub fn trusted(&self) -> bool {
        self.trusted
    }

    pub fn focused_monitor(&self) -> MonitorInfo {
        match self.monitors.get(self.focused_monitor) {
            Some(m) => m.info.clone(),
            None => MonitorInfo {
                uuid: String::new(),
                name: String::new(),
                frame: Rect::default(),
                visible: Rect::default(),
                is_main: true,
            },
        }
    }

    // ---- entry points ----------------------------------------------------

    pub fn on_platform_event(&mut self, ev: PlatformEvent, now: Instant) -> Vec<Outcome> {
        self.now = now;
        match ev {
            PlatformEvent::AppLaunched(info) => self.on_app_launched(info),
            PlatformEvent::AppTerminated(pid) => self.on_app_terminated(pid),
            PlatformEvent::AppActivated(pid) => {
                match self.p.focused_window() {
                    Some(h) if self.by_handle.contains_key(&h) => self.on_window_focused(h),
                    _ => {
                        // An app we don't manage (or macOS can't tell us its
                        // window) took focus: an unknown app may have just
                        // appeared, and "where the user is" is the pointer.
                        if !self.apps.contains_key(&pid) {
                            self.rescan_new_apps();
                        }
                        if let Some(m) = self.monitor_under_mouse()
                            && m != self.focused_monitor
                        {
                            self.focused_monitor = m;
                            self.status_dirty = true;
                            self.broadcast_focus();
                        }
                    }
                }
            }
            PlatformEvent::WindowCreated(info) => {
                if !self.by_handle.contains_key(&info.handle) {
                    self.adopt(info, true);
                }
            }
            PlatformEvent::WindowDestroyed(h) => {
                if let Some(id) = self.by_handle.get(&h).copied() {
                    self.forget_window(id);
                }
            }
            PlatformEvent::WindowFocused(h) => self.on_window_focused(h),
            PlatformEvent::WindowMoved(h, r) | PlatformEvent::WindowResized(h, r) => {
                self.on_window_frame(h, r)
            }
            PlatformEvent::WindowMinimized(h, on) => self.on_minimized(h, on),
            PlatformEvent::TitleChanged(h, t) => {
                if let Some(id) = self.by_handle.get(&h).copied() {
                    self.wins[id].w.title = t;
                    if self.focused == Some(id) {
                        self.broadcast_focus();
                    }
                }
            }
            PlatformEvent::ScreensChanged => self.on_screens_changed(),
            PlatformEvent::Command(args) => match Action::parse(&args) {
                Ok(a) => {
                    if let Err(e) = self.run_one(&a) {
                        tracing::warn!(?args, "menu command failed: {e}");
                    }
                }
                Err(e) => tracing::warn!(?args, "menu command: {e}"),
            },
            PlatformEvent::TrustChanged(on) => {
                let was = self.trusted;
                self.trusted = on;
                if !on {
                    let ch = self.keys.focus_lost();
                    self.set_overlay(ch);
                } else if !was && self.enabled {
                    self.rescan();
                    self.relayout_all();
                }
            }
        }
        self.finish()
    }

    /// Runs only the KeyState; queues resulting actions. Called from inside
    /// the event tap: never touches the platform.
    pub fn on_key(&mut self, raw: RawKeyEvent, now: Instant) -> KeyDisposition {
        if self.overview
            && let RawKeyEvent::Down { keycode, mods, .. } = raw
            && !mods.ctrl
            && !mods.cmd
        {
            // The overview is a switcher: a letter focuses that window;
            // Esc closes; anything else closes and goes on as a normal key.
            let picked = match keyname_from_keycode(keycode) {
                Some(KeyName::Char(c)) => {
                    overview_index(c).and_then(|i| self.overview_targets.get(i).copied())
                }
                _ => None,
            };
            let ch = self.keys.enter_mode(None, now);
            self.set_overlay(ch);
            if let Some(id) = picked {
                self.overview_pick = Some(id);
                return KeyDisposition::Swallow;
            }
            if keyname_from_keycode(keycode) == Some(KeyName::Escape) {
                return KeyDisposition::Swallow;
            }
        }
        let input = match raw {
            RawKeyEvent::Down {
                keycode,
                mods,
                repeat,
            } => match keyname_from_keycode(keycode) {
                Some(k) => KeyInput::Down {
                    chord: KeyChord::new(k, key_mods(mods)),
                    repeat,
                },
                None => return KeyDisposition::Pass,
            },
            RawKeyEvent::Up { keycode, mods } => match keyname_from_keycode(keycode) {
                Some(k) => KeyInput::Up(KeyChord::new(k, key_mods(mods))),
                None => return KeyDisposition::Pass,
            },
            RawKeyEvent::Flags { mods } => KeyInput::Flags(key_mods(mods)),
        };
        let d = self
            .keys
            .on_input(&self.keymap, input, now, tatami_config::mode_action);
        tracing::trace!(?raw, swallow = d.swallow, actions = ?d.actions, mode = ?self.keys.mode(), "key");
        self.key_queue
            .extend(d.actions.into_iter().filter(|a| !a.is_mode_change()));
        self.set_overlay(d.overlay);
        if d.swallow {
            KeyDisposition::Swallow
        } else {
            KeyDisposition::Pass
        }
    }

    /// Executes queued key actions and applies pending overlay/status changes.
    pub fn drain(&mut self, now: Instant) -> Vec<Outcome> {
        self.now = now;
        if !self.key_queue.is_empty() {
            self.follow_mouse();
        }
        if let Some(id) = self.overview_pick.take()
            && self.wins.contains_key(id)
        {
            self.focus_window(id);
        }
        let mut q = std::mem::take(&mut self.key_queue);
        for a in &q {
            if let Err(e) = self.run_one(a) {
                tracing::info!(action = ?a, "key action failed: {e}");
                self.flash = Some((e, self.now + FLASH_FOR));
                self.status_dirty = true;
            }
        }
        q.clear();
        if self.key_queue.is_empty() {
            self.key_queue = q;
        }
        self.finish()
    }

    /// Parse + run a CLI request. `subscribe` is handled by the daemon.
    pub fn handle_request(&mut self, args: &[String], now: Instant) -> (Response, Vec<Outcome>) {
        self.now = now;
        self.follow_mouse();
        let resp = match Action::parse(args) {
            Err(e) => Response::err(0, e.to_string()),
            Ok(a) => match self.run_one(&a) {
                Ok(Reply {
                    text,
                    data: Some(d),
                }) => Response::with_data(0, text, d),
                Ok(Reply { text, data: None }) => Response::ok(0, text),
                Err(e) => Response::err(0, e),
            },
        };
        (resp, self.finish())
    }

    /// Run actions in order, stopping at the first failure.
    pub fn run_actions(
        &mut self,
        actions: &[Action],
        now: Instant,
    ) -> (Result<String, String>, Vec<Outcome>) {
        self.now = now;
        let mut text = String::new();
        let mut res = Ok(());
        for a in actions {
            match self.run_one(a) {
                Ok(r) => {
                    if !r.text.is_empty() {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(&r.text);
                    }
                }
                Err(e) => {
                    res = Err(e);
                    break;
                }
            }
        }
        (res.map(|()| text), self.finish())
    }

    /// Which-key clock, frame-transaction verify, restore deadlines.
    pub fn on_timer(&mut self, now: Instant) -> Vec<Outcome> {
        self.now = now;
        if let Some(ch) = self.keys.tick(now) {
            self.set_overlay(ch);
        }
        self.verify_frames();
        self.expire_restore();
        if now >= self.next_app_scan {
            self.next_app_scan = now + APP_SCAN_EVERY;
            self.rescan_new_apps();
        }
        self.settle_drag();
        if self.flash.as_ref().is_some_and(|(_, until)| *until <= now) {
            self.flash = None;
            self.status_dirty = true;
        }
        self.finish()
    }

    /// Whether a frame transaction is still being verified.
    pub fn tx_pending(&self) -> bool {
        !self.txs.is_empty()
    }

    pub fn next_due(&self) -> Option<Instant> {
        let tx = self.txs.iter().map(|t| t.deadline).min();
        let rs = self.restore.as_ref().and_then(|s| s.next_deadline());
        let fl = self.flash.as_ref().map(|(_, t)| *t);
        let dr = self.drag.map(|(_, t)| t);
        [self.keys.due(), tx, rs, fl, dr, Some(self.next_app_scan)]
            .into_iter()
            .flatten()
            .min()
    }

    /// Rebuild keymap/KeyState, gaps, relayout all.
    pub fn reload_config(&mut self, config: Config, now: Instant) -> Vec<Outcome> {
        self.now = now;
        let (km, _warnings) = config.keymap();
        self.keymap = km;
        self.keys
            .reconfigure(config.general.which_key_delay(), config.general.leader);
        if let Some(m) = self.keys.mode()
            && !self.keymap.has_mode(m)
        {
            let ch = self.keys.enter_mode(None, now);
            self.set_overlay(ch);
        }
        if model_kind(&config) != self.model.kind() {
            tracing::warn!("workspaces.model changes take effect on restart");
        }
        self.gaps = gaps_of(&config);
        self.pad = f64::from(config.layout.accordion_padding);
        self.cfg = config;
        self.resolve_forced();
        self.relayout_all();
        self.status_dirty = true;
        self.finish()
    }

    /// Un-park everything, hide overlay/status.
    pub fn shutdown(&mut self) -> Vec<Outcome> {
        self.unpark_all();
        let mon = self.focused_monitor();
        self.p.show_overlay(None, &mon);
        self.p.set_status("");
        self.status.clear();
        std::mem::take(&mut self.out)
    }

    /// `off`: un-park all and stop managing (events are still tracked);
    /// `on`: re-adopt and relayout.
    pub fn set_enabled(&mut self, on: bool) {
        if on == self.enabled {
            return;
        }
        if on {
            self.enabled = true;
            self.rescan();
            self.relayout_all();
        } else {
            self.unpark_all();
            self.txs.clear();
            self.enabled = false;
        }
        self.status_dirty = true;
    }

    // ---- bookkeeping -------------------------------------------------------

    fn set_overlay(&mut self, ch: OverlayChange) {
        if ch != OverlayChange::Keep {
            self.overlay_pending = Some(ch);
        }
    }

    fn emit(&mut self, ev: IpcEvent) {
        self.out.push(Outcome::Broadcast(ev));
    }

    /// Apply deferred overlay/status/mode changes and hand back outcomes.
    fn finish(&mut self) -> Vec<Outcome> {
        while let Some(id) = self.unmanageable.pop() {
            if self.wins.get(id).is_some_and(|m| m.w.kind.in_tree()) {
                let ws = self.wins[id].ws;
                self.detach(id);
                self.wins[id].w.kind = WindowKind::Floating;
                self.attach(id, ws, false);
                self.relayout(ws);
            }
        }
        if let Some(ch) = self.overlay_pending.take() {
            self.apply_overlay(ch);
        }
        if self.keys.mode() != self.reported_mode.as_deref() {
            self.reported_mode = self.keys.mode().map(str::to_string);
            let mode = self.reported_mode.clone();
            self.emit(IpcEvent::ModeChanged { mode });
            self.status_dirty = true;
        }
        if self.status_dirty {
            self.status_dirty = false;
            let text = self.status_text();
            if text != self.status {
                self.p.set_status(&text);
                self.status = text;
            }
            let menu = self.menu_items();
            if menu != self.menu {
                self.p.set_menu(&menu);
                self.menu = menu;
            }
        }
        if std::mem::take(&mut self.dirty) {
            self.out.push(Outcome::Dirty);
        }
        std::mem::take(&mut self.out)
    }

    /// The status item's menu: every monitor's workspaces (the visible one
    /// checked, the focused monitor marked), then the service actions.
    fn menu_items(&self) -> Vec<MenuItem> {
        let mut rows = Vec::new();
        let mut mons: Vec<(MonitorId, &MonitorInfo)> =
            self.monitors.iter().map(|(id, m)| (id, &m.info)).collect();
        mons.sort_by(|a, b| a.1.frame.x.total_cmp(&b.1.frame.x));
        for (id, info) in mons {
            let mark = if id == self.focused_monitor {
                " ◆"
            } else {
                ""
            };
            rows.push(MenuItem::header(format!("{}{mark}", info.name)));
            let visible = self.model.visible_on(id);
            for ws in self.model.workspaces_on(id) {
                let Some(name) = self.model.name_of(ws) else {
                    continue;
                };
                let n = self.ws_windows(ws).len();
                let title = if n == 1 {
                    format!("{name}  (1 window)")
                } else {
                    format!("{name}  ({n} windows)")
                };
                let mut item = MenuItem::action(title, &["workspace", name]);
                item.checked = visible == Some(ws);
                item.indent = 1;
                rows.push(item);
            }
        }
        rows.push(MenuItem::separator());
        if let Some(m) = self.keys.mode() {
            rows.push(MenuItem::action(
                format!("Leave {m} mode"),
                &["mode", "normal"],
            ));
        }
        rows.push(MenuItem::action("Show keys", &["help"]));
        rows.push(MenuItem::action("Balance sizes", &["balance-sizes"]));
        rows.push(MenuItem::action(
            "Flatten workspace",
            &["flatten-workspace-tree"],
        ));
        let mut en = MenuItem::action("Tiling enabled", &["enable", "toggle"]);
        en.checked = self.enabled;
        rows.push(en);
        rows.push(MenuItem::separator());
        rows.push(MenuItem::action("Save checkpoint", &["checkpoint", "save"]));
        rows.push(MenuItem::action("Reload config", &["reload-config"]));
        rows.push(MenuItem::action(
            "Edit config…",
            &[
                "exec-and-forget",
                "open",
                "-t",
                "~/.config/tatami/config.toml",
            ],
        ));
        rows.push(MenuItem::separator());
        rows.push(MenuItem::action("Quit tatami", &["quit"]));
        rows
    }

    fn status_text(&self) -> String {
        let ws = self
            .model
            .visible_on(self.focused_monitor)
            .and_then(|w| self.model.name_of(w))
            .unwrap_or("");
        let mut s = format!("[{ws}]");
        if let Some(m) = self.keys.mode() {
            s.push(' ');
            s.push_str(&m.to_uppercase());
        }
        if let Some(r) = &self.restore {
            s.push_str(&format!(" restore {}/{}", r.pending(), r.total));
        }
        if !self.enabled {
            s.push_str(" off");
        }
        if let Some((msg, _)) = &self.flash {
            s.push_str(" · ");
            s.push_str(msg);
        }
        s
    }

    fn leader_words(&self) -> String {
        let mut s = String::new();
        mods_words(self.keys.leader(), &mut s);
        s
    }

    /// The which-key sheet for `level`.
    /// The desk: every monitor (left to right), its workspaces (visible one
    /// starred), and one lettered row per window, for the `overview` chord.
    /// Returns the rows and the window each letter stands for.
    fn overview_rows(&self) -> (Vec<OverlayRow>, Vec<WindowId>) {
        let mut mons: Vec<(MonitorId, &MonitorInfo)> =
            self.monitors.iter().map(|(id, m)| (id, &m.info)).collect();
        mons.sort_by(|a, b| a.1.frame.x.total_cmp(&b.1.frame.x));
        let mut rows = Vec::new();
        let mut targets = Vec::new();
        for (i, (id, info)) in mons.iter().enumerate() {
            let here = if *id == self.focused_monitor {
                "◆ "
            } else {
                ""
            };
            rows.push(OverlayRow {
                key: String::new(),
                desc: format!("{here}{}  {}", i + 1, info.name),
                prefix: true,
            });
            let visible = self.model.visible_on(*id);
            for ws in self.model.workspaces_on(*id) {
                let Some(name) = self.model.name_of(ws) else {
                    continue;
                };
                let mark = if visible == Some(ws) { "*" } else { "" };
                rows.push(OverlayRow {
                    key: String::new(),
                    desc: format!("   workspace {name}{mark}"),
                    prefix: true,
                });
                for w in self.ws_windows(ws) {
                    let m = &self.wins[w];
                    let key = overview_letter(targets.len())
                        .map(String::from)
                        .unwrap_or_default();
                    let float = if m.w.kind == WindowKind::Floating {
                        "~ "
                    } else {
                        ""
                    };
                    let desc = if m.w.title.is_empty() || m.w.title == m.w.app_name {
                        format!("{float}{}", m.w.app_name)
                    } else {
                        format!("{float}{} — {}", m.w.app_name, m.w.title)
                    };
                    rows.push(OverlayRow {
                        key,
                        desc,
                        prefix: false,
                    });
                    targets.push(w);
                }
            }
        }
        (rows, targets)
    }

    fn overview_content(&self) -> OverlayContent {
        let (rows, _) = self.overview_rows();
        OverlayContent {
            title: "desk  (letter: focus that window)".into(),
            exit_hint: "Esc  close".into(),
            rows,
            columns: Some(1),
            max_desc: Some(80),
        }
    }

    fn overlay_content(&self, level: &Level) -> OverlayContent {
        let leader = self.leader_words();
        let (title, exit_hint) = match level {
            Level::Top => (leader.clone(), format!("release {leader}  close")),
            Level::Mode(m) => {
                let mut t = String::new();
                for c in self.keymap.mode_path(m) {
                    if !t.is_empty() {
                        t.push(' ');
                    }
                    chord_words(&c, &mut t);
                }
                if !t.is_empty() {
                    t.push_str("  ");
                }
                t.push_str(m);
                (t, "Esc  back".to_string())
            }
        };
        let rows = self
            .keymap
            .which_key(level)
            .into_iter()
            .map(|e| OverlayRow {
                key: e.key,
                desc: e.desc,
                prefix: e.prefix,
            })
            .collect();
        OverlayContent {
            title,
            exit_hint,
            rows,
            columns: None,
            max_desc: None,
        }
    }

    fn apply_overlay(&mut self, ch: OverlayChange) {
        let content = match ch {
            OverlayChange::Keep => return,
            OverlayChange::Hide => {
                self.overview = false;
                None
            }
            OverlayChange::Show(_) if self.overview => Some(self.overview_content()),
            OverlayChange::Show(level) => Some(self.overlay_content(&level)),
        };
        // The sheet goes where the user is looking: the monitor under the
        // pointer, else the focused window's monitor.
        let fallback;
        let under_mouse = self.monitor_under_mouse();
        let mon = match self
            .monitors
            .get(under_mouse.unwrap_or(self.focused_monitor))
        {
            Some(m) => &m.info,
            None => {
                fallback = self.focused_monitor();
                &fallback
            }
        };
        self.p.show_overlay(content.as_ref(), mon);
    }

    fn model_name(&self) -> &'static str {
        match self.model.kind() {
            ModelKind::PerMonitor => "per-monitor",
            ModelKind::Shared => "shared",
        }
    }

    fn ws_name(&self, ws: WorkspaceId) -> &str {
        self.model
            .name_of(ws)
            .or_else(|| self.workspaces.get(ws).map(|w| w.name.as_str()))
            .unwrap_or("")
    }

    fn ws_monitor(&self, ws: WorkspaceId) -> MonitorId {
        self.model
            .monitor_of(ws)
            .or_else(|| self.workspaces.get(ws).map(|w| w.monitor))
            .unwrap_or(self.focused_monitor)
    }

    fn mon_uuid(&self, m: MonitorId) -> String {
        self.monitors
            .get(m)
            .map(|m| m.info.uuid.clone())
            .unwrap_or_default()
    }

    fn visible_ws(&self, m: MonitorId) -> Option<WorkspaceId> {
        self.model.visible_on(m)
    }

    fn create_ws(&mut self, m: MonitorId, name: &str) -> Option<WorkspaceId> {
        let layout = self.cfg.layout.default;
        self.model.resolve(
            m,
            &tatami_protocol::WorkspaceTarget::Name(name.to_string()),
            false,
            &mut creator(&mut self.tree, &mut self.workspaces, layout),
        )
    }

    // ---- monitors ----------------------------------------------------------

    /// The monitor containing `r`'s centre, else one it touches, else focused.
    fn monitor_for_rect(&self, r: &Rect) -> MonitorId {
        let c = r.center();
        self.order
            .iter()
            .find(|&&m| self.monitors[m].info.frame.contains(c))
            .or_else(|| {
                self.order
                    .iter()
                    .find(|&&m| self.monitors[m].info.frame.intersects(r))
            })
            .copied()
            .unwrap_or(self.focused_monitor)
    }

    fn reorder_monitors(&mut self) {
        let mut order: Vec<MonitorId> = self.monitors.keys().collect();
        order.sort_by(|a, b| {
            let (fa, fb) = (&self.monitors[*a].info.frame, &self.monitors[*b].info.frame);
            fa.x.total_cmp(&fb.x).then(fa.y.total_cmp(&fb.y))
        });
        self.order = order;
        let frames: Vec<Rect> = self.monitors.values().map(|m| m.info.frame).collect();
        for m in self.monitors.values_mut() {
            m.corner = parking::parking_corner(&m.info.frame, &frames);
        }
        self.resolve_forced();
    }

    fn resolve_forced(&mut self) {
        let list: Vec<(MonitorId, &MonitorInfo)> = self
            .order
            .iter()
            .map(|&m| (m, &self.monitors[m].info))
            .collect();
        let mut forced = Vec::new();
        for (name, pats) in &self.cfg.workspaces.monitor_force_assignment {
            if let Some(m) = pats
                .iter()
                .find_map(|p| match_monitor_pattern(p, &list, &self.order))
            {
                forced.push((name.clone(), m));
            }
        }
        self.model.set_forced(forced);
    }

    /// Read `screens()`, match by uuid, add new monitors, drop vanished ones
    /// (their workspaces move to a remaining monitor, merging on clashes).
    fn sync_screens(&mut self) {
        let screens = self.p.screens();
        if screens.is_empty() {
            // Display sleep or a reconfiguration in flight; keep what we have.
            return;
        }
        let mut added = Vec::new();
        let mut seen = Vec::with_capacity(screens.len());
        for info in screens {
            match self
                .monitors
                .iter_mut()
                .find(|(_, m)| m.info.uuid == info.uuid)
            {
                Some((id, m)) => {
                    m.info = info;
                    seen.push(id);
                }
                None => {
                    let id = self.monitors.insert(Mon {
                        info,
                        corner: Corner::BottomRight,
                    });
                    seen.push(id);
                    added.push(id);
                }
            }
        }
        let removed: Vec<MonitorId> = self
            .monitors
            .keys()
            .filter(|id| !seen.contains(id))
            .collect();
        for id in removed.iter() {
            self.monitors.remove(*id);
        }
        self.reorder_monitors();
        let layout = self.cfg.layout.default;
        for &id in self.order.clone().iter().filter(|m| added.contains(m)) {
            self.model.add_monitor(
                id,
                &mut creator(&mut self.tree, &mut self.workspaces, layout),
            );
        }
        let fallback = self.order[0];
        for id in removed {
            for rh in self.model.remove_monitor(id, fallback) {
                match rh.merge_into {
                    Some(into) => self.merge_ws(rh.ws, into),
                    None => {
                        if let Some(w) = self.workspaces.get_mut(rh.ws) {
                            w.monitor = fallback;
                        }
                    }
                }
            }
        }
        if !self.monitors.contains_key(self.focused_monitor) {
            self.focused_monitor = self
                .order
                .iter()
                .copied()
                .find(|&m| self.monitors[m].info.is_main)
                .unwrap_or(fallback);
        }
    }

    fn on_screens_changed(&mut self) {
        self.sync_screens();
        if let Some(id) = self.focused {
            self.focused_monitor = self.ws_monitor(self.wins[id].ws);
        }
        self.relayout_all();
        let monitors = self.order.iter().map(|&m| self.mon_uuid(m)).collect();
        self.emit(IpcEvent::MonitorsChanged { monitors });
        self.gc();
        self.status_dirty = true;
        self.dirty = true;
    }

    /// Move every window of `from` into `into` and drop `from`.
    fn merge_ws(&mut self, from: WorkspaceId, into: WorkspaceId) {
        let (Some(f), Some(i)) = (self.workspaces.get(from), self.workspaces.get(into)) else {
            return;
        };
        let (froot, iroot) = (f.root, i.root);
        for m in self.wins.values_mut() {
            if m.ws == from {
                m.ws = into;
            }
        }
        if let Some(fs) = f.fullscreen {
            self.wins[fs].w.kind = WindowKind::Tiled;
        }
        self.tree.move_children(froot, iroot);
        let floating = std::mem::take(&mut self.workspaces[from].floating);
        self.workspaces[into].floating.extend(floating);
        self.tree.remove_root(froot);
        self.workspaces.remove(from);
        if let Some(s) = self.restore.as_mut() {
            s.retarget(from, into);
        }
    }

    // ---- apps and adoption -------------------------------------------------

    fn observe(&mut self, info: AppInfo) {
        let pid = info.pid;
        let ok = self.p.observe_app(pid).is_ok();
        let app = self.apps.entry(pid).or_insert(App {
            info,
            observed: false,
        });
        app.observed |= ok;
    }

    /// Observe every running app and adopt windows we don't know yet, left to
    /// right so initial tiles keep their on-screen order.
    /// Control is where the pointer is. Before a key chord or command runs,
    /// the monitor under the mouse becomes the focused monitor, and if the
    /// focused window is elsewhere, that monitor's own focused (or first)
    /// window takes over without being raised. macOS's own idea of focus is
    /// unreliable here (stale app records, floating utility windows), and
    /// this is also what the user expects on a four-monitor desk.
    fn follow_mouse(&mut self) {
        let Some(m) = self.monitor_under_mouse() else {
            return;
        };
        let on_it = self
            .focused
            .and_then(|f| self.wins.get(f))
            .is_some_and(|w| self.ws_monitor(w.ws) == m);
        if on_it {
            return;
        }
        if self.focused_monitor != m {
            self.focused_monitor = m;
            self.status_dirty = true;
        }
        let pick = self.model.visible_on(m).and_then(|ws| {
            self.workspaces[ws]
                .focused
                .and_then(|l| self.tree.window_of(l))
                .or_else(|| self.ws_windows(ws).into_iter().next())
        });
        match pick {
            Some(id) => {
                self.note_focused(id);
            }
            None => {
                self.focused = None;
                self.broadcast_focus();
            }
        }
    }

    /// A hand-dragged tile has come to rest. Dropped on another monitor it
    /// moves there; dropped over a sibling tile the two swap; otherwise it
    /// snaps back. (Resizing by hand also ends here and snaps back.)
    fn settle_drag(&mut self) {
        let Some((id, at)) = self.drag else {
            return;
        };
        if at > self.now {
            return;
        }
        self.drag = None;
        let Some(m) = self.wins.get(id) else {
            return;
        };
        if !m.w.kind.in_tree() {
            return;
        }
        let (ws, c) = (m.ws, m.w.frame.center());
        let home = self.ws_monitor(ws);
        let there = self
            .monitors
            .iter()
            .find(|(_, mon)| mon.info.frame.contains(c))
            .map(|(i, _)| i);
        if let Some(tm) = there
            && tm != home
            && let Some(to) = self.visible_ws(tm)
        {
            if let Err(e) = self.move_node(id, to, true) {
                tracing::debug!("drag to monitor: {e}");
                self.relayout(ws);
            }
            return;
        }
        let other = self.ws_windows(ws).into_iter().find(|&o| {
            o != id && {
                let w = &self.wins[o];
                w.leaf.is_some() && w.w.expected.unwrap_or(w.w.frame).contains(c)
            }
        });
        if let Some(o) = other
            && let (Some(a), Some(b)) = (self.wins[id].leaf, self.wins[o].leaf)
        {
            self.tree.swap_leaves(a, b);
        }
        self.relayout(ws);
    }

    /// The monitor containing the pointer.
    fn monitor_under_mouse(&self) -> Option<MonitorId> {
        let p = self.p.mouse_location()?;
        self.monitors
            .iter()
            .find(|(_, m)| m.info.frame.contains(p))
            .map(|(id, _)| id)
    }

    /// Observe and adopt only apps we have not seen; cheap enough to run
    /// every few seconds (one window-server query, AX only for new pids).
    fn rescan_new_apps(&mut self) {
        let new: Vec<AppInfo> = self
            .p
            .running_apps()
            .into_iter()
            .filter(|a| !self.apps.get(&a.pid).is_some_and(|x| x.observed))
            .collect();
        for app in new {
            self.on_app_launched(app);
        }
    }

    fn rescan(&mut self) {
        let mut infos: Vec<WindowInfo> = Vec::new();
        for app in self.p.running_apps() {
            let pid = app.pid;
            if !self.apps.get(&pid).is_some_and(|a| a.observed) {
                self.observe(app);
            }
            infos.extend(
                self.p
                    .app_windows(pid)
                    .into_iter()
                    .filter(|w| !self.by_handle.contains_key(&w.handle)),
            );
        }
        infos.sort_by(|a, b| {
            let (ma, mb) = (
                self.monitor_for_rect(&a.frame),
                self.monitor_for_rect(&b.frame),
            );
            let ia = self.order.iter().position(|&m| m == ma);
            let ib = self.order.iter().position(|&m| m == mb);
            ia.cmp(&ib)
                .then(a.frame.x.total_cmp(&b.frame.x))
                .then(a.frame.y.total_cmp(&b.frame.y))
                .then(a.handle.cmp(&b.handle))
        });
        for info in infos {
            self.adopt(info, false);
        }
    }

    fn on_app_launched(&mut self, info: AppInfo) {
        let pid = info.pid;
        self.observe(info);
        for w in self.p.app_windows(pid) {
            if !self.by_handle.contains_key(&w.handle) {
                self.adopt(w, true);
            }
        }
    }

    fn on_app_terminated(&mut self, pid: i32) {
        let gone: Vec<WindowId> = self
            .wins
            .iter()
            .filter(|(_, m)| m.w.pid == pid)
            .map(|(id, _)| id)
            .collect();
        for id in gone {
            self.forget_window(id);
        }
        if self.apps.remove(&pid).is_some_and(|a| a.observed) {
            self.p.unobserve_app(pid);
        }
    }

    fn app_info(&mut self, pid: i32) -> AppInfo {
        if let Some(a) = self.apps.get(&pid) {
            return a.info.clone();
        }
        let info = self
            .p
            .running_apps()
            .into_iter()
            .find(|a| a.pid == pid)
            .unwrap_or(AppInfo {
                pid,
                bundle_id: None,
                name: String::new(),
                exe: None,
            });
        self.apps.insert(
            pid,
            App {
                info: info.clone(),
                observed: true,
            },
        );
        info
    }

    /// Decide whether and how to manage a window: `(kind, rule workspace)`.
    fn classify(&self, info: &WindowInfo, app: &AppInfo) -> Option<(WindowKind, Option<String>)> {
        if info.role != "AXWindow" {
            return None;
        }
        let rule = self.cfg.rules_in_effect().find(|r| {
            r.app_id
                .as_ref()
                .is_none_or(|id| app.bundle_id.as_deref() == Some(id.as_str()))
                && r.app_name_regex
                    .as_ref()
                    .is_none_or(|p| text_match(p, &app.name))
                && r.title_regex
                    .as_ref()
                    .is_none_or(|p| text_match(p, &info.title))
        });
        if rule.is_some_and(|r| r.ignore) {
            return None;
        }
        let standard = info.subrole == "AXStandardWindow";
        let dialog = info.subrole == "AXDialog" && rule.is_some();
        if !standard && !dialog {
            return None;
        }
        let kind = if info.fullscreen {
            WindowKind::NativeFullscreen
        } else if rule.is_some_and(|r| r.float) || !info.resizable {
            // A fixed-size window can never fill a tile; it floats where
            // its app put it.
            WindowKind::Floating
        } else {
            WindowKind::Tiled
        };
        Some((kind, rule.and_then(|r| r.workspace.clone())))
    }

    /// Start managing a window. `fresh`: it just appeared (inserted after the
    /// focused leaf, focused, laid out now); otherwise it is appended and the
    /// caller lays out.
    fn adopt(&mut self, info: WindowInfo, fresh: bool) {
        let app = self.app_info(info.pid);
        let Some((kind, rule_ws)) = self.classify(&info, &app) else {
            return;
        };
        let m = self.monitor_for_rect(&info.frame);
        let ws = match rule_ws.and_then(|n| self.create_ws(m, &n)) {
            Some(ws) => ws,
            None => match self
                .visible_ws(m)
                .or_else(|| self.visible_ws(self.focused_monitor))
            {
                Some(ws) => ws,
                None => return,
            },
        };
        let handle = info.handle;
        let w = Window::new(&info, &app, kind);
        let id = self.wins.insert(Managed {
            w,
            ws,
            leaf: None,
            float_frame: None,
        });
        self.by_handle.insert(handle, id);
        self.dirty = true;
        if info.minimized || kind == WindowKind::NativeFullscreen {
            return;
        }
        if fresh && self.try_restore_slot(id) {
            return;
        }
        self.attach(id, ws, fresh);
        // A floating window parked by a daemon that was killed is still off
        // in a corner; tiled strays come back through the layout, floating
        // ones need pulling in by hand.
        if kind == WindowKind::Floating && self.model.is_visible(ws) {
            let mons: Vec<Rect> = self.monitors.values().map(|m| m.info.frame).collect();
            let f = self.wins[id].w.frame;
            if parking::is_parked(&f, &mons) {
                let area = self
                    .monitors
                    .get(self.ws_monitor(ws))
                    .map(|m| m.info.visible)
                    .unwrap_or(f);
                self.request_frame(id, centered(f, &area));
            }
        }
        let focus_now = self.pending_focus.take_if(|h| *h == handle).is_some()
            || (fresh && self.visible_ws(self.focused_monitor) == Some(ws));
        if fresh {
            if focus_now {
                self.note_focused(id);
            }
            self.relayout(ws);
        }
    }

    /// Put a window into `ws`: tiled after the focused leaf (or appended),
    /// floating onto the list. Does not lay out.
    fn attach(&mut self, id: WindowId, ws: WorkspaceId, after_focused: bool) {
        let Some(w) = self.workspaces.get_mut(ws) else {
            return;
        };
        let m = &mut self.wins[id];
        m.ws = ws;
        if m.w.kind == WindowKind::Fullscreen {
            m.w.kind = WindowKind::Tiled;
        }
        match m.w.kind {
            WindowKind::Tiled => {
                let after = if after_focused { w.focused } else { None };
                let leaf = self.tree.insert_window(w.root, after, id);
                m.leaf = Some(leaf);
                if after_focused {
                    w.prev_focused = w.focused;
                    w.focused = Some(leaf);
                    self.tree.note_focus(leaf);
                } else if w.focused.is_none() {
                    w.focused = Some(leaf);
                }
            }
            WindowKind::Floating if !w.floating.contains(&id) => w.floating.push(id),
            _ => {}
        }
        if !self.model.is_visible(ws) {
            self.park(id);
        }
    }

    /// Take a window out of its workspace's tree / floating list. Fixes the
    /// workspace's focus pointer; returns the window that should take over
    /// focus inside that workspace.
    fn detach(&mut self, id: WindowId) -> Option<WindowId> {
        let m = &mut self.wins[id];
        let ws = m.ws;
        let leaf = m.leaf.take();
        let w = self.workspaces.get_mut(ws)?;
        if w.fullscreen == Some(id) {
            w.fullscreen = None;
            m.w.kind = WindowKind::Tiled;
        }
        let mut next = None;
        if let Some(leaf) = leaf {
            let nl = self
                .tree
                .dfs_next(leaf, true, false)
                .or_else(|| self.tree.dfs_next(leaf, false, false));
            next = nl.and_then(|l| self.tree.window_of(l));
            let root = self.tree.root_of(leaf);
            self.tree.remove_leaf(leaf);
            self.tree.normalize(root);
            if w.focused == Some(leaf) {
                w.focused = nl.filter(|&l| self.tree.window_of(l).is_some());
            }
            if w.prev_focused == Some(leaf) {
                w.prev_focused = None;
            }
        }
        w.floating.retain(|&f| f != id);
        next.or_else(|| w.floating.first().copied())
    }

    fn forget_window(&mut self, id: WindowId) {
        let ws = self.wins[id].ws;
        let next = self.detach(id);
        let m = self.wins.remove(id).expect("live window");
        self.by_handle.remove(&m.w.handle);
        self.txs.retain(|t| t.win != id);
        if self.prev_focused == Some(id) {
            self.prev_focused = None;
        }
        if self.focused == Some(id) {
            self.focused = None;
            // macOS usually announces the successor first; if it hasn't,
            // hand focus to the tree neighbour.
            match next {
                Some(n) if self.model.is_visible(ws) => self.focus_window(n),
                _ => self.broadcast_focus(),
            }
        }
        self.relayout(ws);
        self.dirty = true;
    }

    fn on_minimized(&mut self, h: WindowHandle, on: bool) {
        let Some(id) = self.by_handle.get(&h).copied() else {
            return;
        };
        if self.wins[id].w.minimized == on {
            return;
        }
        self.wins[id].w.minimized = on;
        let ws = self.wins[id].ws;
        if on {
            self.detach(id);
            self.relayout(ws);
        } else {
            let ws = if self.workspaces.contains_key(ws) {
                ws
            } else {
                match self.visible_ws(self.focused_monitor) {
                    Some(w) => w,
                    None => return,
                }
            };
            self.attach(id, ws, true);
            self.relayout(ws);
        }
        self.dirty = true;
    }

    // ---- focus ---------------------------------------------------------------

    fn broadcast_focus(&mut self) {
        let (window, app, title) = match self.focused.and_then(|id| self.wins.get(id)) {
            Some(m) => (
                Some(m.w.handle),
                Some(m.w.app_name.clone()),
                Some(m.w.title.clone()),
            ),
            None => (None, None, None),
        };
        let monitor = self.mon_uuid(self.focused_monitor);
        self.emit(IpcEvent::FocusChanged {
            window,
            app,
            title,
            monitor,
        });
        self.status_dirty = true;
    }

    /// Record `id` as focused (state only). Returns whether its workspace
    /// needs a relayout because an accordion's raised child changed.
    fn note_focused(&mut self, id: WindowId) -> bool {
        let m = &self.wins[id];
        let (ws, leaf) = (m.ws, m.leaf);
        let mut relayout = false;
        if let (Some(leaf), Some(w)) = (leaf, self.workspaces.get_mut(ws))
            && w.focused != Some(leaf)
        {
            w.prev_focused = w.focused;
            w.focused = Some(leaf);
            self.tree.note_focus(leaf);
            relayout = self.has_accordion_above(leaf);
        }
        let mon = self.ws_monitor(ws);
        if self.focused != Some(id) || self.focused_monitor != mon {
            if self.focused != Some(id) {
                self.prev_focused = self.focused;
            }
            self.focused = Some(id);
            self.focused_monitor = mon;
            self.broadcast_focus();
        }
        relayout
    }

    fn has_accordion_above(&self, leaf: NodeId) -> bool {
        let mut cur = self.tree.parent(leaf);
        while let Some(p) = cur {
            if self.tree.layout_of(p).is_accordion() {
                return true;
            }
            cur = self.tree.parent(p);
        }
        false
    }

    /// Focus a window: show its workspace, record, raise.
    /// Something for the direction keys to start from when macOS has not
    /// told us what is focused: the first window of the focused monitor's
    /// visible workspace, else any window at all.
    pub(crate) fn default_focus(&self) -> Option<WindowId> {
        let ws = self.model.visible_on(self.focused_monitor)?;
        self.ws_windows(ws)
            .into_iter()
            .next()
            .or_else(|| self.wins.keys().next())
    }

    fn focus_window(&mut self, id: WindowId) {
        let ws = self.wins[id].ws;
        if !self.model.is_visible(ws) {
            let sw = self.model.switch(self.focused_monitor, ws);
            self.apply_switch(sw, false);
        }
        let relayout = self.note_focused(id);
        let _ = self.p.raise(self.wins[id].w.handle);
        if relayout {
            self.relayout(ws);
        }
        let (m, frame) = (
            self.ws_monitor(ws),
            self.wins[id].w.expected.unwrap_or(self.wins[id].w.frame),
        );
        self.pointer_to(m, Some(frame));
    }

    /// Bring the pointer to monitor `m` (into `around`, else the monitor's
    /// middle) when it is elsewhere, so that "act where the pointer is"
    /// follows keyboard focus across monitors.
    fn pointer_to(&mut self, m: MonitorId, around: Option<Rect>) {
        if !self.cfg.general.mouse_follows_focus || self.monitor_under_mouse() == Some(m) {
            return;
        }
        let Some(mon) = self.monitors.get(m) else {
            return;
        };
        let c = around.unwrap_or(mon.info.visible).center();
        self.p.warp_mouse(c);
    }

    /// Focus what `ws` last had focused, or clear focus if it is empty.
    fn focus_ws(&mut self, ws: WorkspaceId) {
        let Some(w) = self.workspaces.get(ws) else {
            return;
        };
        let target = w
            .focused
            .and_then(|l| self.tree.window_of(l))
            .or_else(|| {
                self.tree
                    .leaves(w.root)
                    .into_iter()
                    .find_map(|l| self.tree.window_of(l))
            })
            .or_else(|| w.floating.first().copied());
        match target {
            Some(id) => self.focus_window(id),
            None => {
                self.focused_monitor = self.ws_monitor(ws);
                if self.focused.take().is_some() {
                    self.prev_focused = None;
                }
                self.broadcast_focus();
            }
        }
    }

    fn on_window_focused(&mut self, h: WindowHandle) {
        if let Some(i) = self.raise_echo.iter().position(|&x| x == h) {
            self.raise_echo.swap_remove(i);
            return;
        }
        let Some(id) = self.by_handle.get(&h).copied() else {
            self.pending_focus = Some(h);
            return;
        };
        let ws = self.wins[id].ws;
        if !self.wins[id].w.kind.in_tree() && self.wins[id].w.kind != WindowKind::Floating {
            return;
        }
        if self.enabled && !self.model.is_visible(ws) {
            // Cmd-tab or a click in the Dock reached a parked window.
            let sw = self.model.switch(self.focused_monitor, ws);
            self.apply_switch(sw, false);
        }
        if self.note_focused(id) {
            self.relayout(ws);
        }
        // Cmd-tab onto another monitor: bring the pointer, so the next chord
        // acts where focus went. A click is already under the pointer.
        let (m, frame) = (
            self.ws_monitor(ws),
            self.wins[id].w.expected.unwrap_or(self.wins[id].w.frame),
        );
        self.pointer_to(m, Some(frame));
    }

    // ---- workspaces ------------------------------------------------------------

    /// Act on a `Switch`: merge, park the hidden, lay out the shown, focus
    /// the target (when `focus`), broadcast, gc.
    fn apply_switch(&mut self, sw: Switch, focus: bool) {
        if sw.noop {
            return;
        }
        let prev = sw.hide.first().map(|&w| self.ws_name(w).to_string());
        if let Some(into) = sw.merge_into {
            self.merge_ws(sw.target, into);
            // `into` may already be visible, so not in `show`; it still
            // gained windows.
            if !sw.show.contains(&into) {
                self.relayout(into);
            }
        }
        for &h in &sw.hide {
            self.relayout(h);
        }
        for &s in &sw.show {
            if let Some(w) = self.workspaces.get_mut(s) {
                w.monitor = self.model.monitor_of(s).unwrap_or(w.monitor);
            }
            self.relayout(s);
        }
        self.focused_monitor = sw.focus_monitor;
        let target = sw.merge_into.unwrap_or(sw.target);
        if focus {
            self.focus_ws(target);
        }
        let workspace = self
            .visible_ws(sw.focus_monitor)
            .map(|w| self.ws_name(w).to_string())
            .unwrap_or_default();
        let monitor = self.mon_uuid(sw.focus_monitor);
        self.emit(IpcEvent::WorkspaceChanged {
            monitor,
            workspace,
            prev,
        });
        self.status_dirty = true;
        self.dirty = true;
        self.gc();
    }

    fn gc(&mut self) {
        let (tree, wss, wins) = (&self.tree, &self.workspaces, &self.wins);
        let dead = self.model.gc(
            |id| {
                wss.get(id).is_none_or(|w| {
                    tree.children(w.root).is_empty()
                        && w.floating.is_empty()
                        && !wins.values().any(|m| m.ws == id)
                })
            },
            &self.cfg.workspaces.persistent,
        );
        for d in dead {
            if let Some(w) = self.workspaces.remove(d) {
                self.tree.remove_root(w.root);
            }
        }
    }

    // ---- layout and frame transactions -----------------------------------------

    fn relayout_all(&mut self) {
        let all: Vec<WorkspaceId> = self.workspaces.keys().collect();
        for ws in all {
            self.relayout(ws);
        }
    }

    /// Lay out a visible workspace, or park a hidden one.
    fn relayout(&mut self, ws: WorkspaceId) {
        if !self.enabled {
            return;
        }
        let Some(w) = self.workspaces.get(ws) else {
            return;
        };
        let Some(mon) = self.monitors.get(self.ws_monitor(ws)) else {
            return;
        };
        if !self.model.is_visible(ws) {
            self.park_ws(ws);
            return;
        }
        let (root, focus_leaf, fs) = (w.root, w.focused, w.fullscreen);
        let area = mon.info.visible;
        let mut out = std::mem::take(&mut self.scratch);
        out.clear();
        // A window that has refused a size gets at least that size from the
        // layout itself; weights are never touched for it.
        let wins = &self.wins;
        let min = |w: WindowId| wins.get(w).and_then(|m| m.w.stubborn);
        let params = layout::Params {
            area,
            gaps: &self.gaps,
            focused: focus_leaf,
            accordion_padding: self.pad,
            min: &min,
        };
        layout::compute(&self.tree, root, &params, &mut out);
        let mut changed = false;
        let mut overlapping = false;
        for p in &out {
            let rect = if Some(p.window) == fs {
                layout::fullscreen_rect(area, &self.gaps, false)
            } else {
                p.rect
            };
            overlapping |= p.z > 0;
            changed |= self.request_frame(p.window, rect);
        }
        let mut i = 0;
        while let Some(&id) = self.workspaces[ws].floating.get(i) {
            if let Some(f) = self.wins[id].float_frame.take() {
                changed |= self.request_frame(
                    id,
                    fit_into(f, &mon_frame(&self.monitors, self.ws_monitor(ws))),
                );
            }
            i += 1;
        }
        let focused_here = self
            .focused
            .filter(|&f| self.wins.get(f).is_some_and(|m| m.ws == ws));
        if changed && let Some(f) = focused_here {
            if overlapping {
                // Accordion z-order: back to front, the focused one last.
                out.sort_by_key(|p| std::cmp::Reverse(p.z));
                for p in out.iter().filter(|p| p.z > 0 && p.window != f) {
                    let h = self.wins[p.window].w.handle;
                    if self.p.raise(h).is_ok() {
                        self.raise_echo.push(h);
                    }
                }
            }
            if overlapping || fs.is_some() {
                let _ = self.p.raise(self.wins[fs.unwrap_or(f)].w.handle);
            }
        }
        self.scratch = out;
    }

    /// Ask for `rect` unless the window is (or is already asked to be) there.
    /// Starts a verify transaction except for known-stubborn windows.
    fn request_frame(&mut self, id: WindowId, rect: Rect) -> bool {
        let m = &mut self.wins[id];
        let cur = m.w.expected.unwrap_or(m.w.frame);
        if satisfied(&m.w, &cur, &rect, SKIP_EPS) {
            return false;
        }
        m.w.expected = Some(rect);
        let stubborn = m.w.stubborn.is_some();
        let t0 = Instant::now();
        // Position first, then size, then position again: an app that clamps
        // the window to the screen (Chrome) would otherwise refuse a grow from
        // the old position, and the retry 60 ms later shows as a two-step.
        if let Err(e) = self
            .p
            .set_window_frame(m.w.handle, rect, SetOrder::PosSizePos)
        {
            // The app refused the write outright (not merely ignored it):
            // this window cannot be tiled, so it floats from now on.
            tracing::info!(app = %m.w.app_name, title = %m.w.title, "cannot be tiled ({e}); floating it");
            m.w.expected = None;
            self.unmanageable.push(id);
            return false;
        }
        tracing::trace!(
            handle = m.w.handle.0,
            ?rect,
            took_us = t0.elapsed().as_micros() as u64,
            "set frame"
        );
        if !stubborn {
            let tx = FrameTx {
                win: id,
                target: rect,
                attempt: 1,
                deadline: self.now + VERIFY_AFTER[0],
            };
            match self.txs.iter_mut().find(|t| t.win == id) {
                Some(t) => *t = tx,
                None => self.txs.push(tx),
            }
        }
        true
    }

    /// Verify due transactions: done, retry with `PosSizePos`, or give up and
    /// remember the size the app insists on.
    fn verify_frames(&mut self) {
        let now = self.now;
        let mut regrow: Vec<WorkspaceId> = Vec::new();
        let mut i = 0;
        while i < self.txs.len() {
            let tx = self.txs[i];
            if tx.deadline > now {
                i += 1;
                continue;
            }
            let Some(m) = self.wins.get_mut(tx.win) else {
                self.txs.swap_remove(i);
                continue;
            };
            let Some(actual) = self.p.window_frame(m.w.handle) else {
                self.txs.swap_remove(i);
                continue;
            };
            // An app at its minimum size answers two writes with the same
            // size and the right position; a third try would only add a
            // visible stutter, so it is stubborn now.
            let clamped = tx.attempt >= 2
                && (actual.x - tx.target.x).abs() <= MATCH_EPS
                && (actual.y - tx.target.y).abs() <= MATCH_EPS
                && (actual.w - m.w.frame.w).abs() <= MATCH_EPS
                && (actual.h - m.w.frame.h).abs() <= MATCH_EPS;
            if actual.approx_eq(&tx.target, MATCH_EPS) {
                m.w.frame = actual;
                m.w.expected = None;
                self.txs.swap_remove(i);
            } else if usize::from(tx.attempt) < VERIFY_AFTER.len() && !clamped {
                let _ = self
                    .p
                    .set_window_frame(m.w.handle, tx.target, SetOrder::PosSizePos);
                let t = &mut self.txs[i];
                t.deadline = now + VERIFY_AFTER[usize::from(t.attempt)];
                t.attempt += 1;
                i += 1;
            } else {
                tracing::warn!(
                    app = %m.w.app_name,
                    title = %m.w.title,
                    wanted = ?tx.target,
                    got = ?actual,
                    "window refuses its frame; keeping its size"
                );
                // Only the refused dimension is a minimum; the other one was
                // accepted and must stay free to shrink (a column of two).
                let refused = |got: f64, want: f64| {
                    if (got - want).abs() > MATCH_EPS {
                        got
                    } else {
                        0.0
                    }
                };
                m.w.stubborn = Some(tatami_protocol::Size {
                    w: refused(actual.w, tx.target.w),
                    h: refused(actual.h, tx.target.h),
                });
                m.w.frame = actual;
                m.w.expected = None;
                if !regrow.contains(&m.ws) {
                    regrow.push(m.ws);
                }
                self.txs.swap_remove(i);
            }
        }
        for ws in regrow {
            self.relayout(ws);
        }
    }

    fn on_window_frame(&mut self, h: WindowHandle, r: Rect) {
        let Some(id) = self.by_handle.get(&h).copied() else {
            return;
        };
        let m = &mut self.wins[id];
        if let Some(e) = m.w.expected
            && satisfied(&m.w, &r, &e, MATCH_EPS)
        {
            m.w.frame = r;
            m.w.expected = None;
            self.txs.retain(|t| t.win != id);
            return;
        }
        let in_flight = self.txs.iter().any(|t| t.win == id);
        let unchanged = m.w.frame.approx_eq(&r, MATCH_EPS);
        m.w.frame = r;
        if in_flight || !self.enabled {
            return;
        }
        if unchanged && m.w.expected.is_none() {
            return;
        }
        m.w.expected = None;
        let (ws, kind) = (m.ws, m.w.kind);
        match kind {
            WindowKind::NativeFullscreen => {
                self.check_native_fullscreen(id);
            }
            WindowKind::Tiled | WindowKind::Fullscreen => {
                let full = self
                    .monitors
                    .get(self.ws_monitor(ws))
                    .is_some_and(|mon| mon.info.frame.approx_eq(&r, MATCH_EPS));
                if full && self.check_native_fullscreen(id) {
                    return;
                }
                // The user is dragging or resizing a tiled window. Decide
                // once it settles (see `settle_drag`), not on every move
                // event, or the window fights the pointer.
                self.drag = Some((id, self.now + DRAG_SETTLE));
            }
            WindowKind::Floating => {
                if !self.model.is_visible(ws) {
                    self.relayout(ws);
                }
            }
            WindowKind::Unmanaged => {}
        }
    }

    /// Re-read the window's native-fullscreen flag (only when its frame hints
    /// at a change) and move it in or out of the tree. Returns whether it is
    /// now in native fullscreen.
    fn check_native_fullscreen(&mut self, id: WindowId) -> bool {
        let (h, pid) = (self.wins[id].w.handle, self.wins[id].w.pid);
        let Some(info) = self.p.app_windows(pid).into_iter().find(|w| w.handle == h) else {
            return false;
        };
        let ws = self.wins[id].ws;
        let was = self.wins[id].w.kind == WindowKind::NativeFullscreen;
        if info.fullscreen && !was {
            self.detach(id);
            self.wins[id].w.kind = WindowKind::NativeFullscreen;
            self.relayout(ws);
            self.dirty = true;
        } else if !info.fullscreen && was {
            self.wins[id].w.kind = WindowKind::Tiled;
            self.attach(id, ws, true);
            self.relayout(ws);
            self.dirty = true;
        }
        info.fullscreen
    }

    // ---- parking -------------------------------------------------------------

    fn park(&mut self, id: WindowId) {
        let m = &mut self.wins[id];
        if m.w.minimized || !(m.w.kind.in_tree() || m.w.kind == WindowKind::Floating) {
            return;
        }
        let Some(mon) = self
            .monitors
            .get(self.model.monitor_of(m.ws).unwrap_or(self.focused_monitor))
        else {
            return;
        };
        if m.w.kind == WindowKind::Floating && m.float_frame.is_none() {
            m.float_frame = Some(m.w.expected.unwrap_or(m.w.frame));
        }
        let size = m
            .float_frame
            .unwrap_or(m.w.expected.unwrap_or(m.w.frame))
            .size();
        let rect = parking::parking_rect(&mon.info.frame, mon.corner, size);
        self.request_frame(id, rect);
    }

    fn park_ws(&mut self, ws: WorkspaceId) {
        let mut ids = std::mem::take(&mut self.scratch_ids);
        ids.clear();
        let w = &self.workspaces[ws];
        self.tree.for_each_leaf(w.root, &mut |l| {
            if let Some(id) = self.tree.window_of(l) {
                ids.push(id);
            }
        });
        ids.extend_from_slice(&w.floating);
        for &id in &ids {
            self.park(id);
        }
        self.scratch_ids = ids;
    }

    /// Bring every parked window back on screen (disable, quit).
    fn unpark_all(&mut self) {
        let mons: Vec<Rect> = self.monitors.values().map(|m| m.info.frame).collect();
        let ids: Vec<WindowId> = self.wins.keys().collect();
        for id in ids {
            let ws = self.wins[id].ws;
            let hidden = !self.model.is_visible(ws);
            let m = &mut self.wins[id];
            if m.w.minimized || !(m.w.kind.in_tree() || m.w.kind == WindowKind::Floating) {
                continue;
            }
            let cur = m.w.expected.unwrap_or(m.w.frame);
            if !hidden && !parking::is_parked(&cur, &mons) {
                continue;
            }
            let area = self
                .monitors
                .get(self.model.monitor_of(ws).unwrap_or(self.focused_monitor))
                .map(|mon| mon.info.visible)
                .unwrap_or(cur);
            let rect = match m.float_frame.take() {
                Some(f) => fit_into(f, &area),
                None => centered(cur, &area),
            };
            let _ = self.p.set_window_frame(m.w.handle, rect, SetOrder::SizePos);
            m.w.frame = rect;
            m.w.expected = None;
        }
        self.txs.clear();
    }
}

fn mon_frame(mons: &SlotMap<MonitorId, Mon>, m: MonitorId) -> Rect {
    mons.get(m).map(|m| m.info.frame).unwrap_or_default()
}
