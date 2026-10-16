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
use tatami_protocol::{Action, IpcEvent, Layout, MonitorTarget, Rect, Response, WindowHandle};
use tatami_state::SavedState;

use crate::ids::{MonitorId, NodeId, WindowId, WorkspaceId};
use crate::layout::{self, Gaps, Insets, Placement};
use crate::parking::{self, Corner};
use crate::platform::{
    AppInfo, KeyDisposition, MenuItem, Mods, MonitorInfo, OverlayContent, OverlayRow, Platform,
    PlatformEvent, RawKeyEvent, SetOrder, WindowInfo, distinct_screens,
};
use crate::tree::Tree;
use crate::window::{Window, WindowKind};
use crate::workspace::{ModelKind, Switch, Workspace, WorkspaceModel, match_monitor_pattern};

use restore::RestoreSession;

/// What the daemon must do after a reactor step.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// `profile …` from a key or the menu; the daemon owns the files.
    Profile(tatami_protocol::ProfileCmd),

    /// Layout state changed; persist it (the daemon debounces).
    Dirty,
    Broadcast(IpcEvent),
    /// `exec-and-forget`: run through `/bin/sh -c`.
    Exec(String),
    Exit,
    ReloadConfig,
    /// `hot-swap`: hand over to the `tatamid` on disk (a new process).
    HotSwap,
    /// `edit-config`: the daemon writes the template if needed and opens
    /// the file with this command (`general.editor`; empty = TextEdit).
    EditConfig(String),
    /// `start-at-login on|off`: the daemon installs or removes the launchd
    /// agent and reports back with `set_start_at_login`.
    StartAtLogin(bool),
    /// `desk …` from a key or the menu; the daemon owns the files.
    Desk(tatami_protocol::DeskCmd),
    /// The set of monitors changed: `saved` is the layout the desk `from`
    /// had the moment before, for the daemon to file; `to` is the desk
    /// now present, whose saved layout the daemon restores if it has one.
    DeskChanged {
        from: String,
        saved: Box<SavedState>,
        to: String,
    },
}

/// Frame-transaction verify delays after attempt 1, 2 and 3.
/// How long a failed key's message stays in the menu bar.
/// Apps that appear without an `AppLaunched` (not registered with
/// LaunchServices) are found by scanning the window server this often.
const APP_SCAN_EVERY: Duration = Duration::from_secs(3);
/// A hand-dragged tile is acted on this long after its last move event.
const DRAG_SETTLE: Duration = Duration::from_millis(200);
const FLASH_FOR: Duration = Duration::from_millis(2500);
/// After our raise, a focus event for a parked sibling window within this
/// long is the app activating, not the user reaching for it.
const RAISE_ECHO_FOR: Duration = Duration::from_secs(1);
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
    /// Our last raise: the app's pid, the window, and when. Activating an
    /// app for one window makes macOS report the app's previous key window
    /// first, and when that one is parked on a hidden workspace, taking
    /// the event as the user's wish would switch workspaces behind their
    /// back (`on_window_focused`).
    last_raise: Option<(i32, WindowId, Instant)>,

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
    /// The overview's lines the highlight can rest on, in display order:
    /// every monitor header and the windows of the expanded monitors.
    overview_items: Vec<OverviewItem>,
    /// A window picked on the overview, focused on the next `drain`.
    overview_pick: Option<WindowId>,
    /// Search text on the overview (`/` starts it); `None` = letters pick.
    overview_query: Option<String>,
    /// The highlighted item (index into `overview_items`).
    overview_sel: usize,
    /// Monitors the overview shows expanded (workspaces and windows).
    overview_open: Vec<MonitorId>,
    /// A line of text being asked for on the sheet (a profile's name).
    prompt: Option<Prompt>,
    /// A tiled window the user is moving by hand, and when to act on it.
    drag: Option<(WindowId, Instant)>,
    /// Windows whose app refused a frame write outright; floated at the end of the step.
    unmanageable: Vec<WindowId>,
    /// Next window-server scan for apps NSWorkspace never told us about.
    next_app_scan: Instant,

    enabled: bool,
    trusted: bool,
    /// The launchd agent is installed (the daemon tells us; the menu shows it).
    start_at_login: bool,
    /// Saved desks, for the menu (the daemon reads the files).
    desks: Vec<tatami_state::DeskInfo>,
    /// Named profiles, for the menu (the daemon reads the files).
    profiles: Vec<String>,
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
/// A question on the sheet: `text` is typed with the keys the tap
/// captures, Enter runs `command` with the text as its last word, Esc
/// drops it. No dialog: a modal would hold the daemon's main thread, and
/// an activating window would steal focus from the app you are in.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Prompt {
    title: String,
    hint: String,
    text: String,
    command: Vec<String>,
}

/// A line on the overview the highlight can rest on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum OverviewItem {
    Monitor(MonitorId),
    Window(WindowId),
    /// A binding listed by a search: the chord in the mode (an index into
    /// `Keymap::modes`, `None` for a direct chord); Enter runs it.
    Key {
        mode: Option<usize>,
        chord: KeyChord,
    },
}

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

/// A key row matches when a word of its text (chord, command, mode; `-`
/// and `+` split words too) starts with the query, or the text contains a
/// multi-word query (`workspace 3`). Windows match on any substring; keys
/// are short words, where `g` matching `toggle` is noise.
fn key_text_matches(text: &str, q: &str) -> bool {
    if q.contains(' ') {
        return text.contains(q);
    }
    text.split([' ', '-', '+']).any(|w| w.starts_with(q))
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
    /// app, adopts existing windows into workspace trees, un-parks any
    /// stray parked windows, and either lays everything out or, when
    /// `saved` is given, starts the restore session (which lays out once,
    /// so windows never see the interim layout; the caller decides whether
    /// `restore.enabled` applies). Does NOT install the key tap (daemon
    /// does).
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
            last_raise: None,
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
            overview_items: Vec::new(),
            overview_pick: None,
            overview_query: None,
            overview_sel: 0,
            overview_open: Vec::new(),
            prompt: None,
            start_at_login: false,
            desks: Vec::new(),
            profiles: Vec::new(),
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
        if !saved.is_some_and(|s| r.restore_inner(s, true)) {
            r.relayout_all();
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

    /// Whether tatamid starts at login, as the daemon found it; shown in
    /// the menu.
    pub fn set_start_at_login(&mut self, on: bool) {
        if self.start_at_login != on {
            self.start_at_login = on;
            self.status_dirty = true;
        }
    }

    pub fn start_at_login(&self) -> bool {
        self.start_at_login
    }

    /// The saved desks, for the menu; the daemon lists the files.
    pub fn set_desks(&mut self, desks: Vec<tatami_state::DeskInfo>) {
        if self.desks != desks {
            self.desks = desks;
            self.status_dirty = true;
        }
    }

    /// The named profiles, for the menu; the daemon lists the files.
    pub fn set_profiles(&mut self, names: Vec<String>) {
        if self.profiles != names {
            self.profiles = names;
            self.status_dirty = true;
        }
    }

    /// The desk present now: the id of the connected monitors' set.
    pub fn desk_id(&self) -> String {
        let uuids: Vec<&str> = self
            .monitors
            .values()
            .map(|m| m.info.uuid.as_str())
            .collect();
        tatami_state::desk_id_of(&uuids)
    }

    /// `3 monitors: A, B, C`, left to right.
    pub fn desk_name(&self) -> String {
        let names: Vec<&str> = self
            .order
            .iter()
            .map(|&m| self.monitors[m].info.name.as_str())
            .collect();
        tatami_state::desk_name(&names)
    }

    pub fn enabled(&self) -> bool {
        self.enabled
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
            // A menu row may chain commands with a `;` word (`focus-monitor
            // 2 ; workspace 3`); the chain stops at the first failure.
            PlatformEvent::Command(args) => {
                for part in args.split(|a| a == ";") {
                    match Action::parse(part) {
                        Ok(a) => {
                            if let Err(e) = self.run_one(&a) {
                                tracing::warn!(?args, "menu command failed: {e}");
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(?args, "menu command: {e}");
                            break;
                        }
                    }
                }
            }
            PlatformEvent::TrustChanged(on) => {
                let was = self.trusted;
                self.trusted = on;
                if !on {
                    let ch = self.keys.focus_lost();
                    self.set_overlay(ch);
                } else if !was && self.enabled {
                    // A restore that waited for trust gets its full
                    // timeout from now.
                    let d = self.now + Duration::from_secs(self.cfg.restore.timeout_s);
                    if let Some(s) = self.restore.as_mut() {
                        s.restart_clock(d);
                    }
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
        if self.prompt.is_some()
            && let RawKeyEvent::Down { keycode, mods, .. } = raw
            && !mods.ctrl
            && !mods.cmd
        {
            return self.prompt_key(keycode, mods, now);
        }
        if self.overview
            && let RawKeyEvent::Down { keycode, mods, .. } = raw
            && !mods.ctrl
            && !mods.cmd
        {
            return self.overview_key(keycode, mods, now);
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
        // No restore clock without trust (`expire_restore` would not act
        // on it, and a due-but-ignored deadline would spin the timer).
        let rs = self
            .restore
            .as_ref()
            .filter(|_| self.trusted)
            .and_then(|s| s.next_deadline());
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

    /// After a failed handover: whatever the other daemon moved before
    /// it gave up, put back.
    pub fn resume(&mut self, now: Instant) -> Vec<Outcome> {
        self.now = now;
        self.rescan();
        self.relayout_all();
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
            // The row switches the monitor it is listed under, not the
            // focused one: the menu bar is on one monitor and the pointer
            // sits there while the menu is open. Its index is the one
            // `focus-monitor N` and `list-monitors` use.
            let idx = self
                .order
                .iter()
                .position(|&m| m == id)
                .map_or(1, |i| i + 1)
                .to_string();
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
                let mut item =
                    MenuItem::action(title, &["focus-monitor", &idx, ";", "workspace", name]);
                item.checked = visible == Some(ws);
                item.indent = 1;
                rows.push(item);
            }
        }
        rows.push(MenuItem::separator());
        // Profiles: the default ones are the desks, one layout per set of
        // monitors kept by itself, the present one checked; then the
        // named ones. Picking any applies its layout to the monitors
        // present.
        rows.push(MenuItem::header("Profiles"));
        let here = self.desk_id();
        for d in &self.desks {
            let mut item =
                MenuItem::action(format!("Default · {}", d.name), &["desk", "restore", &d.id]);
            item.checked = d.id == here;
            item.indent = 1;
            rows.push(item);
        }
        for name in &self.profiles {
            let mut item = MenuItem::action(name.clone(), &["profile", "restore", name]);
            item.indent = 1;
            rows.push(item);
        }
        rows.push(MenuItem::action("Save profile…", &["profile", "save-as"]));
        if !self.profiles.is_empty() {
            let mut del = MenuItem::header("Delete profile");
            del.children = self
                .profiles
                .iter()
                .map(|n| MenuItem::action(n.clone(), &["profile", "delete", n]))
                .collect();
            rows.push(del);
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
        rows.push(MenuItem::action("Reload config", &["reload-config"]));
        rows.push(MenuItem::action("Edit config…", &["edit-config"]));
        rows.push(MenuItem::separator());
        let mut login = MenuItem::action("Start at login", &["start-at-login", "toggle"]);
        login.checked = self.start_at_login;
        rows.push(login);
        rows.push(MenuItem::action(
            "Hot-swap to the installed build",
            &["hot-swap"],
        ));
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

    /// A key while the overview is up: an outline, walked with the arrows.
    /// `↑`/`↓` move the highlight over monitor headers and windows, `→`
    /// expands the highlighted monitor and `←` collapses it (from a window
    /// too), Enter focuses the window (or toggles the monitor); a letter
    /// picks its window directly, `1`-`9` expand monitor N. `/` (or space)
    /// starts a search over every monitor and over the keymap; typing
    /// filters, Backspace edits, Enter on a key row runs that binding. Esc
    /// closes; any other key closes it and goes on as a normal key.
    fn overview_key(&mut self, keycode: u16, mods: Mods, now: Instant) -> KeyDisposition {
        let key = keyname_from_keycode(keycode);
        let close = |r: &mut Self| {
            let ch = r.keys.enter_mode(None, now);
            r.set_overlay(ch);
        };
        let redraw = |r: &mut Self| r.set_overlay(OverlayChange::Show(Level::Top));
        // The lines changed: rebuild the items and keep the highlight on
        // `keep` where it is still listed, else on the first line.
        let rebuild = |r: &mut Self, keep: Option<OverviewItem>| {
            r.overview_items = r.overview_rows().1;
            r.overview_sel = keep
                .and_then(|k| r.overview_items.iter().position(|&i| i == k))
                .unwrap_or(0);
            redraw(r);
        };
        let current = self.overview_items.get(self.overview_sel).copied();
        match (self.overview_query.is_some(), key) {
            (_, Some(KeyName::Escape)) => {
                close(self);
                KeyDisposition::Swallow
            }
            (false, Some(KeyName::Right)) => {
                if let Some(OverviewItem::Monitor(m)) = current
                    && !self.overview_open.contains(&m)
                {
                    self.overview_open.push(m);
                    rebuild(self, current);
                }
                KeyDisposition::Swallow
            }
            (false, Some(KeyName::Left)) => {
                let m = match current {
                    Some(OverviewItem::Monitor(m)) => Some(m),
                    Some(OverviewItem::Window(w)) => Some(self.ws_monitor(self.wins[w].ws)),
                    Some(OverviewItem::Key { .. }) | None => None,
                };
                if let Some(m) = m {
                    self.overview_open.retain(|&o| o != m);
                    rebuild(self, Some(OverviewItem::Monitor(m)));
                }
                KeyDisposition::Swallow
            }
            (false, Some(KeyName::Char(d))) if d.is_ascii_digit() && d != '0' => {
                let n = d as usize - '1' as usize;
                if let Some(&m) = self.monitors_left_to_right().get(n) {
                    if !self.overview_open.contains(&m) {
                        self.overview_open.push(m);
                    }
                    rebuild(self, Some(OverviewItem::Monitor(m)));
                }
                KeyDisposition::Swallow
            }
            // Letters mode.
            (false, Some(KeyName::Char('/'))) | (false, Some(KeyName::Space)) => {
                self.overview_query = Some(String::new());
                rebuild(self, None);
                KeyDisposition::Swallow
            }
            (false, Some(KeyName::Char(c))) => {
                let picked = overview_index(c).and_then(|i| {
                    self.overview_items
                        .iter()
                        .filter_map(|it| match it {
                            OverviewItem::Window(w) => Some(*w),
                            _ => None,
                        })
                        .nth(i)
                });
                close(self);
                match picked {
                    Some(id) => {
                        self.overview_pick = Some(id);
                        KeyDisposition::Swallow
                    }
                    None => KeyDisposition::Pass,
                }
            }
            // Either mode: ↑/↓ move the highlight, Enter acts on it.
            (_, Some(KeyName::Enter)) => {
                match current {
                    Some(OverviewItem::Window(w)) => {
                        close(self);
                        self.overview_pick = Some(w);
                    }
                    Some(OverviewItem::Monitor(m)) => {
                        if self.overview_open.contains(&m) {
                            self.overview_open.retain(|&o| o != m);
                        } else {
                            self.overview_open.push(m);
                        }
                        rebuild(self, current);
                    }
                    // The binding runs as if its chord had been pressed:
                    // the queue takes the whole list, mode changes
                    // included, and `drain` runs it after the sheet is gone.
                    Some(OverviewItem::Key { mode, chord }) => {
                        let actions = self.overview_binding(mode, &chord);
                        close(self);
                        self.key_queue.extend(actions);
                    }
                    None => close(self),
                }
                KeyDisposition::Swallow
            }
            (true, Some(KeyName::Backspace)) => {
                if self.overview_query.as_mut().and_then(String::pop).is_none() {
                    self.overview_query = None;
                }
                rebuild(self, None);
                if self.overview_query.is_none() {
                    self.overview_sel = self.overview_focused_index();
                    redraw(self);
                }
                KeyDisposition::Swallow
            }
            (_, Some(KeyName::Down)) | (_, Some(KeyName::Tab)) if !mods.shift => {
                if !self.overview_items.is_empty() {
                    self.overview_sel = (self.overview_sel + 1) % self.overview_items.len();
                }
                redraw(self);
                KeyDisposition::Swallow
            }
            (_, Some(KeyName::Up)) | (_, Some(KeyName::Tab)) => {
                let n = self.overview_items.len();
                if n > 0 {
                    self.overview_sel = (self.overview_sel + n - 1) % n;
                }
                redraw(self);
                KeyDisposition::Swallow
            }
            (true, Some(KeyName::Char(c))) => {
                let c = if mods.shift {
                    c.to_ascii_uppercase()
                } else {
                    c
                };
                if let Some(q) = self.overview_query.as_mut() {
                    q.push(c);
                }
                rebuild(self, None);
                KeyDisposition::Swallow
            }
            (true, Some(KeyName::Space)) => {
                if let Some(q) = self.overview_query.as_mut() {
                    q.push(' ');
                }
                rebuild(self, None);
                KeyDisposition::Swallow
            }
            (true, _) => KeyDisposition::Swallow,
            (false, _) => {
                close(self);
                KeyDisposition::Pass
            }
        }
    }

    /// What a key row's binding does, as a list to queue.
    fn overview_binding(&self, mode: Option<usize>, chord: &KeyChord) -> Vec<Action> {
        let found = match mode {
            None => self.keymap.lookup_direct(chord),
            Some(i) => self
                .keymap
                .modes()
                .get(i)
                .and_then(|m| self.keymap.lookup_mode(m, chord)),
        };
        found.map(<[Action]>::to_vec).unwrap_or_default()
    }

    /// The bindings a search for `q` (lowercase) turns up, direct chords
    /// first then each mode's: the chord as you would press it (`alt+s u`
    /// for a key inside a mode), what it does, and where it lives. A
    /// match is on the start of any of those words (`key_text_matches`),
    /// so `service` lists the whole `alt+s` mode, `swap` finds `hot-swap`
    /// and `h` the keys on `h`, while a letter inside a word stays quiet.
    /// `Esc`/`Enter` → normal in every mode is chrome and left out.
    fn overview_keys(&self, q: &str) -> Vec<(String, String, Option<usize>, KeyChord)> {
        let modes = self.keymap.modes();
        let mut out = Vec::new();
        for (mode, chord, actions) in self.keymap.bindings() {
            if actions.iter().all(|a| matches!(a, Action::Normal)) {
                continue;
            }
            let mode_i = mode.and_then(|m| modes.iter().position(|n| n == m));
            if mode.is_some() && mode_i.is_none() {
                continue;
            }
            let mut words = String::new();
            if let Some(m) = mode {
                let path = self.keymap.mode_path(m);
                if path.is_empty() {
                    // A mode nothing binds (entered from the CLI).
                    words.push('+');
                    words.push_str(m);
                    words.push(' ');
                }
                for c in path {
                    words.push_str(&c.display_relative(tatami_keymap::Mods::NONE));
                    words.push(' ');
                }
            }
            words.push_str(&chord.display_relative(tatami_keymap::Mods::NONE));
            let desc = self.keymap.describe(actions);
            let mut hay = format!("{words} {desc} {}", mode.unwrap_or(""));
            for a in actions {
                hay.push(' ');
                hay.push_str(&tatami_config::command_words(a));
            }
            if key_text_matches(&hay.to_lowercase(), q) {
                out.push((words, desc, mode_i, *chord));
            }
        }
        out
    }

    /// Open a prompt on the sheet; Enter runs `command` plus the text.
    pub(super) fn prompt_show(&mut self, title: &str, hint: &str, command: &[&str]) {
        self.overview = false;
        self.prompt = Some(Prompt {
            title: title.into(),
            hint: hint.into(),
            text: String::new(),
            command: command.iter().map(|s| s.to_string()).collect(),
        });
        let ch = self.keys.pin();
        self.set_overlay(ch);
        self.set_overlay(OverlayChange::Show(Level::Top));
    }

    /// A key while a prompt is up: letters, digits, `-`, `_` and space
    /// are the text (shift makes capitals), Backspace edits, Enter runs
    /// the command with a non-empty text, Esc cancels. Everything else is
    /// swallowed: the sheet has the keyboard until it is answered.
    fn prompt_key(&mut self, keycode: u16, mods: Mods, now: Instant) -> KeyDisposition {
        let close = |r: &mut Self| {
            r.prompt = None;
            let ch = r.keys.enter_mode(None, now);
            r.set_overlay(ch);
        };
        match keyname_from_keycode(keycode) {
            Some(KeyName::Escape) => close(self),
            Some(KeyName::Enter) => {
                let Some(p) = self.prompt.as_ref() else {
                    return KeyDisposition::Swallow;
                };
                let text = p.text.trim().to_string();
                if text.is_empty() {
                    return KeyDisposition::Swallow;
                }
                let mut argv = p.command.clone();
                argv.push(text);
                match Action::parse(&argv) {
                    Ok(a) => self.key_queue.push(a),
                    Err(e) => tracing::warn!("prompt: {e}"),
                }
                close(self);
            }
            Some(KeyName::Backspace) => {
                if let Some(p) = self.prompt.as_mut() {
                    p.text.pop();
                }
                self.set_overlay(OverlayChange::Show(Level::Top));
            }
            Some(KeyName::Space) => {
                if let Some(p) = self.prompt.as_mut()
                    && !p.text.is_empty()
                {
                    p.text.push(' ');
                }
                self.set_overlay(OverlayChange::Show(Level::Top));
            }
            Some(KeyName::Char(c)) if c.is_ascii_alphanumeric() || c == '-' || c == '_' => {
                let c = if mods.shift {
                    c.to_ascii_uppercase()
                } else {
                    c
                };
                if let Some(p) = self.prompt.as_mut() {
                    p.text.push(c);
                }
                self.set_overlay(OverlayChange::Show(Level::Top));
            }
            _ => {}
        }
        KeyDisposition::Swallow
    }

    fn prompt_content(&self, p: &Prompt) -> OverlayContent {
        OverlayContent {
            title: format!("{}: {}▏", p.title, p.text),
            exit_hint: "Enter  save  ·  Esc  cancel".into(),
            rows: vec![OverlayRow {
                key: String::new(),
                desc: p.hint.clone(),
                prefix: true,
                command: Vec::new(),
            }],
            columns: Some(1),
            max_desc: Some(80),
            list: true,
        }
    }

    /// The desk as an outline: every monitor (left to right) as a header,
    /// and under each expanded one its workspaces (visible one starred)
    /// with one lettered row per window. While searching every monitor is
    /// searched, only matching windows are listed and the highlighted one
    /// is marked, and a `keys` section below lists the bindings the text
    /// matches. Returns the rows and the items the highlight can rest on,
    /// in order.
    fn overview_rows(&self) -> (Vec<OverlayRow>, Vec<OverviewItem>) {
        let mons = self.monitors_left_to_right();
        let query = self.overview_query.as_deref().map(str::to_lowercase);
        let searching = query.is_some();
        let mut rows = Vec::new();
        let mut items: Vec<OverviewItem> = Vec::new();
        let mut letters = 0usize;
        for (i, &id) in mons.iter().enumerate() {
            let Some(mon) = self.monitors.get(id) else {
                continue;
            };
            let open = searching || self.overview_open.contains(&id);
            let count: usize = self
                .model
                .workspaces_on(id)
                .into_iter()
                .map(|ws| self.ws_windows(ws).len())
                .sum();
            let here = if id == self.focused_monitor {
                " ◆"
            } else {
                ""
            };
            // Headers are items only outside a search; a click expands.
            let sel = if !searching && items.len() == self.overview_sel {
                "▶ "
            } else {
                "  "
            };
            if !searching {
                items.push(OverviewItem::Monitor(id));
            }
            let header_at = rows.len();
            // A blank line between monitors; the marker column is fixed so
            // nothing shifts as the highlight moves.
            if i > 0 {
                rows.push(OverlayRow {
                    key: String::new(),
                    desc: String::new(),
                    prefix: true,
                    command: Vec::new(),
                });
            }
            rows.push(OverlayRow {
                key: if open { "▾" } else { "▸" }.into(),
                desc: format!("{sel}{}  {}  ({count}){here}", i + 1, mon.info.name),
                prefix: true,
                command: vec!["overview".into(), (i + 1).to_string()],
            });
            if !open {
                continue;
            }
            let visible = self.model.visible_on(id);
            let mut any_on_monitor = false;
            for ws in self.model.workspaces_on(id) {
                let Some(name) = self.model.name_of(ws) else {
                    continue;
                };
                let mark = if visible == Some(ws) { " *" } else { "" };
                let mut ws_rows = vec![OverlayRow {
                    key: String::new(),
                    desc: format!("   workspace {name}{mark}"),
                    prefix: true,
                    command: Vec::new(),
                }];
                let mut any = false;
                for w in self.ws_windows(ws) {
                    let m = &self.wins[w];
                    if let Some(q) = &query
                        && !q.is_empty()
                        && !format!("{} {} {} {}", m.w.app_name, m.w.title, name, mon.info.name)
                            .to_lowercase()
                            .contains(q.as_str())
                    {
                        continue;
                    }
                    let selected = items.len() == self.overview_sel;
                    let key = if searching {
                        String::new()
                    } else {
                        overview_letter(letters)
                            .map(String::from)
                            .unwrap_or_default()
                    };
                    letters += 1;
                    let sel = if selected { "  ▶ " } else { "    " };
                    let float = if m.w.kind == WindowKind::Floating {
                        "~ "
                    } else {
                        ""
                    };
                    let desc = if m.w.title.is_empty() || m.w.title == m.w.app_name {
                        format!("{sel}{float}{}", m.w.app_name)
                    } else {
                        format!("{sel}{float}{} — {}", m.w.app_name, m.w.title)
                    };
                    ws_rows.push(OverlayRow {
                        key,
                        desc,
                        prefix: false,
                        command: vec![
                            "focus".into(),
                            "--window-id".into(),
                            m.w.handle.0.to_string(),
                        ],
                    });
                    items.push(OverviewItem::Window(w));
                    any = true;
                }
                // While searching, a workspace with no match has no header.
                if any || !searching {
                    rows.extend(ws_rows);
                }
                any_on_monitor |= any;
            }
            // Nor does a monitor with none.
            if searching && !any_on_monitor {
                rows.truncate(header_at);
            }
        }
        // The keymap, once there is text to match: an empty search is the
        // whole desk, not every key as well (`alt+/` is the key sheet).
        if let Some(q) = query.as_deref().filter(|q| !q.is_empty()) {
            let keys = self.overview_keys(q);
            let width = keys.iter().map(|k| k.0.chars().count()).max();
            if let Some(width) = width {
                if !rows.is_empty() {
                    rows.push(OverlayRow {
                        key: String::new(),
                        desc: String::new(),
                        prefix: true,
                        command: Vec::new(),
                    });
                }
                rows.push(OverlayRow {
                    key: "▾".into(),
                    desc: format!("  keys  ({})", keys.len()),
                    prefix: true,
                    command: Vec::new(),
                });
                for (words, desc, mode_i, chord) in keys {
                    let sel = if items.len() == self.overview_sel {
                        "  ▶ "
                    } else {
                        "    "
                    };
                    let mut command = vec![
                        "trigger-binding".to_string(),
                        chord.display_relative(tatami_keymap::Mods::NONE),
                    ];
                    if let Some(m) = mode_i.and_then(|i| self.keymap.modes().get(i)) {
                        command.push("--mode".into());
                        command.push(m.clone());
                    }
                    rows.push(OverlayRow {
                        key: String::new(),
                        desc: format!("{sel}{words:<width$}  {desc}"),
                        prefix: false,
                        command,
                    });
                    items.push(OverviewItem::Key {
                        mode: mode_i,
                        chord,
                    });
                }
            }
        }
        (rows, items)
    }

    /// Opens the overview (or keeps it open) with `expand` expanded, the
    /// highlight on `select` when it is listed, else on the focused window
    /// or monitor.
    pub(super) fn overview_show(&mut self, expand: MonitorId, select: Option<OverviewItem>) {
        self.overview = true;
        self.overview_query = None;
        if !self.overview_open.contains(&expand) {
            self.overview_open.push(expand);
        }
        self.overview_items = self.overview_rows().1;
        self.overview_sel = select
            .and_then(|s| self.overview_items.iter().position(|&i| i == s))
            .unwrap_or_else(|| self.overview_focused_index());
        let ch = self.keys.pin();
        self.set_overlay(ch);
        self.set_overlay(OverlayChange::Show(Level::Top));
    }

    /// The monitor `general.overview_monitor` names: `main`, `pointer`,
    /// `focused`, a number or part of a name. Unknown → the main display.
    fn overview_monitor(&self) -> Option<MonitorId> {
        let main = || {
            self.monitors
                .iter()
                .find(|(_, m)| m.info.is_main)
                .map(|(i, _)| i)
        };
        match self.cfg.general.overview_monitor.trim() {
            "" | "main" => main(),
            "pointer" => self.monitor_under_mouse().or_else(main),
            "focused" => Some(self.focused_monitor),
            pat => self
                .resolve_monitor(&MonitorTarget::Pattern(pat.to_string()), false)
                .ok()
                .or_else(main),
        }
    }

    /// Where the highlight starts: the focused window's row, else the
    /// focused monitor's header, else the first line.
    pub(super) fn overview_focused_index(&self) -> usize {
        let win = self.focused.map(OverviewItem::Window);
        let mon = OverviewItem::Monitor(self.focused_monitor);
        self.overview_items
            .iter()
            .position(|&i| Some(i) == win)
            .or_else(|| self.overview_items.iter().position(|&i| i == mon))
            .unwrap_or(0)
    }

    /// Monitor ids ordered by their x position, the order of the overview
    /// and of `focus-monitor next`.
    fn monitors_left_to_right(&self) -> Vec<MonitorId> {
        let mut mons: Vec<(MonitorId, f64)> = self
            .monitors
            .iter()
            .map(|(id, m)| (id, m.info.frame.x))
            .collect();
        mons.sort_by(|a, b| a.1.total_cmp(&b.1));
        mons.into_iter().map(|(id, _)| id).collect()
    }

    fn overview_content(&self) -> OverlayContent {
        let (rows, items) = self.overview_rows();
        let title = match &self.overview_query {
            None => {
                "desk  ·  ↑↓ move  → open  ← close  ·  Enter or letter: focus  ·  / search windows and keys"
                    .to_string()
            }
            Some(q) => format!(
                "search: {q}  ({} match{})",
                items.len(),
                if items.len() == 1 { "" } else { "es" }
            ),
        };
        let exit_hint = if self.overview_query.is_some() {
            "↑↓ Enter  ·  Esc  close"
        } else {
            "Esc  close"
        };
        OverlayContent {
            title,
            exit_hint: exit_hint.into(),
            rows,
            columns: Some(1),
            max_desc: Some(80),
            list: true,
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
                command: Vec::new(),
            })
            .collect();
        OverlayContent {
            title,
            exit_hint,
            rows,
            columns: None,
            max_desc: None,
            list: false,
        }
    }

    fn apply_overlay(&mut self, ch: OverlayChange) {
        let content = match ch {
            OverlayChange::Keep => return,
            OverlayChange::Hide => {
                self.overview = false;
                self.prompt = None;
                None
            }
            OverlayChange::Show(_) if self.prompt.is_some() => {
                self.prompt.as_ref().map(|p| self.prompt_content(p))
            }
            OverlayChange::Show(_) if self.overview => Some(self.overview_content()),
            OverlayChange::Show(level) => Some(self.overlay_content(&level)),
        };
        // The sheet goes where the user is looking: the monitor under the
        // pointer, else the focused window's monitor.
        let fallback;
        let under_mouse = self.monitor_under_mouse();
        // The overview is the desk's map: it opens where the config says
        // (`general.overview_monitor`, default the main display), wherever
        // the pointer is.
        let target = if self.overview {
            self.overview_monitor().or(under_mouse)
        } else {
            under_mouse
        };
        let mon = match self.monitors.get(target.unwrap_or(self.focused_monitor)) {
            Some(m) => &m.info,
            None => {
                fallback = self.focused_monitor();
                &fallback
            }
        };
        self.p.show_overlay(content.as_ref(), mon);
        // The overview's highlight is also shown on the desk itself: an
        // outline around the monitor it is on, so the list and the room
        // agree.
        let mark = if content.is_some() && self.overview && self.prompt.is_none() {
            self.overview_marked()
                .and_then(|m| self.monitors.get(m))
                .map(|m| m.info.clone())
        } else {
            None
        };
        self.p.mark_monitor(mark.as_ref());
    }

    /// The monitor the overview's highlight is on: the header itself, or
    /// the highlighted window's. A key row is on none.
    fn overview_marked(&self) -> Option<MonitorId> {
        let (_, items) = self.overview_rows();
        match items.get(self.overview_sel)? {
            OverviewItem::Monitor(m) => Some(*m),
            OverviewItem::Window(w) => self.wins.get(*w).map(|w| self.ws_monitor(w.ws)),
            OverviewItem::Key { .. } => None,
        }
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

    /// The lowest configured workspace name on `m` that has no windows,
    /// creating it when it does not exist yet; `None` when all are taken.
    fn own_workspace(&mut self, m: MonitorId) -> Option<WorkspaceId> {
        let names = self.cfg.workspaces.names.clone();
        for name in &names {
            match self.model.find(m, name) {
                Some(ws) if self.ws_windows(ws).is_empty() => return Some(ws),
                Some(_) => continue,
                None => return self.create_ws(m, name),
            }
        }
        None
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
        let screens = distinct_screens(self.p.screens());
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
        // Leaving a desk: hand its layout to the daemon before the
        // vanished monitor's workspaces are folded into the rest, so what
        // is filed is the arrangement as it was, not the fallback.
        let before = self.desk_id();
        let now_present: Vec<String> = distinct_screens(self.p.screens())
            .into_iter()
            .map(|s| s.uuid)
            .collect();
        let changing = !now_present.is_empty() && tatami_state::desk_id_of(&now_present) != before;
        let leaving = changing.then(|| Box::new(self.snapshot()));
        self.sync_screens();
        // A desk never saved has nothing for the daemon to hand back; it
        // starts the way the config says a window should.
        if changing && !self.desks.iter().any(|d| d.id == self.desk_id()) {
            self.spread_unsaved_desk();
        }
        if let Some(id) = self.focused {
            self.focused_monitor = self.ws_monitor(self.wins[id].ws);
        }
        self.relayout_all();
        // Rare, and the one record of what a desk looked like when a
        // report comes in from another set of monitors.
        let listing: Vec<String> = self
            .order
            .iter()
            .enumerate()
            .map(|(i, &m)| {
                let info = &self.monitors[m].info;
                let f = &info.frame;
                format!(
                    "{} {} {}x{}{:+}{:+} {}",
                    i + 1,
                    info.name,
                    f.w,
                    f.h,
                    f.x,
                    f.y,
                    info.uuid
                )
            })
            .collect();
        tracing::info!("screens: {}", listing.join("; "));
        let monitors = self.order.iter().map(|&m| self.mon_uuid(m)).collect();
        self.emit(IpcEvent::MonitorsChanged { monitors });
        self.gc();
        self.status_dirty = true;
        self.dirty = true;
        if let Some(saved) = leaving {
            let to = self.desk_id();
            tracing::info!(from = %saved.desk_name(), to = %self.desk_name(), "desk changed");
            self.out.push(Outcome::DeskChanged {
                from: before,
                saved,
                to,
            });
        }
    }

    /// Arriving on a desk never saved, with `new_window = "own"`: on each
    /// monitor every app gets a workspace of its own, its windows tiled
    /// together there, as macOS groups windows by app. The fold alone
    /// piles the vanished monitors' windows into one workspace by name,
    /// and a laptop on its own would show them all at once; one per
    /// window would run past nine. Floating windows stay; past the named
    /// workspaces the rest share the visible one. A monitor whose
    /// workspaces already hold one app each is left alone.
    fn spread_unsaved_desk(&mut self) {
        if self.cfg.workspaces.new_window != tatami_config::NewWindow::OwnWorkspace {
            return;
        }
        for m in self.order.clone() {
            // Apps in order of first appearance, each with its windows.
            let mut groups: Vec<(String, Vec<WindowId>)> = Vec::new();
            let mut tidy = true;
            for ws in self.model.workspaces_on(m) {
                let mut apps_here: Vec<String> = Vec::new();
                for id in self.ws_windows(ws) {
                    if self.wins[id].leaf.is_none() {
                        continue;
                    }
                    let key = app_key(&self.wins[id].w);
                    match groups.iter_mut().find(|g| g.0 == key) {
                        Some(g) => {
                            // Seen in an earlier workspace: the app is split.
                            tidy &= g.1.iter().all(|&o| self.wins[o].ws == ws);
                            g.1.push(id);
                        }
                        None => groups.push((key.clone(), vec![id])),
                    }
                    if !apps_here.contains(&key) {
                        apps_here.push(key);
                    }
                }
                tidy &= apps_here.len() <= 1;
            }
            if tidy {
                continue;
            }
            for g in &groups {
                for &id in &g.1 {
                    self.detach(id);
                }
            }
            let mut show = None;
            for (_, ids) in groups {
                let Some(ws) = self.own_workspace(m).or_else(|| self.visible_ws(m)) else {
                    continue;
                };
                for id in ids {
                    self.attach(id, ws, false);
                    if self.focused == Some(id) || show.is_none() {
                        show = Some(ws);
                    }
                }
            }
            if let Some(ws) = show
                && !self.model.is_visible(ws)
            {
                let sw = self.model.switch(m, ws);
                self.apply_switch(sw, false);
            }
        }
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
        let own = kind == WindowKind::Tiled
            && rule_ws.is_none()
            && self.cfg.workspaces.new_window == tatami_config::NewWindow::OwnWorkspace;
        let ws = match rule_ws.and_then(|n| self.create_ws(m, &n)) {
            Some(ws) => ws,
            None => match (own.then(|| self.own_workspace(m)).flatten())
                .or_else(|| self.visible_ws(m))
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
        // A saved slot wins however the window turned up: a rescan after
        // trust arrives finds every window at once, and each one belongs
        // in its slot, not on the next empty workspace.
        if self.try_restore_slot(id) {
            return;
        }
        self.attach(id, ws, fresh);
        // A fresh window on its own workspace: show it, like the app
        // intended when it opened the window.
        if fresh && own && self.enabled && !self.model.is_visible(ws) {
            let sw = self.model.switch(m, ws);
            self.apply_switch(sw, false);
        }
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
        self.last_raise = Some((self.wins[id].w.pid, id, self.now));
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
            // A parked window of the app we just raised another window of:
            // the activation's side effect (the app's old key window
            // surfaces first), not the user. Re-assert what they asked
            // for instead of switching that workspace in.
            if let Some((pid, intended, at)) = self.last_raise
                && pid == self.wins[id].w.pid
                && intended != id
                && self.now.duration_since(at) < RAISE_ECHO_FOR
                && let Some(w) = self.wins.get(intended)
            {
                tracing::debug!(app = %w.w.app_name, "ignoring a parked window's focus echo after our raise");
                let h = w.w.handle;
                if self.p.raise(h).is_ok() {
                    self.raise_echo.push(h);
                }
                return;
            }
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

/// What "the same app" means for grouping: the bundle id, else the
/// process.
fn app_key(w: &Window) -> String {
    w.bundle_id
        .clone()
        .unwrap_or_else(|| format!("pid:{}", w.pid))
}
