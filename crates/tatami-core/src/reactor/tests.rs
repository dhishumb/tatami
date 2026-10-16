//! Reactor rules, each proven against `FakePlatform`.

use std::time::{Duration, Instant};

use tatami_config::{Config, Outer, Rule};
use tatami_protocol::{IpcEvent, Layout, Rect, Response, Size, WindowHandle};
use tatami_state::{SavedMonitor, SavedNode, SavedState, SavedWindow, SavedWorkspace};

use super::{Outcome, OverviewItem, Reactor};
use crate::fake::{Behaviour, FakeCall, FakePlatform};
use crate::parking::is_parked;
use crate::platform::{
    KeyDisposition, Mods, MonitorInfo, Platform, PlatformEvent, RawKeyEvent, SetOrder,
};
use crate::tree::{Leaf, NodeKind};
use crate::window::WindowKind;

const W: f64 = 1000.0;
const H: f64 = 800.0;

fn mon(uuid: &str, x: f64) -> MonitorInfo {
    MonitorInfo {
        uuid: uuid.into(),
        name: format!("Display {uuid}"),
        frame: Rect::new(x, 0.0, W, H),
        visible: Rect::new(x, 0.0, W, H),
        is_main: x == 0.0,
    }
}

fn cfg() -> Config {
    let mut c = Config::default();
    c.gaps.inner = 0;
    c.gaps.outer = Outer::all(0);
    // Most tests exercise tiling within one workspace.
    c.workspaces.new_window = tatami_config::NewWindow::Tile;
    c
}

fn cfg_own() -> Config {
    let mut c = cfg();
    c.workspaces.new_window = tatami_config::NewWindow::OwnWorkspace;
    c
}

fn r(x: f64, y: f64, w: f64, h: f64) -> Rect {
    Rect::new(x, y, w, h)
}

/// A window to create before the reactor starts.
struct Win {
    bundle: &'static str,
    title: &'static str,
    frame: Rect,
}

fn win(bundle: &'static str, title: &'static str, x: f64) -> Win {
    Win {
        bundle,
        title,
        frame: r(x, 100.0, 300.0, 300.0),
    }
}

struct T {
    r: Reactor<FakePlatform>,
    now: Instant,
    out: Vec<Outcome>,
}

/// Fake with `wins` (one app per distinct bundle id), before the reactor.
fn fake_with(mons: Vec<MonitorInfo>, wins: &[Win]) -> (FakePlatform, Vec<WindowHandle>) {
    let f = FakePlatform::new(mons);
    let mut pids: Vec<(&str, i32)> = Vec::new();
    let mut hs = Vec::new();
    for w in wins {
        let pid = match pids.iter().find(|p| p.0 == w.bundle) {
            Some(p) => p.1,
            None => {
                let pid = f.add_app(w.bundle, &w.bundle.replace("com.", ""));
                pids.push((w.bundle, pid));
                pid
            }
        };
        hs.push(f.add_window(pid, w.title, w.frame));
    }
    f.take_events();
    (f, hs)
}

/// Start and settle, keeping the platform call log.
fn start_logged(f: FakePlatform, c: Config, saved: Option<SavedState>) -> T {
    let now = Instant::now();
    let mut t = T {
        r: Reactor::new(f, c, saved, now),
        now,
        out: Vec::new(),
    };
    t.settle();
    t
}

fn start(f: FakePlatform, c: Config, saved: Option<SavedState>) -> T {
    let mut t = start_logged(f, c, saved);
    t.calls();
    t
}

fn setup(mons: Vec<MonitorInfo>, wins: &[Win]) -> (T, Vec<WindowHandle>) {
    let (f, hs) = fake_with(mons, wins);
    (start(f, cfg(), None), hs)
}

impl T {
    fn fake(&mut self) -> &mut FakePlatform {
        self.r.platform()
    }

    fn pump(&mut self) {
        for _ in 0..50 {
            let evs = self.fake().take_events();
            if evs.is_empty() {
                return;
            }
            for e in evs {
                let o = self.r.on_platform_event(e, self.now);
                self.out.extend(o);
            }
        }
        panic!("events never settle");
    }

    /// Deliver events and run frame-verify timers (anything due within a
    /// second) until none is pending. Restore deadlines are left alone.
    fn settle(&mut self) {
        self.pump();
        for _ in 0..20 {
            match self.r.next_due() {
                Some(d) if d <= self.now + Duration::from_secs(1) => {
                    self.now = self.now.max(d);
                    let o = self.r.on_timer(self.now);
                    self.out.extend(o);
                    self.pump();
                }
                _ => return,
            }
        }
        panic!("timers never settle");
    }

    fn event(&mut self, e: PlatformEvent) {
        let o = self.r.on_platform_event(e, self.now);
        self.out.extend(o);
        self.settle();
    }

    fn run(&mut self, cmd: &str) -> Response {
        let args: Vec<String> = cmd.split_whitespace().map(String::from).collect();
        let (resp, o) = self.r.handle_request(&args, self.now);
        self.out.extend(o);
        self.settle();
        resp
    }

    fn ok(&mut self, cmd: &str) {
        let resp = self.run(cmd);
        assert!(resp.ok, "{cmd}: {}", resp.error);
    }

    fn frame(&mut self, h: WindowHandle) -> Rect {
        self.fake().frame(h).expect("window exists")
    }

    fn calls(&mut self) -> Vec<FakeCall> {
        self.fake().take_calls()
    }

    fn focused(&self) -> Option<WindowHandle> {
        self.r.focused.map(|id| self.r.wins[id].w.handle)
    }

    fn focus(&mut self, h: WindowHandle) {
        self.event(PlatformEvent::WindowFocused(h));
    }

    fn key(&mut self, raw: RawKeyEvent) -> KeyDisposition {
        self.r.on_key(raw, self.now)
    }

    /// Run queued key actions; no timers, so the which-key clock stays put.
    fn drain(&mut self) {
        let o = self.r.drain(self.now);
        self.out.extend(o);
        self.pump();
    }

    fn mon_id(&self, uuid: &str) -> crate::MonitorId {
        *self
            .r
            .order
            .iter()
            .find(|&&m| self.r.monitors[m].info.uuid == uuid)
            .unwrap()
    }

    fn visible_name(&self, uuid: &str) -> String {
        let m = self.mon_id(uuid);
        let ws = self.r.model.visible_on(m).unwrap();
        self.r.model.name_of(ws).unwrap().to_string()
    }

    /// Window titles of the focused monitor's workspace in tree order.
    fn order(&self) -> Vec<String> {
        let ws = self.r.model.visible_on(self.r.focused_monitor).unwrap();
        self.r
            .tree
            .leaves(self.r.workspaces[ws].root)
            .into_iter()
            .map(|l| match self.r.tree.window_of(l) {
                Some(id) => self.r.wins[id].w.title.clone(),
                None => "?".into(),
            })
            .collect()
    }

    fn broadcasts(&mut self) -> Vec<IpcEvent> {
        std::mem::take(&mut self.out)
            .into_iter()
            .filter_map(|o| match o {
                Outcome::Broadcast(e) => Some(e),
                _ => None,
            })
            .collect()
    }
}

fn alt() -> Mods {
    Mods {
        alt: true,
        ..Mods::default()
    }
}

fn down(keycode: u16, mods: Mods) -> RawKeyEvent {
    RawKeyEvent::Down {
        keycode,
        mods,
        repeat: false,
    }
}

const K_H: u16 = 0x04;
const K_X: u16 = 0x07;
const K_W: u16 = 0x0D;
const K_R: u16 = 0x0F;
const K_3: u16 = 0x14;

fn set_frames(calls: &[FakeCall], h: WindowHandle) -> Vec<SetOrder> {
    calls
        .iter()
        .filter_map(|c| match c {
            FakeCall::SetFrame { window, order, .. } if *window == h => Some(*order),
            _ => None,
        })
        .collect()
}

// ---- adoption and new windows ----------------------------------------------

#[test]
fn adoption_tiles_two_windows_side_by_side_on_each_monitor() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0), mon("m2", W)],
        &[
            win("com.a", "a", 0.0),
            win("com.b", "b", 400.0),
            win("com.c", "c", 1100.0),
            win("com.d", "d", 1500.0),
        ],
    );
    assert_eq!(t.frame(h[0]), r(0.0, 0.0, 500.0, H));
    assert_eq!(t.frame(h[1]), r(500.0, 0.0, 500.0, H));
    assert_eq!(t.frame(h[2]), r(W, 0.0, 500.0, H));
    assert_eq!(t.frame(h[3]), r(1500.0, 0.0, 500.0, H));
    assert_eq!(t.visible_name("m1"), "1");
    assert_eq!(t.visible_name("m2"), "1", "per-monitor: first name on each");
}

#[test]
fn adoption_unparks_a_stray_parked_window() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[Win {
            bundle: "com.a",
            title: "a",
            frame: r(999.0, 799.0, 400.0, 300.0),
        }],
    );
    assert_eq!(t.frame(h[0]), r(0.0, 0.0, W, H));
}

#[test]
fn new_window_goes_after_the_focused_one_and_takes_focus() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[
            win("com.a", "a", 0.0),
            win("com.a", "b", 300.0),
            win("com.a", "c", 600.0),
        ],
    );
    t.focus(h[0]);
    let pid = t.r.wins[t.r.by_handle[&h[0]]].w.pid;
    let n = t.fake().add_window(pid, "new", r(10.0, 10.0, 100.0, 100.0));
    t.settle();
    assert_eq!(t.order(), ["a", "new", "b", "c"]);
    assert_eq!(t.frame(n), r(250.0, 0.0, 250.0, H));
    assert_eq!(t.frame(h[2]), r(750.0, 0.0, 250.0, H));
    assert_eq!(t.focused(), Some(n));
}

#[test]
fn new_window_lands_on_the_monitor_under_its_centre() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0), mon("m2", W)],
        &[win("com.a", "a", 0.0)],
    );
    let pid = t.r.wins[t.r.by_handle[&h[0]]].w.pid;
    let n = t
        .fake()
        .add_window(pid, "right", r(1200.0, 100.0, 300.0, 300.0));
    t.settle();
    assert_eq!(t.frame(n), r(W, 0.0, W, H));
    assert_eq!(t.frame(h[0]), r(0.0, 0.0, W, H));
}

#[test]
fn focus_event_before_window_created_is_applied_on_arrival() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0), mon("m2", W)],
        &[win("com.a", "a", 0.0), win("com.a", "b", 1200.0)],
    );
    t.focus(h[1]);
    let pid = t.r.wins[t.r.by_handle[&h[0]]].w.pid;
    // AX order: focus first, then creation. The new window is on m1 while
    // m2 is focused, so only the early focus event can make it focused.
    let next = WindowHandle(h[1].0 + 1);
    t.event(PlatformEvent::WindowFocused(next));
    let n = t
        .fake()
        .add_window(pid, "late", r(100.0, 100.0, 300.0, 300.0));
    assert_eq!(n, next);
    t.settle();
    assert_eq!(t.focused(), Some(n));
}

#[test]
fn rules_float_ignore_and_workspace() {
    let mut c = cfg();
    c.rules = vec![
        Rule {
            app_id: Some("com.float".into()),
            float: true,
            ..Rule::default()
        },
        Rule {
            title_regex: Some("^picture".into()),
            ignore: true,
            ..Rule::default()
        },
        Rule {
            app_name_regex: Some("MAIL".into()),
            workspace: Some("3".into()),
            ..Rule::default()
        },
    ];
    let (f, h) = fake_with(
        vec![mon("m1", 0.0)],
        &[
            win("com.a", "a", 0.0),
            win("com.float", "f", 200.0),
            win("com.a", "Picture in picture", 400.0),
            win("com.mail", "inbox", 600.0),
        ],
    );
    let mut t = start(f, c, None);
    assert_eq!(t.frame(h[0]), r(0.0, 0.0, W, H), "only `a` tiles here");
    assert_eq!(
        t.frame(h[1]),
        r(200.0, 100.0, 300.0, 300.0),
        "floating left alone"
    );
    assert!(!t.r.by_handle.contains_key(&h[2]), "ignored");
    let mail = t.frame(h[3]);
    assert!(
        is_parked(&mail, &[r(0.0, 0.0, W, H)]),
        "on hidden 3: {mail:?}"
    );
    t.ok("workspace 3");
    assert_eq!(t.frame(h[3]), r(0.0, 0.0, W, H));
}

// ---- frame transactions ------------------------------------------------------

#[test]
fn window_ignoring_frames_goes_stubborn_after_three_attempts() {
    let (f, h) = fake_with(vec![mon("m1", 0.0)], &[win("com.a", "a", 10.0)]);
    f.set_behaviour(
        h[0],
        Behaviour {
            ignore_frames: true,
            ..Behaviour::default()
        },
    );
    let mut t = start_logged(f, cfg(), None);
    let calls = t.calls();
    assert_eq!(
        &set_frames(&calls, h[0])[..3],
        [
            SetOrder::PosSizePos,
            SetOrder::PosSizePos,
            SetOrder::PosSizePos
        ]
    );
    let id = t.r.by_handle[&h[0]];
    assert_eq!(t.r.wins[id].w.stubborn, Some(Size { w: 300.0, h: 300.0 }));
    assert!(!t.r.tx_pending(), "no transaction left running");
    t.ok("balance-sizes");
    assert!(
        set_frames(&t.calls(), h[0]).len() <= 1,
        "a stubborn window gets at most one untracked request"
    );
    assert!(!t.r.tx_pending());
}

#[test]
fn min_size_window_retries_then_its_slot_grows_to_fit() {
    let (f, h) = fake_with(
        vec![mon("m1", 0.0)],
        &[win("com.a", "a", 0.0), win("com.b", "b", 400.0)],
    );
    f.set_behaviour(
        h[1],
        Behaviour {
            min_size: Some(Size { w: 700.0, h: 0.0 }),
            ..Behaviour::default()
        },
    );
    let mut t = start_logged(f, cfg(), None);
    let calls = t.calls();
    assert!(set_frames(&calls, h[1]).contains(&SetOrder::PosSizePos));
    // b keeps its 700; a keeps its half and is overlapped, ending at the edge.
    assert_eq!(t.frame(h[1]), r(300.0, 0.0, 700.0, H));
    assert_eq!(t.frame(h[0]), r(0.0, 0.0, 500.0, H));
    assert!(!t.r.tx_pending());
}

#[test]
fn dragging_a_tiled_window_snaps_it_back() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[win("com.a", "a", 0.0), win("com.b", "b", 400.0)],
    );
    t.fake().user_set_frame(h[0], r(50.0, 60.0, 200.0, 200.0));
    t.settle();
    assert_eq!(t.frame(h[0]), r(0.0, 0.0, 500.0, H));
}

#[test]
fn our_own_echo_causes_no_second_request() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[win("com.a", "a", 0.0), win("com.b", "b", 400.0)],
    );
    t.focus(h[0]);
    t.calls();
    t.ok("layout v_tiles");
    let calls = t.calls();
    assert_eq!(set_frames(&calls, h[0]), [SetOrder::PosSizePos]);
    assert_eq!(set_frames(&calls, h[1]), [SetOrder::PosSizePos]);
}

// ---- minimize / destroy ------------------------------------------------------

#[test]
fn minimize_removes_from_tree_and_deminimize_reinserts() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[win("com.a", "a", 0.0), win("com.b", "b", 400.0)],
    );
    t.fake().set_minimized(h[0], true).unwrap();
    t.settle();
    assert_eq!(t.frame(h[1]), r(0.0, 0.0, W, H));
    assert!(t.r.wins[t.r.by_handle[&h[0]]].w.minimized);
    t.fake().set_minimized(h[0], false).unwrap();
    t.settle();
    assert_eq!(t.order().len(), 2);
    assert_eq!(t.frame(h[1]).w, 500.0);
    assert_eq!(t.frame(h[0]).w, 500.0);
}

#[test]
fn destroying_the_focused_window_focuses_its_neighbour_and_relays() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[
            win("com.a", "a", 0.0),
            win("com.a", "b", 300.0),
            win("com.a", "c", 600.0),
        ],
    );
    t.focus(h[1]);
    t.ok("close");
    assert_eq!(t.order(), ["a", "c"]);
    assert_eq!(t.focused(), Some(h[2]));
    assert_eq!(t.frame(h[2]), r(500.0, 0.0, 500.0, H));
    assert!(t.out.contains(&Outcome::Dirty));
}

#[test]
fn app_termination_drops_all_its_windows() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[
            win("com.a", "a1", 0.0),
            win("com.a", "a2", 300.0),
            win("com.b", "b", 600.0),
        ],
    );
    let pid = t.r.wins[t.r.by_handle[&h[0]]].w.pid;
    t.event(PlatformEvent::AppTerminated(pid));
    assert_eq!(t.order(), ["b"]);
    assert_eq!(t.frame(h[2]), r(0.0, 0.0, W, H));
    assert!(t.calls().contains(&FakeCall::Unobserve(pid)));
}

// ---- workspaces ------------------------------------------------------------

#[test]
fn switching_parks_on_the_free_corner_and_returning_unparks() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0), mon("m2", W)],
        &[win("com.a", "a", 0.0), win("com.b", "b", 400.0)],
    );
    t.focus(h[0]);
    t.ok("workspace 2");
    // m2 is to the right, so m1 parks bottom-left: 1pt left on screen.
    assert_eq!(t.frame(h[0]), r(1.0 - 500.0, H - 1.0, 500.0, H));
    assert_eq!(t.frame(h[1]), r(1.0 - 500.0, H - 1.0, 500.0, H));
    assert_eq!(t.focused(), None);
    let ev = t.broadcasts();
    assert!(ev.contains(&IpcEvent::WorkspaceChanged {
        monitor: "m1".into(),
        workspace: "2".into(),
        prev: Some("1".into()),
    }));
    t.ok("workspace 1");
    assert_eq!(t.frame(h[0]), r(0.0, 0.0, 500.0, H));
    assert_eq!(t.frame(h[1]), r(500.0, 0.0, 500.0, H));
    assert_eq!(t.focused(), Some(h[0]));
}

#[test]
fn floating_window_gets_its_frame_back_after_parking() {
    let mut c = cfg();
    c.rules = vec![Rule {
        app_id: Some("com.f".into()),
        float: true,
        ..Rule::default()
    }];
    let (f, h) = fake_with(vec![mon("m1", 0.0)], &[win("com.f", "f", 200.0)]);
    let mut t = start(f, c, None);
    t.ok("workspace 2");
    assert!(is_parked(&t.frame(h[0]), &[r(0.0, 0.0, W, H)]));
    t.ok("workspace 1");
    assert_eq!(t.frame(h[0]), r(200.0, 100.0, 300.0, 300.0));
}

#[test]
fn per_monitor_model_shows_the_same_name_on_both_monitors() {
    let (mut t, _) = setup(vec![mon("m1", 0.0), mon("m2", W)], &[]);
    t.ok("workspace 2");
    t.ok("focus-monitor next");
    t.ok("workspace 2");
    assert_eq!(t.visible_name("m1"), "2");
    assert_eq!(t.visible_name("m2"), "2");
    let resp = t.run("list-workspaces --json");
    let rows = resp.data.unwrap();
    let visible: Vec<_> = rows
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["visible"] == true)
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(visible, ["2", "2"]);
}

#[test]
fn shared_model_switch_to_a_workspace_shown_elsewhere_moves_focus_only() {
    let mut c = cfg();
    c.workspaces.model = tatami_config::WorkspaceModel::Shared;
    let (f, h) = fake_with(
        vec![mon("m1", 0.0), mon("m2", W)],
        &[win("com.a", "a", 0.0), win("com.b", "b", 1200.0)],
    );
    let mut t = start(f, c, None);
    assert_eq!(t.visible_name("m1"), "1");
    assert_eq!(t.visible_name("m2"), "2");
    t.focus(h[0]);
    t.calls();
    t.ok("workspace 2");
    assert_eq!(t.visible_name("m1"), "1");
    assert_eq!(t.r.focused_monitor, t.mon_id("m2"));
    assert_eq!(t.focused(), Some(h[1]));
    let calls = t.calls();
    assert!(set_frames(&calls, h[0]).is_empty(), "nothing parked");
}

#[test]
fn summon_workspace_needs_the_shared_model() {
    let (mut t, _) = setup(vec![mon("m1", 0.0)], &[]);
    let resp = t.run("summon-workspace 2");
    assert!(!resp.ok && resp.error.contains("shared"));
}

#[test]
fn move_node_creates_the_workspace_and_gc_drops_it_when_left_empty() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[win("com.a", "a", 0.0), win("com.b", "b", 400.0)],
    );
    t.focus(h[0]);
    let m1 = t.mon_id("m1");
    t.ok("move-node-to-workspace 5");
    assert!(t.r.model.find(m1, "5").is_some());
    assert!(is_parked(&t.frame(h[0]), &[r(0.0, 0.0, W, H)]));
    assert_eq!(t.frame(h[1]), r(0.0, 0.0, W, H));
    assert_eq!(
        t.focused(),
        Some(h[1]),
        "focus stays on the source workspace"
    );
    t.ok("workspace 5");
    assert_eq!(t.frame(h[0]), r(0.0, 0.0, W, H));
    t.ok("move-node-to-workspace 1 --focus-follows-window");
    assert_eq!(t.visible_name("m1"), "1");
    assert!(t.r.model.find(m1, "5").is_none(), "empty 5 collected");
    assert_eq!(t.focused(), Some(h[0]));
}

#[test]
fn back_and_forth_returns_to_the_previous_workspace() {
    let (mut t, _) = setup(vec![mon("m1", 0.0)], &[win("com.a", "a", 0.0)]);
    t.ok("workspace 4");
    t.ok("workspace-back-and-forth");
    assert_eq!(t.visible_name("m1"), "1");
}

#[test]
fn focusing_a_parked_window_switches_to_its_workspace() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[win("com.a", "a", 0.0), win("com.b", "b", 400.0)],
    );
    t.focus(h[0]);
    t.ok("move-node-to-workspace 2");
    t.focus(h[0]);
    assert_eq!(t.visible_name("m1"), "2");
    assert_eq!(t.frame(h[0]), r(0.0, 0.0, W, H));
}

// ---- focus and move -------------------------------------------------------

#[test]
fn focus_moves_by_geometry() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[win("com.a", "a", 0.0), win("com.b", "b", 400.0)],
    );
    t.focus(h[1]);
    t.ok("focus left");
    assert_eq!(t.focused(), Some(h[0]));
    assert!(t.calls().contains(&FakeCall::Raise(h[0])));
    t.ok("focus right");
    assert_eq!(t.focused(), Some(h[1]));
    assert!(!t.run("focus right").ok, "nothing further right");
}

#[test]
fn focus_crosses_to_the_next_monitor_at_the_edge() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0), mon("m2", W)],
        &[win("com.a", "a", 0.0), win("com.c", "c", 1200.0)],
    );
    t.focus(h[0]);
    t.ok("focus right");
    assert_eq!(t.focused(), Some(h[1]));
    assert_eq!(t.r.focused_monitor, t.mon_id("m2"));
}

#[test]
fn focus_wraps_around_within_the_workspace() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[
            win("com.a", "a", 0.0),
            win("com.a", "b", 300.0),
            win("com.a", "c", 600.0),
        ],
    );
    t.focus(h[2]);
    t.ok("focus right --wrap-around");
    assert_eq!(t.focused(), Some(h[0]));
}

#[test]
fn focus_back_and_forth_and_dfs() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[
            win("com.a", "a", 0.0),
            win("com.a", "b", 300.0),
            win("com.a", "c", 600.0),
        ],
    );
    t.focus(h[0]);
    t.ok("focus dfs-next");
    assert_eq!(t.focused(), Some(h[1]));
    t.ok("focus-back-and-forth");
    assert_eq!(t.focused(), Some(h[0]));
}

#[test]
fn move_at_the_root_edge_goes_to_the_next_monitor() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0), mon("m2", W)],
        &[
            win("com.a", "a", 0.0),
            win("com.a", "b", 400.0),
            win("com.c", "c", 1200.0),
        ],
    );
    t.focus(h[1]);
    t.ok("move right");
    assert_eq!(t.frame(h[0]), r(0.0, 0.0, W, H));
    assert_eq!(t.frame(h[2]), r(W, 0.0, 500.0, H));
    assert_eq!(t.frame(h[1]), r(1500.0, 0.0, 500.0, H));
    assert_eq!(t.focused(), Some(h[1]));
    assert_eq!(t.r.focused_monitor, t.mon_id("m2"));
}

#[test]
fn move_inside_the_tree_swaps_neighbours() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[win("com.a", "a", 0.0), win("com.a", "b", 400.0)],
    );
    t.focus(h[0]);
    t.ok("move right");
    assert_eq!(t.order(), ["b", "a"]);
    assert_eq!(t.frame(h[0]), r(500.0, 0.0, 500.0, H));
}

// ---- layout commands ---------------------------------------------------------

#[test]
fn layout_toggles_floating_and_tiling() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[win("com.a", "a", 0.0), win("com.b", "b", 400.0)],
    );
    t.focus(h[0]);
    t.ok("layout floating tiling");
    let id = t.r.by_handle[&h[0]];
    assert_eq!(t.r.wins[id].w.kind, WindowKind::Floating);
    assert_eq!(t.frame(h[1]), r(0.0, 0.0, W, H));
    t.ok("layout floating tiling");
    assert_eq!(t.r.wins[id].w.kind, WindowKind::Tiled);
    assert_eq!(t.frame(h[1]).w, 500.0);
}

#[test]
fn layout_cycles_to_the_first_argument_that_changes_something() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[win("com.a", "a", 0.0), win("com.b", "b", 400.0)],
    );
    t.focus(h[0]);
    t.ok("layout tiles horizontal vertical");
    assert_eq!(t.frame(h[0]), r(0.0, 0.0, W, 400.0));
    assert_eq!(t.frame(h[1]), r(0.0, 400.0, W, 400.0));
    t.ok("layout tiles horizontal vertical");
    assert_eq!(t.frame(h[1]), r(500.0, 0.0, 500.0, H));
}

#[test]
fn accordion_raises_back_to_front_with_the_focused_window_last() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[
            win("com.a", "a", 0.0),
            win("com.a", "b", 300.0),
            win("com.a", "c", 600.0),
        ],
    );
    t.focus(h[0]);
    t.calls();
    t.ok("layout h_accordion");
    let raises: Vec<WindowHandle> = t
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            FakeCall::Raise(w) => Some(w),
            _ => None,
        })
        .collect();
    assert_eq!(raises, [h[2], h[1], h[0]]);
    assert_eq!(
        t.focused(),
        Some(h[0]),
        "echoes of the z-order raises ignored"
    );
    assert_eq!(t.frame(h[0]), r(0.0, 0.0, 970.0, H));
}

#[test]
fn fullscreen_fills_the_visible_rect_and_toggles_back() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[win("com.a", "a", 0.0), win("com.b", "b", 400.0)],
    );
    t.focus(h[0]);
    t.ok("fullscreen");
    assert_eq!(t.frame(h[0]), r(0.0, 0.0, W, H));
    assert_eq!(
        t.frame(h[1]),
        r(500.0, 0.0, 500.0, H),
        "still laid out behind"
    );
    t.ok("fullscreen off");
    assert_eq!(t.frame(h[0]), r(0.0, 0.0, 500.0, H));
}

#[test]
fn resize_width_and_set() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[win("com.a", "a", 0.0), win("com.b", "b", 400.0)],
    );
    t.focus(h[0]);
    t.ok("resize width +100");
    assert_eq!(t.frame(h[0]), r(0.0, 0.0, 600.0, H));
    t.ok("resize smart 300");
    assert_eq!(t.frame(h[0]), r(0.0, 0.0, 300.0, H));
    t.ok("balance-sizes");
    assert_eq!(t.frame(h[0]).w, 500.0);
}

#[test]
fn join_and_flatten() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[
            win("com.a", "a", 0.0),
            win("com.a", "b", 300.0),
            win("com.a", "c", 600.0),
        ],
    );
    t.focus(h[2]);
    t.ok("join-with left");
    assert_eq!(t.frame(h[1]), r(500.0, 0.0, 500.0, 400.0));
    assert_eq!(t.frame(h[2]), r(500.0, 400.0, 500.0, 400.0));
    t.ok("flatten-workspace-tree");
    assert_eq!(t.frame(h[2]).h, H);
}

// ---- keys ---------------------------------------------------------------------

#[test]
fn alt_h_is_swallowed_and_focus_moves_on_drain() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[win("com.a", "a", 0.0), win("com.b", "b", 400.0)],
    );
    t.focus(h[1]);
    t.calls();
    t.key(RawKeyEvent::Flags { mods: alt() });
    assert_eq!(t.key(down(K_H, alt())), KeyDisposition::Swallow);
    assert!(
        t.calls().is_empty(),
        "the tap callback never touches the platform"
    );
    assert_eq!(t.focused(), Some(h[1]));
    t.drain();
    assert_eq!(t.focused(), Some(h[0]));
}

#[test]
fn unbound_alt_x_passes_through() {
    let (mut t, _) = setup(vec![mon("m1", 0.0)], &[]);
    assert_eq!(t.key(down(K_X, alt())), KeyDisposition::Pass);
    assert_eq!(t.key(down(K_H, Mods::default())), KeyDisposition::Pass);
}

#[test]
fn alt_w_then_3_switches_workspace_and_leaves_the_mode() {
    let (mut t, _) = setup(vec![mon("m1", 0.0)], &[win("com.a", "a", 0.0)]);
    t.key(RawKeyEvent::Flags { mods: alt() });
    assert_eq!(t.key(down(K_W, alt())), KeyDisposition::Swallow);
    t.drain();
    assert_eq!(t.r.keys.mode(), Some("workspaces"));
    assert!(
        t.calls()
            .contains(&FakeCall::Status("[1] WORKSPACES".into()))
    );
    assert_eq!(t.key(down(K_3, alt())), KeyDisposition::Swallow);
    t.drain();
    assert_eq!(t.visible_name("m1"), "3");
    assert_eq!(t.r.keys.mode(), None);
    assert!(t.calls().contains(&FakeCall::Status("[3]".into())));
    let ev = t.broadcasts();
    assert!(ev.contains(&IpcEvent::ModeChanged {
        mode: Some("workspaces".into())
    }));
    assert!(ev.contains(&IpcEvent::ModeChanged { mode: None }));
}

#[test]
fn alt_r_is_sticky_and_h_resizes_each_time() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[win("com.a", "a", 0.0), win("com.b", "b", 400.0)],
    );
    t.focus(h[1]);
    t.key(down(K_R, alt()));
    t.key(RawKeyEvent::Flags {
        mods: Mods::default(),
    });
    assert_eq!(t.key(down(K_H, Mods::default())), KeyDisposition::Swallow);
    assert_eq!(t.key(down(K_H, Mods::default())), KeyDisposition::Swallow);
    t.drain();
    assert_eq!(t.r.keys.mode(), Some("resize"));
    assert_eq!(t.frame(h[1]), r(600.0, 0.0, 400.0, H));
}

#[test]
fn holding_alt_shows_the_sheet_on_tick_and_release_hides_it() {
    let (mut t, _) = setup(vec![mon("m1", 0.0)], &[]);
    t.key(RawKeyEvent::Flags { mods: alt() });
    t.drain();
    assert!(!t.calls().iter().any(|c| matches!(c, FakeCall::Overlay(_))));
    let due = t.r.next_due().expect("which-key clock armed");
    t.now = due;
    let o = t.r.on_timer(due);
    t.out.extend(o);
    assert!(t.calls().contains(&FakeCall::Overlay(Some("alt".into()))));
    let c = t.r.overlay_content(&tatami_keymap::Level::Top);
    assert_eq!(c.exit_hint, "release alt  close");
    assert!(
        c.rows
            .iter()
            .any(|r| r.key == "h" && r.desc == "focus left")
    );
    assert!(c.rows.iter().any(|r| r.desc == "+workspaces" && r.prefix));
    t.key(RawKeyEvent::Flags {
        mods: Mods::default(),
    });
    t.drain();
    assert!(t.calls().contains(&FakeCall::Overlay(None)));
}

#[test]
fn mode_sheet_title_names_the_path() {
    let (t, _) = setup(vec![mon("m1", 0.0)], &[]);
    let c =
        t.r.overlay_content(&tatami_keymap::Level::Mode("workspaces".into()));
    assert_eq!(c.title, "alt w  workspaces");
    assert_eq!(c.exit_hint, "Esc  back");
}

#[test]
fn trigger_binding_runs_the_bound_actions() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[win("com.a", "a", 0.0), win("com.b", "b", 400.0)],
    );
    t.focus(h[1]);
    t.ok("trigger-binding alt+h");
    assert_eq!(t.focused(), Some(h[0]));
}

// ---- restore -----------------------------------------------------------------

fn saved_win(bundle: &str, title: &str) -> SavedWindow {
    SavedWindow {
        bundle_id: Some(bundle.into()),
        app: bundle.replace("com.", ""),
        title: title.into(),
        subrole: "AXStandardWindow".into(),
        pid: None,
        cg_id: None,
        size: None,
    }
}

fn saved_two(first: SavedWindow, second: SavedWindow) -> SavedState {
    SavedState {
        version: 1,
        saved_at: String::new(),
        model: "per-monitor".into(),
        focused_monitor: Some("m1".into()),
        monitors: vec![SavedMonitor {
            uuid: "m1".into(),
            name: String::new(),
            frame: r(0.0, 0.0, W, H),
            active: "1".into(),
            workspaces: vec![SavedWorkspace {
                name: "1".into(),
                root: SavedNode::Container {
                    layout: Layout::HTiles,
                    weight: 1.0,
                    children: vec![
                        SavedNode::Window {
                            weight: 1.0,
                            window: first,
                        },
                        SavedNode::Window {
                            weight: 1.0,
                            window: second,
                        },
                    ],
                },
                floating: Vec::new(),
                focused: Some(0),
            }],
        }],
    }
}

#[test]
fn restore_launches_the_missing_app_and_fills_its_slot_in_order() {
    let (f, h) = fake_with(vec![mon("m1", 0.0)], &[win("com.term", "shell", 0.0)]);
    let saved = saved_two(
        saved_win("com.mail", "Inbox"),
        saved_win("com.term", "shell"),
    );
    let mut t = start(f, cfg(), Some(saved));
    // `start` drained the calls; the launch happened inside `new`.
    assert_eq!(t.order(), ["?", "shell"]);
    assert_eq!(
        t.frame(h[0]),
        r(500.0, 0.0, 500.0, H),
        "right slot, left reserved"
    );
    assert!(t.r.restore.is_some());
    let pid = t.fake().add_app("com.mail", "mail");
    let m = t.fake().add_window(pid, "Inbox", r(0.0, 0.0, 200.0, 200.0));
    t.settle();
    assert_eq!(t.order(), ["Inbox", "shell"]);
    assert_eq!(t.frame(m), r(0.0, 0.0, 500.0, H));
    assert!(t.r.restore.is_none(), "session over");
    assert!(t.broadcasts().contains(&IpcEvent::Restore {
        pending: 0,
        total: 2
    }));
}

#[test]
fn restore_calls_launch_once_for_a_missing_app() {
    let (f, _) = fake_with(vec![mon("m1", 0.0)], &[win("com.term", "shell", 0.0)]);
    let saved = saved_two(
        saved_win("com.mail", "Inbox"),
        saved_win("com.mail", "Drafts"),
    );
    let now = Instant::now();
    let mut r = Reactor::new(f, cfg(), Some(saved), now);
    let launches: Vec<FakeCall> = r
        .platform()
        .take_calls()
        .into_iter()
        .filter(|c| matches!(c, FakeCall::Launch(_)))
        .collect();
    assert_eq!(launches, [FakeCall::Launch("com.mail".into())]);
}

#[test]
fn restore_deadline_prunes_the_placeholder() {
    let (f, h) = fake_with(vec![mon("m1", 0.0)], &[win("com.term", "shell", 0.0)]);
    let saved = saved_two(
        saved_win("com.mail", "Inbox"),
        saved_win("com.term", "shell"),
    );
    let mut t = start(f, cfg(), Some(saved));
    assert_eq!(t.frame(h[0]).w, 500.0);
    t.now += Duration::from_secs(61);
    let o = t.r.on_timer(t.now);
    t.out.extend(o);
    t.settle();
    assert_eq!(t.order(), ["shell"]);
    assert_eq!(t.frame(h[0]), r(0.0, 0.0, W, H));
    assert!(t.r.restore.is_none());
}

#[test]
fn restore_ignores_a_state_from_another_model() {
    let (f, _) = fake_with(vec![mon("m1", 0.0)], &[win("com.term", "shell", 0.0)]);
    let mut saved = saved_two(saved_win("com.mail", "Inbox"), saved_win("com.term", "x"));
    saved.model = "shared".into();
    let t = start(f, cfg(), Some(saved));
    assert!(t.r.restore.is_none());
    assert_eq!(t.order(), ["shell"]);
}

fn three_with_nested() -> (T, Vec<WindowHandle>) {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[
            win("com.a", "a", 0.0),
            win("com.b", "b", 300.0),
            win("com.c", "c", 600.0),
        ],
    );
    t.focus(h[2]);
    t.ok("join-with left");
    t.ok("resize height +100");
    (t, h)
}

fn shape(n: &SavedNode) -> String {
    match n {
        SavedNode::Window { weight, window } => format!("{}:{weight:.2}", window.title),
        SavedNode::Container {
            layout,
            weight,
            children,
        } => {
            let inner: Vec<String> = children.iter().map(shape).collect();
            format!("{layout}:{weight:.2}[{}]", inner.join(", "))
        }
    }
}

#[test]
fn snapshot_then_restore_reproduces_order_layouts_and_weights() {
    let (t, _) = three_with_nested();
    let snap = t.r.snapshot();
    let before = shape(&snap.monitors[0].workspaces[0].root);
    assert!(before.starts_with("h_tiles"), "{before}");
    assert!(before.contains("v_tiles"), "{before}");
    let (f, _) = fake_with(
        vec![mon("m1", 0.0)],
        &[
            win("com.c", "c", 600.0),
            win("com.b", "b", 300.0),
            win("com.a", "a", 0.0),
        ],
    );
    let t2 = start(f, cfg(), Some(snap));
    let after = shape(&t2.r.snapshot().monitors[0].workspaces[0].root);
    assert_eq!(after, before);
    assert!(t2.r.restore.is_none(), "every window matched");
}

#[test]
fn a_restore_waits_for_trust_before_giving_up() {
    // A saved layout with a window of an app that is not running, loaded
    // by a daemon without Accessibility: the slot must wait for trust,
    // then get the full timeout.
    let (t, _) = setup(vec![mon("m1", 0.0)], &[win("com.a", "a", 0.0)]);
    let mut saved = t.r.snapshot();
    let SavedNode::Container { children, .. } = &mut saved.monitors[0].workspaces[0].root else {
        panic!("root is a container");
    };
    let mut ghost = children[0].clone();
    if let SavedNode::Window { window, .. } = &mut ghost {
        window.bundle_id = Some("com.gone".into());
        window.app = "Gone".into();
        window.pid = None;
        window.cg_id = None;
    }
    children.push(ghost);
    let (f, _) = fake_with(vec![mon("m1", 0.0)], &[win("com.a", "a", 0.0)]);
    let mut t = start(f, cfg(), Some(saved));
    assert!(t.r.restore.is_some(), "the ghost's slot is pending");
    t.event(PlatformEvent::TrustChanged(false));
    t.now += Duration::from_secs(120);
    let o = t.r.on_timer(t.now);
    t.out.extend(o);
    t.settle();
    assert!(
        t.r.restore.is_some(),
        "untrusted: nothing seen yet, keep waiting"
    );
    t.event(PlatformEvent::TrustChanged(true));
    t.now += Duration::from_secs(30);
    let o = t.r.on_timer(t.now);
    t.out.extend(o);
    t.settle();
    assert!(t.r.restore.is_some(), "the clock started at trust");
    t.now += Duration::from_secs(40);
    let o = t.r.on_timer(t.now);
    t.out.extend(o);
    t.settle();
    assert!(t.r.restore.is_none(), "given up after the full timeout");
}

/// `(workspace name, window titles in tree order)` per workspace of the
/// first monitor.
fn titles_by_workspace(saved: &SavedState) -> Vec<(String, Vec<String>)> {
    fn walk(n: &SavedNode, out: &mut Vec<String>) {
        match n {
            SavedNode::Window { window, .. } => out.push(window.title.clone()),
            SavedNode::Container { children, .. } => children.iter().for_each(|c| walk(c, out)),
        }
    }
    saved.monitors[0]
        .workspaces
        .iter()
        .map(|w| {
            let mut t = Vec::new();
            walk(&w.root, &mut t);
            (w.name.clone(), t)
        })
        .collect()
}

#[test]
fn trust_arriving_puts_the_present_windows_into_their_saved_slots() {
    // A daemon that starts without Accessibility sees no window. When
    // trust arrives, the rescan must fill the saved slots with the windows
    // that were there all along, not open a workspace per window and let
    // the slots time out.
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[
            win("com.a", "a", 0.0),
            win("com.b", "b", 400.0),
            win("com.c", "c", 800.0),
        ],
    );
    t.focus(h[2]);
    t.ok("move-node-to-workspace 2");
    let saved = t.r.snapshot();
    let want = titles_by_workspace(&saved);
    assert_eq!(
        want,
        vec![
            ("1".to_string(), vec!["a".to_string(), "b".to_string()]),
            ("2".to_string(), vec!["c".to_string()]),
        ]
    );

    let (f, _) = fake_with(vec![mon("m1", 0.0)], &[]);
    let mut t = start(f, cfg_own(), Some(saved));
    t.event(PlatformEvent::TrustChanged(false));
    assert!(t.r.restore.is_some(), "every slot is pending");
    // The windows exist; untrusted, nothing reported them.
    for (bundle, title, x) in [
        ("com.a", "a", 0.0),
        ("com.b", "b", 400.0),
        ("com.c", "c", 800.0),
    ] {
        let pid = t.fake().add_app(bundle, title);
        t.fake().add_window(pid, title, r(x, 100.0, 300.0, 300.0));
    }
    t.fake().take_events();
    t.event(PlatformEvent::TrustChanged(true));
    assert!(t.r.restore.is_none(), "every slot filled at trust");
    assert_eq!(titles_by_workspace(&t.r.snapshot()), want);
}

#[test]
fn live_restore_reslots_existing_windows() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[win("com.a", "a", 0.0), win("com.b", "b", 400.0)],
    );
    let snap = t.r.snapshot();
    t.focus(h[0]);
    t.ok("move right");
    assert_eq!(t.order(), ["b", "a"]);
    let o = t.r.restore(snap, t.now);
    t.out.extend(o);
    t.settle();
    assert_eq!(t.order(), ["a", "b"]);
    assert_eq!(t.frame(h[0]), r(0.0, 0.0, 500.0, H));
}

#[test]
fn snapshot_records_active_focus_and_floating() {
    let mut c = cfg();
    c.rules = vec![Rule {
        app_id: Some("com.f".into()),
        float: true,
        ..Rule::default()
    }];
    let (f, h) = fake_with(
        vec![mon("m1", 0.0)],
        &[win("com.a", "a", 0.0), win("com.f", "f", 200.0)],
    );
    let mut t = start(f, c, None);
    t.focus(h[0]);
    let s = t.r.snapshot();
    let m = &s.monitors[0];
    assert_eq!(m.active, "1");
    let ws = &m.workspaces[0];
    assert_eq!(ws.focused, Some(0));
    assert_eq!(ws.floating[0].frame, r(200.0, 100.0, 300.0, 300.0));
    assert_eq!(ws.floating[0].window.title, "f");
    assert_eq!(s.focused_monitor.as_deref(), Some("m1"));
}

// ---- config, lifecycle, requests ------------------------------------------------

#[test]
fn reload_config_applies_new_gaps() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[win("com.a", "a", 0.0), win("com.b", "b", 400.0)],
    );
    let mut c = cfg();
    c.gaps.inner = 20;
    let o = t.r.reload_config(c, t.now);
    t.out.extend(o);
    t.settle();
    assert_eq!(t.frame(h[0]), r(0.0, 0.0, 490.0, H));
    assert_eq!(t.frame(h[1]), r(510.0, 0.0, 490.0, H));
}

#[test]
fn shutdown_unparks_everything_and_clears_the_ui() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[win("com.a", "a", 0.0), win("com.b", "b", 400.0)],
    );
    t.ok("workspace 2");
    let mons = [r(0.0, 0.0, W, H)];
    assert!(is_parked(&t.frame(h[0]), &mons));
    t.calls();
    t.r.shutdown();
    assert!(!is_parked(&t.frame(h[0]), &mons));
    assert!(!is_parked(&t.frame(h[1]), &mons));
    let calls = t.calls();
    assert!(calls.contains(&FakeCall::Overlay(None)));
    assert!(calls.contains(&FakeCall::Status(String::new())));
}

#[test]
fn enable_off_releases_windows_and_on_takes_them_back() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[win("com.a", "a", 0.0), win("com.b", "b", 400.0)],
    );
    t.ok("workspace 2");
    t.ok("enable off");
    let mons = [r(0.0, 0.0, W, H)];
    assert!(!is_parked(&t.frame(h[0]), &mons));
    t.fake().user_set_frame(h[1], r(5.0, 5.0, 100.0, 100.0));
    t.settle();
    assert_eq!(t.frame(h[1]), r(5.0, 5.0, 100.0, 100.0), "not managed");
    t.ok("enable on");
    assert!(is_parked(&t.frame(h[0]), &mons), "workspace 2 still shown");
}

#[test]
fn removed_monitor_merges_its_workspace_into_the_remaining_one() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0), mon("m2", W)],
        &[win("com.a", "a", 0.0), win("com.c", "c", 1200.0)],
    );
    t.fake().set_screens(vec![mon("m1", 0.0)]);
    t.settle();
    assert_eq!(t.r.order.len(), 1);
    assert_eq!(t.frame(h[0]), r(0.0, 0.0, 500.0, H));
    assert_eq!(t.frame(h[1]), r(500.0, 0.0, 500.0, H));
    assert!(t.broadcasts().contains(&IpcEvent::MonitorsChanged {
        monitors: vec!["m1".into()]
    }));
}

#[test]
fn unplugging_a_monitor_files_the_desk_and_plugging_it_back_restores_it() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0), mon("m2", W)],
        &[win("com.a", "a", 0.0), win("com.c", "c", 1200.0)],
    );
    let home = tatami_state::desk_id_of(&["m1", "m2"]);
    let work = tatami_state::desk_id_of(&["m1"]);
    assert_eq!(t.r.desk_id(), home);
    assert_eq!(t.r.desk_name(), "2 monitors: Display m1, Display m2");

    // Undock: the layout as it was, m2 included, goes to the daemon.
    t.fake().set_screens(vec![mon("m1", 0.0)]);
    t.settle();
    let filed = t
        .out
        .iter()
        .find_map(|o| match o {
            Outcome::DeskChanged { from, saved, to } if from == &home && to == &work => {
                Some(saved.clone())
            }
            _ => None,
        })
        .expect("desk change filed");
    assert_eq!(filed.monitors.len(), 2);
    assert_eq!(filed.desk_id(), home);
    assert_eq!(t.frame(h[1]), r(500.0, 0.0, 500.0, H), "c folded onto m1");
    t.out.clear();

    // Dock again: the daemon is told, and hands the filed layout back.
    t.fake().set_screens(vec![mon("m1", 0.0), mon("m2", W)]);
    t.settle();
    assert!(t.out.iter().any(|o| matches!(
        o,
        Outcome::DeskChanged { from, to, .. } if from == &work && to == &home
    )));
    t.calls();
    let out = t.r.restore_desk(*filed, t.now);
    t.out.extend(out);
    t.settle();
    assert_eq!(t.frame(h[0]), r(0.0, 0.0, W, H), "a alone on m1 again");
    assert_eq!(t.frame(h[1]), r(W, 0.0, W, H), "c back on m2");
    assert!(
        !t.calls().iter().any(|c| matches!(c, FakeCall::Launch(_))),
        "a desk switch launches nothing"
    );
}

#[test]
fn a_desk_never_saved_gives_every_app_its_own_workspace() {
    // Unplugging to the laptop alone folds the other monitors' windows
    // onto it. With `new_window = "own"` each app then gets a workspace
    // to itself, its windows tiled together there, instead of every
    // window tiling into one; a desk that was saved is left to the
    // daemon to restore.
    let wins = || {
        [
            win("com.a", "a", 0.0),
            win("com.b", "b1", W),
            win("com.b", "b2", W + 400.0),
            win("com.c", "c", W + 800.0),
        ]
    };
    let (f, h) = fake_with(vec![mon("m1", 0.0), mon("m2", W)], &wins());
    let mut t = start(f, cfg_own(), None);
    let m1 = t.mon_id("m1");
    t.focus(h[0]);
    t.fake().set_screens(vec![mon("m1", 0.0)]);
    t.settle();
    let ws = |t: &T, i: usize| t.r.wins[t.r.by_handle[&h[i]]].ws;
    assert_eq!(ws(&t, 1), ws(&t, 2), "both b windows share a workspace");
    assert!(
        ws(&t, 0) != ws(&t, 1) && ws(&t, 1) != ws(&t, 3) && ws(&t, 0) != ws(&t, 3),
        "one workspace per app"
    );
    assert_eq!(
        t.r.model.visible_on(m1),
        Some(ws(&t, 0)),
        "the focused app shows"
    );
    assert_eq!(t.frame(h[0]), r(0.0, 0.0, W, H), "and fills the laptop");
    let name = t.r.ws_name(ws(&t, 1)).to_string();
    t.ok(&format!("workspace {name}"));
    assert_eq!(
        t.frame(h[1]),
        r(0.0, 0.0, 500.0, H),
        "b's windows tile together"
    );
    assert_eq!(t.frame(h[2]), r(500.0, 0.0, 500.0, H));

    let (f, h) = fake_with(vec![mon("m1", 0.0), mon("m2", W)], &wins());
    let mut t = start(f, cfg_own(), None);
    t.r.set_desks(vec![tatami_state::DeskInfo {
        id: tatami_state::desk_id_of(&["m1"]),
        name: "1 monitor: Display m1".into(),
        monitors: 1,
        saved_at: String::new(),
    }]);
    t.fake().set_screens(vec![mon("m1", 0.0)]);
    t.settle();
    assert_eq!(
        t.frame(h[0]),
        r(0.0, 0.0, 500.0, H),
        "a saved desk is only folded; the daemon restores it"
    );
}

#[test]
fn a_desk_restore_drops_slots_for_apps_that_are_gone_at_once() {
    let (mut t, h) = setup(vec![mon("m1", 0.0)], &[win("com.a", "a", 0.0)]);
    let mut saved = t.r.snapshot();
    // A second window, of an app that is not running, beside `a`.
    let SavedNode::Container { children, .. } = &mut saved.monitors[0].workspaces[0].root else {
        panic!("root is a container");
    };
    let mut ghost = children[0].clone();
    if let SavedNode::Window { window, .. } = &mut ghost {
        window.bundle_id = Some("com.gone".into());
        window.app = "Gone".into();
        window.pid = None;
        window.cg_id = None;
    }
    children.push(ghost);
    let out = t.r.restore_desk(saved, t.now);
    t.out.extend(out);
    t.settle();
    assert!(!t.calls().iter().any(|c| matches!(c, FakeCall::Launch(_))));
    assert!(t.r.restore.is_none(), "nothing waited for");
    assert_eq!(t.frame(h[0]), r(0.0, 0.0, W, H), "a has the screen again");
}

#[test]
fn the_menu_lists_profiles_desks_first_with_the_present_one_checked() {
    let (mut t, _h) = setup(vec![mon("m1", 0.0)], &[win("com.a", "a", 0.0)]);
    let menu = t.r.menu_items();
    assert!(menu.iter().any(|m| m.title == "Profiles"));
    assert!(
        menu.iter().any(|m| m.command == ["profile", "save-as"]),
        "Save profile… is always offered"
    );
    assert!(
        !menu.iter().any(|m| m.title == "Delete profile"),
        "nothing to delete yet"
    );
    t.r.set_profiles(vec!["work".into()]);
    let menu = t.r.menu_items();
    assert!(
        menu.iter()
            .any(|m| m.command == ["profile", "restore", "work"])
    );
    let del = menu.iter().find(|m| m.title == "Delete profile").unwrap();
    assert_eq!(del.children[0].command, ["profile", "delete", "work"]);
    // "Save profile…" asks for the name on the sheet; typed keys are the
    // name, Enter saves under it, and the sheet goes away.
    t.ok("profile save-as");
    assert!(t.r.prompt.is_some());
    let down = |code: u16| RawKeyEvent::Down {
        keycode: code,
        mods: Mods::default(),
        repeat: false,
    };
    for k in [13, 31, 15, 40] {
        assert_eq!(t.key(down(k)), KeyDisposition::Swallow); // w o r k
    }
    assert_eq!(t.r.prompt.as_ref().unwrap().text, "work");
    assert_eq!(t.key(down(36)), KeyDisposition::Swallow); // Enter
    t.drain();
    assert!(t.r.prompt.is_none());
    assert!(t.out.iter().any(|o| matches!(
        o,
        Outcome::Profile(tatami_protocol::ProfileCmd::Save(Some(n))) if n == "work"
    )));
    // Esc drops it without a save.
    t.out.clear();
    t.ok("profile save-as");
    assert_eq!(t.key(down(13)), KeyDisposition::Swallow);
    assert_eq!(t.key(down(53)), KeyDisposition::Swallow); // Esc
    t.drain();
    assert!(t.r.prompt.is_none());
    assert!(!t.out.iter().any(|o| matches!(o, Outcome::Profile(_))));
    let here = t.r.desk_id();
    t.r.set_desks(vec![
        tatami_state::DeskInfo {
            id: here.clone(),
            name: "1 monitor: Display m1".into(),
            monitors: 1,
            saved_at: String::new(),
        },
        tatami_state::DeskInfo {
            id: "0123456789abcdef".into(),
            name: "3 monitors: A, B, C".into(),
            monitors: 3,
            saved_at: String::new(),
        },
    ]);
    let menu = t.r.menu_items();
    let mine = menu
        .iter()
        .find(|m| m.command == ["desk", "restore", &here])
        .expect("present desk listed");
    assert!(mine.checked);
    let other = menu
        .iter()
        .find(|m| m.command == ["desk", "restore", "0123456789abcdef"])
        .unwrap();
    assert!(!other.checked && other.title == "Default · 3 monitors: A, B, C");
    let args: Vec<String> = vec!["desk".into(), "list".into()];
    let (resp, out) = t.r.handle_request(&args, t.now);
    assert!(resp.ok && out.contains(&Outcome::Desk(tatami_protocol::DeskCmd::List)));
}

#[test]
fn list_windows_json_returns_data() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[win("com.a", "a", 0.0), win("com.b", "b", 400.0)],
    );
    t.focus(h[1]);
    let resp = t.run("list-windows --json");
    assert!(resp.ok);
    let rows = resp.data.expect("json data");
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["handle"], h[1].0);
    assert_eq!(rows[1]["focused"], true);
    assert_eq!(rows[0]["workspace"], "1");
    let text = t.run("list-windows").stdout;
    assert!(text.lines().next().unwrap().contains("ws=1"), "{text}");
}

#[test]
fn requests_ping_quit_exec_and_daemon_words() {
    let (mut t, _) = setup(vec![mon("m1", 0.0)], &[win("com.a", "a", 0.0)]);
    let ping = t.run("ping");
    assert!(ping.stdout.starts_with("pong: tatamid"), "{}", ping.stdout);
    assert_eq!(ping.data.unwrap()["windows"], 1);
    let args = |s: &str| -> Vec<String> { s.split_whitespace().map(String::from).collect() };
    let (resp, out) = t.r.handle_request(&args("quit"), t.now);
    assert!(resp.ok && out.contains(&Outcome::Exit));
    // `hot-swap` is the daemon's to do; the reactor only asks, and the
    // menu offers it.
    let (resp, out) = t.r.handle_request(&args("hot-swap"), t.now);
    assert!(resp.ok && out.contains(&Outcome::HotSwap), "{resp:?}");
    // `edit-config` carries the configured editor; `start-at-login toggle`
    // asks for the opposite of what the daemon last reported, and the
    // menu shows the daemon's word.
    let (resp, out) = t.r.handle_request(&args("edit-config"), t.now);
    assert!(
        resp.ok && out.contains(&Outcome::EditConfig(String::new())),
        "{resp:?}"
    );
    let (resp, out) = t.r.handle_request(&args("start-at-login"), t.now);
    assert!(
        resp.ok && out.contains(&Outcome::StartAtLogin(true)),
        "{resp:?}"
    );
    assert!(!t.r.start_at_login(), "only the daemon flips it");
    t.r.set_start_at_login(true);
    let login =
        t.r.menu_items()
            .into_iter()
            .find(|m| m.command == ["start-at-login", "toggle"])
            .unwrap();
    assert!(login.checked);
    let (_, out) = t.r.handle_request(&args("start-at-login"), t.now);
    assert!(out.contains(&Outcome::StartAtLogin(false)));
    assert!(
        t.r.menu_items().iter().any(|m| m.command == ["hot-swap"]),
        "menu offers the swap"
    );
    let (_, out) =
        t.r.handle_request(&args("exec-and-forget open -a Mail"), t.now);
    assert!(out.contains(&Outcome::Exec("open -a Mail".into())));
    let (_, out) = t.r.handle_request(&args("reload-config"), t.now);
    assert!(out.contains(&Outcome::ReloadConfig));
    let (resp, out) = t.r.handle_request(&args("profile save"), t.now);
    assert!(resp.ok, "{resp:?}");
    assert!(out.contains(&Outcome::Profile(tatami_protocol::ProfileCmd::Save(None))));
    let (resp, _) = t.r.handle_request(&args("frobnicate"), t.now);
    assert!(!resp.ok);
    let (res, _) = t.r.run_actions(
        &[
            tatami_protocol::Action::Version,
            tatami_protocol::Action::Ping,
        ],
        t.now,
    );
    assert!(
        res.as_ref()
            .unwrap()
            .starts_with(&format!("{}\npong", env!("CARGO_PKG_VERSION"))),
        "{res:?}"
    );
}

#[test]
fn trust_loss_resets_key_state() {
    let (mut t, _) = setup(vec![mon("m1", 0.0)], &[]);
    t.key(down(K_W, alt()));
    t.drain();
    assert_eq!(t.r.keys.mode(), Some("workspaces"));
    t.event(PlatformEvent::TrustChanged(false));
    assert_eq!(t.r.keys.mode(), None);
    assert!(!t.r.trusted());
}

#[test]
fn leaf_bookkeeping_survives_a_busy_session() {
    let (mut t, h) = three_with_nested();
    t.ok("move-node-to-workspace 2");
    t.ok("workspace 2");
    t.ok("move-node-to-workspace 1 --focus-follows-window");
    t.focus(h[0]);
    t.ok("layout floating");
    t.ok("layout tiling");
    for (id, m) in &t.r.wins {
        if let Some(l) = m.leaf {
            assert!(matches!(
                t.r.tree.node(l).kind,
                NodeKind::Leaf(Leaf::Window(w)) if w == id
            ));
        }
    }
    let windows: usize =
        t.r.workspaces
            .values()
            .map(|w| t.r.tree.leaves(w.root).len())
            .sum();
    assert_eq!(windows, 3);
}

#[test]
fn move_workspace_to_monitor_merges_on_a_name_clash() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0), mon("m2", W)],
        &[win("com.a", "a", 0.0), win("com.c", "c", 1200.0)],
    );
    t.focus(h[0]);
    t.ok("move-workspace-to-monitor next");
    assert_eq!(t.frame(h[1]), r(W, 0.0, 500.0, H));
    assert_eq!(t.frame(h[0]), r(1500.0, 0.0, 500.0, H));
    let m1 = t.mon_id("m1");
    let fresh = t.r.model.visible_on(m1).unwrap();
    assert_eq!(t.visible_name("m1"), "1", "first free name on m1 again");
    assert!(t.r.tree.children(t.r.workspaces[fresh].root).is_empty());
    assert_eq!(t.r.focused_monitor, t.mon_id("m2"));
}

#[test]
fn native_fullscreen_leaves_the_tree_until_it_exits() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[win("com.a", "a", 0.0), win("com.b", "b", 400.0)],
    );
    t.fake()
        .set_native_fullscreen(h[0], true, r(0.0, 0.0, W, H));
    t.settle();
    let id = t.r.by_handle[&h[0]];
    assert_eq!(t.r.wins[id].w.kind, WindowKind::NativeFullscreen);
    assert_eq!(t.frame(h[1]), r(0.0, 0.0, W, H));
    assert_eq!(t.frame(h[0]), r(0.0, 0.0, W, H), "left alone");
    t.fake()
        .set_native_fullscreen(h[0], false, r(100.0, 100.0, 300.0, 300.0));
    t.settle();
    assert_eq!(t.r.wins[id].w.kind, WindowKind::Tiled);
    assert_eq!(t.order().len(), 2);
    assert_eq!(t.frame(h[1]).w, 500.0);
}

#[test]
fn flip_pairs_the_focused_window_with_its_neighbour_and_back() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[
            win("com.a", "a", 0.0),
            win("com.b", "b", 300.0),
            win("com.c", "c", 600.0),
        ],
    );
    t.focus(h[1]);
    t.ok("flip");
    let (a, b, c) = (t.frame(h[0]), t.frame(h[1]), t.frame(h[2]));
    assert_eq!(a.x, 0.0, "the untouched neighbour keeps its column");
    assert_eq!(b.x, c.x, "b and c share a column");
    assert!(b.y < c.y && b.h < H, "b is stacked above c");
    t.ok("flip");
    let (b, c) = (t.frame(h[1]), t.frame(h[2]));
    assert_eq!(b.y, c.y, "back side by side");
    assert!(
        b.x < c.x && (b.h - H).abs() < 1.0,
        "the pair merged into the row again"
    );
}

/// The user's 1920-wide monitor: four apps whose minimum widths add up to
/// more than the screen. Repeated `flip` must keep every window on the
/// monitor and must not thrash (bounded writes per flip).
#[test]
fn repeated_flip_with_min_width_windows_stays_on_screen_and_settles() {
    let m = MonitorInfo {
        uuid: "wide".into(),
        name: "wide".into(),
        frame: Rect::new(0.0, 0.0, 1920.0, 1080.0),
        visible: Rect::new(0.0, 0.0, 1920.0, 1080.0),
        is_main: true,
    };
    let (f, h) = fake_with(
        vec![m.clone()],
        &[
            win("com.bruno", "Bruno", 0.0),
            win("com.chrome", "Chrome", 480.0),
            win("com.claude", "Claude", 960.0),
            win("com.finder", "Finder", 1440.0),
        ],
    );
    for (i, w) in [700.0, 500.0, 600.0, 485.0].into_iter().enumerate() {
        f.set_behaviour(
            h[i],
            Behaviour {
                min_size: Some(Size { w, h: 0.0 }),
                ..Behaviour::default()
            },
        );
    }
    let mut t = start_logged(f, cfg(), None);
    t.calls();
    t.focus(h[1]);
    t.calls();
    for round in 0..6 {
        t.ok("flip");
        let calls = t.calls();
        let writes: Vec<usize> = h.iter().map(|&w| set_frames(&calls, w).len()).collect();
        let frames: Vec<Rect> = h.iter().map(|&w| t.frame(w)).collect();
        eprintln!("flip {round}: writes {writes:?}");
        for (w, r) in frames.iter().enumerate() {
            eprintln!("   {w}: {:.0},{:.0} {:.0}x{:.0}", r.x, r.y, r.w, r.h);
            assert!(
                r.x >= -1.0 && r.y >= -1.0 && r.right() <= 1921.0 && r.bottom() <= 1081.0,
                "flip {round}: window {w} off screen: {r:?}"
            );
        }
        assert!(
            writes.iter().all(|&n| n <= 3),
            "flip {round}: thrash {writes:?}"
        );
    }
}

#[test]
fn flip_twice_is_a_round_trip_for_the_weights() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[
            win("com.a", "a", 0.0),
            win("com.b", "b", 250.0),
            win("com.c", "c", 500.0),
            win("com.d", "d", 750.0),
        ],
    );
    t.focus(h[1]);
    t.ok("flip");
    let (a, b, d) = (t.frame(h[0]).w, t.frame(h[1]).w, t.frame(h[3]).w);
    assert!(
        (b - a).abs() <= 1.0 && (a - d).abs() <= 1.0,
        "the pair takes one ordinary column: {a} {b} {d}"
    );
    t.ok("flip");
    let ws: Vec<f64> = h.iter().map(|&w| t.frame(w).w).collect();
    assert!(
        ws.iter().all(|&w| (w - ws[0]).abs() <= 1.0),
        "equal columns again: {ws:?}"
    );
    assert!(ws[0] >= 249.0, "{ws:?}");
}

#[test]
fn a_fixed_size_window_floats_instead_of_being_tiled() {
    let (f, h) = fake_with(
        vec![mon("m1", 0.0)],
        &[
            win("com.a", "a", 0.0),
            win("com.apple.systempreferences", "Settings", 300.0),
        ],
    );
    f.set_resizable(h[1], false);
    let mut t = start(f, cfg(), None);
    t.ok("list-windows");
    assert_eq!(t.r.wins[t.r.by_handle[&h[1]]].w.kind, WindowKind::Floating);
    assert_eq!(t.frame(h[0]).w, W, "the tiled window has the whole monitor");
}

#[test]
fn dragging_a_tile_onto_another_monitor_moves_it_there() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0), mon("m2", W)],
        &[win("com.a", "a", 0.0), win("com.b", "b", 300.0)],
    );
    // The user drags `b` so its centre lands on the second monitor.
    t.fake()
        .user_set_frame(h[1], Rect::new(W + 100.0, 50.0, 400.0, 300.0));
    t.settle();
    let b = t.r.wins[t.r.by_handle[&h[1]]].ws;
    let m2 =
        t.r.monitors
            .iter()
            .find(|(_, m)| m.info.uuid == "m2")
            .map(|(i, _)| i)
            .unwrap();
    assert_eq!(t.r.ws_monitor(b), m2, "b now lives on the second monitor");
    assert_eq!(t.frame(h[0]).w, W, "a has the first monitor to itself");
    assert_eq!(t.frame(h[1]), Rect::new(W, 0.0, W, H), "b fills the second");
}

#[test]
fn dragging_a_tile_over_a_sibling_swaps_them() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[win("com.a", "a", 0.0), win("com.b", "b", 500.0)],
    );
    let a_before = t.frame(h[0]);
    // Drop `a` over `b`'s half.
    t.fake()
        .user_set_frame(h[0], Rect::new(600.0, 100.0, 300.0, 300.0));
    t.settle();
    assert_eq!(t.frame(h[1]), a_before, "b took a's slot");
    assert_eq!(t.frame(h[0]).x, W / 2.0, "a took b's slot");
}

#[test]
fn restore_keeps_a_rule_floated_window_floating_even_from_a_tiled_slot() {
    // Saved while Settings was still tiled next to the terminal.
    let (f, h) = fake_with(
        vec![mon("m1", 0.0)],
        &[
            win("com.term", "shell", 0.0),
            win("com.apple.systempreferences", "General", 500.0),
        ],
    );
    let saved = saved_two(
        saved_win("com.term", "shell"),
        saved_win("com.apple.systempreferences", "General"),
    );
    let mut t = start(f, cfg(), Some(saved));
    let settings = t.r.by_handle[&h[1]];
    assert_eq!(
        t.r.wins[settings].w.kind,
        WindowKind::Floating,
        "the built-in rule wins over the saved slot"
    );
    assert_eq!(
        t.frame(h[0]),
        r(0.0, 0.0, W, H),
        "the terminal has the monitor to itself"
    );
    assert!(t.r.restore.is_none(), "no placeholder left waiting");
}

#[test]
fn a_profile_save_from_a_key_is_handed_to_the_daemon() {
    let (mut t, _h) = setup(vec![mon("m1", 0.0)], &[win("com.a", "a", 0.0)]);
    let (res, out) = t.r.run_actions(
        &[tatami_protocol::Action::Profile(
            tatami_protocol::ProfileCmd::Save(Some("work".into())),
        )],
        t.now,
    );
    assert!(res.is_ok(), "{res:?}");
    assert!(out.iter().any(|o| matches!(
        o,
        Outcome::Profile(tatami_protocol::ProfileCmd::Save(Some(n))) if n == "work"
    )));
}

#[test]
fn a_floating_window_parked_by_a_dead_daemon_is_pulled_back() {
    // Parked one point inside the bottom-right corner, where a killed
    // daemon left it; fixed size, so it floats on adoption.
    let (f, h) = fake_with(
        vec![mon("m1", 0.0)],
        &[
            win("com.a", "a", 0.0),
            win("com.apple.calculator", "Calculator", 0.0),
        ],
    );
    f.set_resizable(h[1], false);
    f.user_set_frame(h[1], Rect::new(W - 1.0, H - 1.0, 300.0, 200.0));
    f.take_events();
    let mut t = start(f, cfg(), None);
    t.settle();
    let r = t.frame(h[1]);
    assert_eq!(t.r.wins[t.r.by_handle[&h[1]]].w.kind, WindowKind::Floating);
    assert!(
        r.x >= 0.0 && r.right() <= W && r.y >= 0.0 && r.bottom() <= H,
        "back on the monitor: {r:?}"
    );
}

#[test]
fn resize_keeps_growing_until_the_neighbour_hits_its_minimum() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[win("com.a", "a", 0.0), win("com.b", "b", 500.0)],
    );
    t.focus(h[0]);
    let mut last = t.frame(h[0]).w;
    let args: Vec<String> = ["resize", "width", "+100"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    for _ in 0..8 {
        // Past the sliver the command reports "cannot resize further"; the
        // width must never go down on the way there.
        let _ = t.r.handle_request(&args, t.now);
        t.settle();
        let w = t.frame(h[0]).w;
        assert!(w >= last, "shrank: {w} < {last}");
        last = w;
    }
    assert!(
        last >= W * 0.9,
        "a should reach nearly the whole row, got {last}"
    );
}

#[test]
fn resize_in_an_overflowing_row_still_grows_the_focused_tile() {
    // Four minimum widths that exceed the monitor: the row is in overlap
    // mode. Growing one tile must still make it wider (it overlaps more).
    let m = MonitorInfo {
        uuid: "wide".into(),
        name: "wide".into(),
        frame: Rect::new(0.0, 0.0, 1920.0, 1080.0),
        visible: Rect::new(0.0, 0.0, 1920.0, 1080.0),
        is_main: true,
    };
    let (f, h) = fake_with(
        vec![m],
        &[
            win("com.bruno", "Bruno", 0.0),
            win("com.chrome", "Chrome", 480.0),
            win("com.claude", "Claude", 960.0),
            win("com.finder", "Finder", 1440.0),
        ],
    );
    for (i, w) in [700.0, 500.0, 600.0, 485.0].into_iter().enumerate() {
        f.set_behaviour(
            h[i],
            Behaviour {
                min_size: Some(Size { w, h: 0.0 }),
                ..Behaviour::default()
            },
        );
    }
    let mut t = start_logged(f, cfg(), None);
    t.calls();
    t.focus(h[1]);
    let before = t.frame(h[1]).w;
    for _ in 0..4 {
        t.ok("resize width +100");
    }
    let after = t.frame(h[1]).w;
    eprintln!("chrome: {before} -> {after}");
    assert!(after >= before + 300.0, "grew: {before} -> {after}");
    for &w in &h {
        let r = t.frame(w);
        assert!(r.x >= 0.0 && r.right() <= 1920.0, "on screen: {r:?}");
    }
}

#[test]
fn focusing_another_monitor_by_key_brings_the_pointer_along() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0), mon("m2", W)],
        &[win("com.a", "a", 0.0), win("com.b", "b", W + 10.0)],
    );
    t.fake()
        .set_mouse(tatami_protocol::Point { x: 100.0, y: 100.0 });
    t.focus(h[0]);
    t.ok("focus-monitor next");
    let p = t.fake().mouse_location().expect("pointer");
    assert!(p.x >= W, "pointer moved to the second monitor: {p:?}");
    // The next chord therefore acts on the second monitor.
    t.ok("workspace 2");
    let m2 =
        t.r.monitors
            .iter()
            .find(|(_, m)| m.info.uuid == "m2")
            .map(|(i, _)| i)
            .unwrap();
    let vis = t.r.model.visible_on(m2).unwrap();
    assert_eq!(t.r.model.name_of(vis), Some("2"));
    // And focus crossing an edge does the same on the way back.
    t.fake().set_mouse(tatami_protocol::Point {
        x: W + 100.0,
        y: 100.0,
    });
    t.ok("workspace 1");
    t.ok("focus left");
    let p = t.fake().mouse_location().unwrap();
    assert!(
        p.x < W,
        "pointer followed focus back to the first monitor: {p:?}"
    );
}

#[test]
fn overview_is_an_outline_with_the_current_monitor_expanded() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0), mon("m2", W)],
        &[win("com.a", "a", 0.0), win("com.b", "b", W + 10.0)],
    );
    t.fake().set_mouse(tatami_protocol::Point {
        x: W + 100.0,
        y: 100.0,
    });
    t.focus(h[1]); // focus on m2: the overview opens with m2 expanded
    t.calls();
    t.ok("overview");
    assert!(t.r.overview);
    let (m1, m2) = (t.mon_id("m1"), t.mon_id("m2"));
    let (b, a) = (t.r.by_handle[&h[1]], t.r.by_handle[&h[0]]);
    let (rows, items) = t.r.overview_rows();
    let text: String = rows
        .iter()
        .map(|r| format!("{}|{}\n", r.key, r.desc))
        .collect();
    assert!(
        text.contains("▸|  1  Display m1  (1)\n") && text.contains("▾|  2  Display m2  (1) ◆"),
        "{text}"
    );
    assert_eq!(
        items,
        vec![
            OverviewItem::Monitor(m1),
            OverviewItem::Monitor(m2),
            OverviewItem::Window(b)
        ],
        "m1 collapsed, m2 expanded: {text}"
    );
    assert_eq!(
        t.r.overview_sel, 2,
        "highlight starts on the focused window"
    );
    assert_eq!(
        rows[0].command,
        vec!["overview".to_string(), "1".into()],
        "a header's click expands it"
    );
    // The sheet went to the main monitor (m1), not the pointer's (m2).
    let shown_on = t
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            FakeCall::OverlayOn(uuid) => Some(uuid),
            _ => None,
        })
        .next_back();
    assert_eq!(shown_on.as_deref(), Some("m1"));
    // `1` expands the first monitor and highlights its header; a click on
    // a header does the same and keeps what is already open.
    let down = |code: u16| RawKeyEvent::Down {
        keycode: code,
        mods: Mods::default(),
        repeat: false,
    };
    assert_eq!(t.key(down(18)), KeyDisposition::Swallow);
    assert_eq!(
        t.r.overview_rows().1,
        vec![
            OverviewItem::Monitor(m1),
            OverviewItem::Window(a),
            OverviewItem::Monitor(m2),
            OverviewItem::Window(b)
        ]
    );
    assert_eq!(t.r.overview_sel, 0);
    t.event(PlatformEvent::Command(vec!["overview".into(), "2".into()]));
    assert_eq!(t.r.overview_rows().1.len(), 4, "m1 stays open");
    assert_eq!(t.r.overview_sel, 2, "the clicked header is highlighted");
    // A second `overview` closes.
    t.ok("overview");
    assert!(!t.r.overview);
}

#[test]
fn overview_arrows_walk_the_outline_and_expand_or_collapse_monitors() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0), mon("m2", W)],
        &[win("com.a", "a", 0.0), win("com.b", "b", W + 10.0)],
    );
    t.fake()
        .set_mouse(tatami_protocol::Point { x: 100.0, y: 100.0 });
    t.focus(h[0]);
    t.ok("overview");
    let (m1, m2) = (t.mon_id("m1"), t.mon_id("m2"));
    let (a, b) = (t.r.by_handle[&h[0]], t.r.by_handle[&h[1]]);
    let down = |code: u16| RawKeyEvent::Down {
        keycode: code,
        mods: Mods::default(),
        repeat: false,
    };
    assert_eq!(
        t.r.overview_items[t.r.overview_sel],
        OverviewItem::Window(a)
    );
    // → on a window changes nothing.
    assert_eq!(t.key(down(124)), KeyDisposition::Swallow);
    assert_eq!(t.r.overview_items.len(), 3);
    // ← from a window collapses its monitor and lands on the header.
    assert_eq!(t.key(down(123)), KeyDisposition::Swallow);
    assert_eq!(
        t.r.overview_items,
        vec![OverviewItem::Monitor(m1), OverviewItem::Monitor(m2)]
    );
    assert_eq!(t.r.overview_sel, 0);
    // ↓ to m2, → expands it in place, ↓ onto b, Enter focuses b.
    assert_eq!(t.key(down(125)), KeyDisposition::Swallow);
    assert_eq!(t.key(down(124)), KeyDisposition::Swallow);
    assert_eq!(
        t.r.overview_items,
        vec![
            OverviewItem::Monitor(m1),
            OverviewItem::Monitor(m2),
            OverviewItem::Window(b)
        ]
    );
    assert_eq!(t.r.overview_sel, 1, "the highlight stays on the header");
    let (rows, _) = t.r.overview_rows();
    assert!(
        rows.iter()
            .any(|r| r.key == "▾" && r.desc.starts_with("▶ 2  ")),
        "{rows:?}"
    );
    // Enter on a header toggles it too.
    assert_eq!(t.key(down(36)), KeyDisposition::Swallow);
    assert_eq!(t.r.overview_items.len(), 2);
    assert_eq!(t.key(down(36)), KeyDisposition::Swallow);
    assert_eq!(t.r.overview_items.len(), 3);
    assert_eq!(t.key(down(125)), KeyDisposition::Swallow);
    assert_eq!(t.key(down(36)), KeyDisposition::Swallow);
    t.drain();
    assert!(!t.r.overview);
    assert_eq!(t.r.focused, Some(b));
    // A search looks under collapsed monitors too.
    t.ok("overview");
    t.key(down(123)); // collapse m2 (focused now)
    assert_eq!(t.r.overview_items.len(), 2);
    assert_eq!(t.key(down(44)), KeyDisposition::Swallow); // `/`
    assert_eq!(t.key(down(46)), KeyDisposition::Swallow); // `m`
    assert_eq!(t.key(down(18)), KeyDisposition::Swallow); // `1`: "Display m1"
    assert_eq!(t.r.overview_items, vec![OverviewItem::Window(a)]);
    let (rows, _) = t.r.overview_rows();
    assert!(
        !rows.iter().any(|r| r.desc.contains("Display m2")),
        "a monitor with no match has no header: {rows:?}"
    );
}

#[test]
fn the_overview_outlines_the_highlighted_monitor_on_screen() {
    // The list says which monitor the highlight is on; the desk shows it
    // too, with an outline that follows the highlight and goes when the
    // overview closes.
    let (mut t, h) = setup(
        vec![mon("m1", 0.0), mon("m2", W)],
        &[win("com.a", "a", 0.0), win("com.b", "b", W + 10.0)],
    );
    t.fake()
        .set_mouse(tatami_protocol::Point { x: 100.0, y: 100.0 });
    t.focus(h[0]);
    t.calls();
    t.ok("overview");
    assert!(
        t.calls().contains(&FakeCall::Mark(Some("m1".into()))),
        "the highlight starts on a, which is on m1"
    );
    let down = |code: u16| RawKeyEvent::Down {
        keycode: code,
        mods: Mods::default(),
        repeat: false,
    };
    t.key(down(123)); // ← collapses m1 onto its header
    t.key(down(125)); // ↓ to m2's header
    t.drain();
    assert!(t.calls().contains(&FakeCall::Mark(Some("m2".into()))));
    t.key(down(53)); // Esc
    t.drain();
    assert_eq!(
        t.calls().iter().rfind(|c| matches!(c, FakeCall::Mark(_))),
        Some(&FakeCall::Mark(None)),
        "closing takes the outline down"
    );
}

#[test]
fn a_letter_on_the_overview_focuses_that_window_anywhere() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0), mon("m2", W)],
        &[win("com.a", "a", 0.0), win("com.b", "b", W + 10.0)],
    );
    t.fake()
        .set_mouse(tatami_protocol::Point { x: 100.0, y: 100.0 });
    t.focus(h[0]);
    t.ok("overview");
    let down = |code: u16| RawKeyEvent::Down {
        keycode: code,
        mods: Mods::default(),
        repeat: false,
    };
    // m2 is collapsed; `2` (keycode 19) expands it, and b is letter `b` (11).
    assert_eq!(t.key(down(11)), KeyDisposition::Pass, "no such letter yet");
    t.drain();
    t.ok("overview");
    assert_eq!(t.key(down(19)), KeyDisposition::Swallow);
    assert_eq!(t.key(down(11)), KeyDisposition::Swallow);
    t.drain();
    assert!(!t.r.overview, "closed after the pick");
    assert_eq!(t.r.focused, Some(t.r.by_handle[&h[1]]));
    let p = t.fake().mouse_location().unwrap();
    assert!(p.x >= W, "pointer followed to the second monitor: {p:?}");
    // Esc closes without picking.
    t.ok("overview");
    assert_eq!(t.key(down(53)), KeyDisposition::Swallow);
    t.drain();
    assert!(!t.r.overview);
}

#[test]
fn overview_search_filters_the_windows_and_enter_focuses_the_highlighted_match() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[
            win("com.a", "alpha notes", 0.0),
            win("com.b", "beta", 300.0),
            win("com.c", "gamma", 600.0),
        ],
    );
    t.focus(h[0]);
    t.ok("overview");
    let down = |code: u16| RawKeyEvent::Down {
        keycode: code,
        mods: Mods::default(),
        repeat: false,
    };
    // `/` (44) starts the search; `g` (5) narrows to gamma.
    assert_eq!(t.key(down(44)), KeyDisposition::Swallow);
    assert_eq!(t.key(down(5)), KeyDisposition::Swallow);
    t.drain();
    let (rows, items) = t.r.overview_rows();
    assert_eq!(
        items,
        vec![OverviewItem::Window(t.r.by_handle[&h[2]])],
        "only gamma matches"
    );
    assert!(
        rows.iter()
            .any(|r| r.key.is_empty() && r.desc.starts_with("  ▶ ") && r.desc.contains("gamma")),
        "{rows:?}"
    );
    assert!(!rows.iter().any(|r| r.desc.contains("alpha")), "{rows:?}");
    // Backspace to an empty query keeps search mode; one more leaves it.
    assert_eq!(t.key(down(51)), KeyDisposition::Swallow);
    assert_eq!(t.r.overview_query.as_deref(), Some(""));
    assert_eq!(t.r.overview_rows().1.len(), 3);
    // ↓ (125) moves the highlight to beta; Enter (36) focuses it.
    assert_eq!(t.key(down(125)), KeyDisposition::Swallow);
    assert_eq!(t.key(down(36)), KeyDisposition::Swallow);
    t.drain();
    assert!(!t.r.overview);
    assert_eq!(t.r.focused, Some(t.r.by_handle[&h[1]]));
}

#[test]
fn overview_search_lists_matching_keys_and_enter_runs_the_binding() {
    let (mut t, h) = setup(vec![mon("m1", 0.0)], &[win("com.a", "alpha", 0.0)]);
    t.focus(h[0]);
    t.ok("overview");
    let down = |code: u16| RawKeyEvent::Down {
        keycode: code,
        mods: Mods::default(),
        repeat: false,
    };
    let chord = |s: &str| tatami_keymap::KeyChord::parse(s).unwrap();
    // `/` (44) then `hot` (4, 31, 17): no window matches; the service
    // mode's `u` does, shown as the keys you would press.
    for k in [44, 4, 31, 17] {
        assert_eq!(t.key(down(k)), KeyDisposition::Swallow);
    }
    let (rows, items) = t.r.overview_rows();
    let service = t.r.keymap.modes().iter().position(|m| m == "service");
    assert!(service.is_some());
    assert_eq!(
        items,
        vec![OverviewItem::Key {
            mode: service,
            chord: chord("u"),
        }]
    );
    let row = rows
        .iter()
        .find(|r| r.desc.contains("hot-swap"))
        .expect("a key row");
    assert!(row.desc.starts_with("  ▶ alt+s u"), "{row:?}");
    assert_eq!(row.command, ["trigger-binding", "u", "--mode", "service"]);
    assert!(
        rows.iter().any(|r| r.desc.contains("keys  (1)")),
        "{rows:?}"
    );
    // Enter (36) runs it as the chord would, once the sheet is gone.
    assert_eq!(t.key(down(36)), KeyDisposition::Swallow);
    t.drain();
    assert!(!t.r.overview);
    assert!(t.out.contains(&Outcome::HotSwap), "{:?}", t.out);

    // A mode's name finds the chord that opens it and the keys inside it;
    // Enter on the opener enters the mode.
    t.ok("overview");
    for k in [44, 15, 14, 1, 34, 6, 14] {
        t.key(down(k)); // `/resize`
    }
    let (_, items) = t.r.overview_rows();
    let opener = OverviewItem::Key {
        mode: None,
        chord: chord("alt+r"),
    };
    let at = items
        .iter()
        .position(|&i| i == opener)
        .expect("alt+r listed");
    let resize = t.r.keymap.modes().iter().position(|m| m == "resize");
    assert!(
        items.contains(&OverviewItem::Key {
            mode: resize,
            chord: chord("h"),
        }),
        "{items:?}"
    );
    for _ in 0..at {
        t.key(down(125)); // ↓
    }
    assert_eq!(t.key(down(36)), KeyDisposition::Swallow);
    t.drain();
    assert!(!t.r.overview);
    assert_eq!(t.r.keys.mode(), Some("resize"));
}

#[test]
fn a_parked_sibling_window_surfacing_after_our_raise_does_not_switch_workspaces() {
    // Chrome has a window on each monitor; the one on m2 is parked on a
    // hidden workspace. Focusing the m1 window (the overview, `focus
    // --window-id`) activates Chrome, and macOS reports the parked window
    // as focused first. That must not switch m2's workspace.
    let (mut t, h) = setup(
        vec![mon("m1", 0.0), mon("m2", 1000.0)],
        &[
            win("com.chrome", "a", 0.0),
            win("com.chrome", "b", 1000.0),
            win("com.other", "c", 300.0),
        ],
    );
    t.focus(h[1]);
    t.ok("move-node-to-workspace 2");
    let b = t.r.by_handle[&h[1]];
    let ws_b = t.r.wins[b].ws;
    assert!(!t.r.model.is_visible(ws_b), "b is parked on m2's 2");
    let m2 = t.mon_id("m2");
    let shown = t.r.model.visible_on(m2);
    t.focus(h[2]);
    t.ok(&format!("focus --window-id {}", h[0].0));
    assert_eq!(t.focused(), Some(h[0]));
    // The echo: Chrome's old key window, parked, reported focused.
    t.focus(h[1]);
    assert_eq!(t.r.model.visible_on(m2), shown, "m2 keeps its workspace");
    assert!(!t.r.model.is_visible(ws_b));
    assert_eq!(
        t.focused(),
        Some(h[0]),
        "the asked-for window stays focused"
    );
    assert!(
        t.calls()
            .iter()
            .any(|c| matches!(c, FakeCall::Raise(x) if *x == h[0])),
        "the asked-for window is raised again"
    );
    // A second later the same event is the user (Cmd-tab, the Dock).
    t.now += Duration::from_secs(2);
    t.focus(h[1]);
    assert!(t.r.model.is_visible(ws_b), "a real focus switches it in");
}

#[test]
fn overview_window_rows_carry_a_focus_command_for_clicks() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[win("com.a", "a", 0.0), win("com.b", "b", 500.0)],
    );
    t.ok("overview");
    let (rows, _) = t.r.overview_rows();
    let cmd = &rows.iter().find(|r| r.desc.contains('b')).unwrap().command;
    assert_eq!(
        cmd,
        &[
            "focus".to_string(),
            "--window-id".into(),
            h[1].0.to_string()
        ]
    );
    t.event(PlatformEvent::Command(cmd.clone()));
    assert_eq!(t.r.focused, Some(t.r.by_handle[&h[1]]));
}

#[test]
fn a_menu_workspace_row_switches_the_monitor_it_is_listed_under() {
    // Each monitor has its own workspaces and the row sits under its
    // monitor's header; picking it must switch that monitor, whichever
    // one has focus (the menu bar is usually on another one).
    let (mut t, h) = setup(
        vec![mon("m1", 0.0), mon("m2", W)],
        &[win("com.a", "a", 0.0), win("com.b", "b", W)],
    );
    t.focus(h[1]);
    t.ok("workspace 2");
    assert_eq!(t.visible_name("m2"), "2");
    t.focus(h[0]);
    assert_eq!(t.r.focused_monitor, t.mon_id("m1"));
    let rows = t.r.menu_items();
    let m2 = rows
        .iter()
        .position(|r| r.title.starts_with("Display m2"))
        .unwrap();
    let row = rows[m2 + 1..]
        .iter()
        .find(|r| r.title.starts_with("1 "))
        .unwrap();
    t.event(PlatformEvent::Command(row.command.clone()));
    assert_eq!(t.visible_name("m2"), "1", "m2 switched");
    assert_eq!(t.visible_name("m1"), "1", "m1 untouched");
    assert_eq!(t.r.focused_monitor, t.mon_id("m2"));
}

#[test]
fn overview_monitor_setting_overrides_the_main_display() {
    let (f, _h) = fake_with(
        vec![mon("m1", 0.0), mon("m2", W)],
        &[win("com.a", "a", 0.0), win("com.b", "b", W + 10.0)],
    );
    let mut c = cfg();
    c.general.overview_monitor = "m2".into(); // part of "Display m2"
    let mut t = start(f, c, None);
    t.fake()
        .set_mouse(tatami_protocol::Point { x: 100.0, y: 100.0 });
    t.ok("overview");
    let shown_on = t
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            FakeCall::OverlayOn(uuid) => Some(uuid),
            _ => None,
        })
        .next_back();
    assert_eq!(shown_on.as_deref(), Some("m2"));
    t.ok("overview");
    // A number works too, and `pointer` follows the mouse.
    t.r.reload_config(
        {
            let mut c = cfg();
            c.general.overview_monitor = "1".into();
            c
        },
        t.now,
    );
    t.ok("overview");
    let last = t
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            FakeCall::OverlayOn(u) => Some(u),
            _ => None,
        })
        .next_back();
    assert_eq!(last.as_deref(), Some("m1"));
}

#[test]
fn by_default_each_new_window_gets_its_own_workspace_and_is_shown() {
    let (f, h) = fake_with(vec![mon("m1", 0.0)], &[win("com.a", "a", 0.0)]);
    let mut t = start(f, cfg_own(), None);
    assert_eq!(t.frame(h[0]), r(0.0, 0.0, W, H), "alone on workspace 1");
    let pid = t.fake().add_app("com.code", "Code");
    let b = t
        .fake()
        .add_window(pid, "lib.rs", r(10.0, 10.0, 300.0, 300.0));
    t.settle();
    let m1 = t.mon_id("m1");
    let vis = t.r.model.visible_on(m1).unwrap();
    assert_eq!(
        t.r.model.name_of(vis),
        Some("2"),
        "the new window opened workspace 2 and we are on it"
    );
    assert_eq!(t.r.wins[t.r.by_handle[&b]].ws, vis);
    assert_eq!(t.frame(b), r(0.0, 0.0, W, H), "it fills the screen");
    // A third window takes 3; moving it onto 2 tiles the two there.
    let c = t
        .fake()
        .add_window(pid, "main.rs", r(10.0, 10.0, 300.0, 300.0));
    t.settle();
    assert_eq!(
        t.r.model.name_of(t.r.model.visible_on(m1).unwrap()),
        Some("3")
    );
    t.ok("move-node-to-workspace 2 --focus-follows-window");
    assert_eq!(
        t.r.model.name_of(t.r.model.visible_on(m1).unwrap()),
        Some("2")
    );
    assert_eq!(t.frame(b).w + t.frame(c).w, W, "b and c share workspace 2");
    // Workspace 3 is empty and hidden: gone, so the next window reuses it.
    assert!(t.r.model.find(m1, "3").is_none());
    let d = t.fake().add_window(pid, "x.rs", r(0.0, 0.0, 100.0, 100.0));
    t.settle();
    assert_eq!(t.r.model.name_of(t.r.wins[t.r.by_handle[&d]].ws), Some("3"));
}

#[test]
fn own_workspace_policy_falls_back_to_tiling_when_all_names_are_taken() {
    let (f, _h) = fake_with(vec![mon("m1", 0.0)], &[]);
    let mut c = cfg_own();
    c.workspaces.names = vec!["1".into(), "2".into()];
    let mut t = start(f, c, None);
    let pid = t.fake().add_app("com.code", "Code");
    let a = t.fake().add_window(pid, "a", r(0.0, 0.0, 100.0, 100.0));
    t.settle();
    let b = t.fake().add_window(pid, "b", r(0.0, 0.0, 100.0, 100.0));
    t.settle();
    let cw = t.fake().add_window(pid, "c", r(0.0, 0.0, 100.0, 100.0));
    t.settle();
    let m1 = t.mon_id("m1");
    assert_eq!(t.r.model.name_of(t.r.wins[t.r.by_handle[&a]].ws), Some("1"));
    assert_eq!(t.r.model.name_of(t.r.wins[t.r.by_handle[&b]].ws), Some("2"));
    assert_eq!(
        t.r.wins[t.r.by_handle[&cw]].ws,
        t.r.model.visible_on(m1).unwrap(),
        "c tiles into the visible one"
    );
    assert_eq!(t.frame(b).w + t.frame(cw).w, W);
}

#[test]
fn overview_arrows_and_enter_work_without_a_search() {
    let (mut t, h) = setup(
        vec![mon("m1", 0.0)],
        &[
            win("com.a", "a", 0.0),
            win("com.b", "b", 300.0),
            win("com.c", "c", 600.0),
        ],
    );
    t.focus(h[1]);
    t.ok("overview");
    assert_eq!(
        t.r.overview_sel, 2,
        "highlight starts on the focused window"
    );
    let (rows, _) = t.r.overview_rows();
    assert!(
        rows.iter()
            .any(|r| r.key == "b" && r.desc.starts_with("  ▶ ")),
        "{rows:?}"
    );
    let down = |code: u16| RawKeyEvent::Down {
        keycode: code,
        mods: Mods::default(),
        repeat: false,
    };
    assert_eq!(t.key(down(125)), KeyDisposition::Swallow); // ↓ → c
    assert_eq!(t.key(down(36)), KeyDisposition::Swallow); // Enter
    t.drain();
    assert!(!t.r.overview);
    assert_eq!(t.r.focused, Some(t.r.by_handle[&h[2]]));
}

#[test]
fn two_displays_sharing_a_uuid_are_two_monitors() {
    // macOS gives identical panels without a serial the same display
    // uuid. Taken at face value the pair is one monitor to the reactor:
    // one window per pair stays, and `alt+1` after `alt+2` lays the
    // left monitor's windows out on the right one.
    let twin = |x: f64| MonitorInfo {
        uuid: "same".into(),
        name: format!("Twin {x}"),
        frame: Rect::new(x, 0.0, W, H),
        visible: Rect::new(x, 0.0, W, H),
        is_main: x == 0.0,
    };
    let (f, h) = fake_with(
        vec![twin(0.0), twin(W)],
        &[win("com.a", "a", 0.0), win("com.b", "b", W)],
    );
    let mut t = start(f, cfg(), None);
    assert_eq!(t.r.order.len(), 2, "both twins are monitors");
    let uuids: Vec<String> = t.r.order.iter().map(|&m| t.r.mon_uuid(m)).collect();
    assert_eq!(uuids, ["same", "same#2"], "left to right, suffixed");
    assert_eq!(t.frame(h[0]), r(0.0, 0.0, W, H), "a fills the left twin");
    assert_eq!(t.frame(h[1]), r(W, 0.0, W, H), "b fills the right twin");
    // The same pair again is the same desk: nothing folds or moves.
    let desk = t.r.desk_id();
    t.fake().set_screens(vec![twin(0.0), twin(W)]);
    t.settle();
    assert_eq!(t.r.desk_id(), desk, "the desk id is stable");
    assert_eq!(t.r.order.len(), 2);
    assert_eq!(t.frame(h[1]), r(W, 0.0, W, H), "b stays on the right twin");
}

#[test]
fn a_float_carried_to_another_monitor_changes_workspace() {
    // plyterm, floated on the left monitor and then put on the centre
    // one, stayed in the left workspace 1: `alt+2` there hid it along
    // with the tiles, and `alt+1` showed it fitted into the left monitor.
    let (mut t, h) = setup(
        vec![mon("m1", 0.0), mon("m2", W)],
        &[win("com.a", "a", 0.0), win("com.b", "b", 300.0)],
    );
    let m2 = t.mon_id("m2");
    t.focus(h[1]);
    t.ok("layout floating");
    let there = Rect::new(W + 100.0, 50.0, 400.0, 300.0);
    t.fake().user_set_frame(h[1], there);
    t.settle();
    let b = t.r.wins[t.r.by_handle[&h[1]]].ws;
    assert_eq!(t.r.ws_monitor(b), m2, "b now belongs to the second monitor");
    assert_eq!(t.frame(h[1]), there, "and was not moved");
    // Switching the first monitor away and back leaves it alone.
    t.ok("workspace 2");
    assert_eq!(t.frame(h[1]), there, "b is not hidden with monitor 1");
    t.ok("workspace 1");
    assert_eq!(t.frame(h[1]), there, "nor fitted back into it");
    assert_eq!(
        t.frame(h[0]),
        r(0.0, 0.0, W, H),
        "a has monitor 1 to itself"
    );
}
