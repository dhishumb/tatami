//! In-memory `Platform` for reactor tests. Apps can be told to misbehave
//! the way real ones do (ignore frames, enforce a minimum size, snap to a
//! character grid); every call is logged and the events a real system would
//! send back are queued for `take_events`.
//!
//! Interior mutability because several `Platform` methods take `&self`.

use std::cell::RefCell;

use tatami_protocol::{Rect, Size, WindowHandle};

use crate::platform::{
    AppInfo, MenuItem, MonitorInfo, OverlayContent, Platform, PlatformError, PlatformEvent,
    SetOrder, WindowInfo,
};

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Behaviour {
    /// Every frame request is silently ignored.
    pub ignore_frames: bool,
    /// Sizes below this are bumped up to it.
    pub min_size: Option<Size>,
    /// Sizes are floored to multiples of this (terminal cells).
    pub grid: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FakeCall {
    SetFrame {
        window: WindowHandle,
        rect: Rect,
        order: SetOrder,
    },
    Raise(WindowHandle),
    SetMinimized(WindowHandle, bool),
    Close(WindowHandle),
    Observe(i32),
    Unobserve(i32),
    Launch(String),
    /// Number of rows in the menu that was set.
    Menu(usize),
    /// Title of the overlay shown, `None` when hidden.
    Overlay(Option<String>),
    /// Uuid of the monitor a sheet was shown on.
    OverlayOn(String),
    /// Uuid of the monitor outlined, `None` when the outline came down.
    Mark(Option<String>),
    Status(String),
}

#[derive(Debug, Clone)]
struct FakeWindow {
    info: WindowInfo,
    behaviour: Behaviour,
}

#[derive(Debug, Default)]
struct State {
    screens: Vec<MonitorInfo>,
    apps: Vec<AppInfo>,
    windows: Vec<FakeWindow>,
    /// What `mouse_location` answers; `None` until `set_mouse`.
    mouse: Option<tatami_protocol::Point>,
    observed: Vec<i32>,
    focused: Option<WindowHandle>,
    calls: Vec<FakeCall>,
    events: Vec<PlatformEvent>,
    next_pid: i32,
    next_handle: u64,
}

impl State {
    fn window(&mut self, w: WindowHandle) -> Result<&mut FakeWindow, PlatformError> {
        self.windows
            .iter_mut()
            .find(|x| x.info.handle == w)
            .ok_or(PlatformError::NoWindow)
    }

    /// Store a new frame and queue the move/resize events macOS would send.
    fn apply_frame(&mut self, w: WindowHandle, r: Rect) {
        let Ok(win) = self.window(w) else { return };
        let old = win.info.frame;
        win.info.frame = r;
        if (old.x, old.y) != (r.x, r.y) {
            self.events.push(PlatformEvent::WindowMoved(w, r));
        }
        if (old.w, old.h) != (r.w, r.h) {
            self.events.push(PlatformEvent::WindowResized(w, r));
        }
    }
}

/// What a window with `b` ends up with when asked for `req`.
fn accept(b: &Behaviour, cur: Rect, req: Rect) -> Rect {
    if b.ignore_frames {
        return cur;
    }
    let mut r = req;
    if let Some(g) = b.grid.filter(|g| *g > 0.0) {
        r.w = (r.w / g).floor() * g;
        r.h = (r.h / g).floor() * g;
    }
    if let Some(m) = b.min_size {
        r.w = r.w.max(m.w);
        r.h = r.h.max(m.h);
    }
    r
}

#[derive(Debug, Default)]
pub struct FakePlatform {
    st: RefCell<State>,
}

impl FakePlatform {
    pub fn new(screens: Vec<MonitorInfo>) -> Self {
        let st = State {
            screens,
            next_pid: 100,
            next_handle: 1,
            ..State::default()
        };
        Self {
            st: RefCell::new(st),
        }
    }

    pub fn set_mouse(&self, p: tatami_protocol::Point) {
        self.st.borrow_mut().mouse = Some(p);
    }

    pub fn set_screens(&self, screens: Vec<MonitorInfo>) {
        let mut st = self.st.borrow_mut();
        st.screens = screens;
        st.events.push(PlatformEvent::ScreensChanged);
    }

    pub fn add_app(&self, bundle_id: &str, name: &str) -> i32 {
        let mut st = self.st.borrow_mut();
        st.next_pid = st.next_pid.max(100) + 1;
        let pid = st.next_pid;
        let app = AppInfo {
            pid,
            bundle_id: Some(bundle_id.to_string()),
            name: name.to_string(),
            exe: None,
        };
        st.apps.push(app.clone());
        st.events.push(PlatformEvent::AppLaunched(app));
        pid
    }

    /// A standard window. `WindowCreated` is queued only once the app is
    /// observed, as with real AX notifications.
    pub fn add_window(&self, pid: i32, title: &str, frame: Rect) -> WindowHandle {
        let mut st = self.st.borrow_mut();
        st.next_handle = st.next_handle.max(1) + 1;
        let handle = WindowHandle(st.next_handle);
        let info = WindowInfo {
            handle,
            pid,
            title: title.to_string(),
            role: "AXWindow".into(),
            subrole: "AXStandardWindow".into(),
            frame,
            minimized: false,
            fullscreen: false,
            resizable: true,
        };
        if st.observed.contains(&pid) {
            st.events.push(PlatformEvent::WindowCreated(info.clone()));
        }
        st.windows.push(FakeWindow {
            info,
            behaviour: Behaviour::default(),
        });
        handle
    }

    /// The window closes on its own (user or app).
    pub fn remove_window(&self, w: WindowHandle) {
        let mut st = self.st.borrow_mut();
        let before = st.windows.len();
        st.windows.retain(|x| x.info.handle != w);
        if st.windows.len() != before {
            st.events.push(PlatformEvent::WindowDestroyed(w));
        }
    }

    pub fn set_behaviour(&self, w: WindowHandle, b: Behaviour) {
        if let Ok(win) = self.st.borrow_mut().window(w) {
            win.behaviour = b;
        }
    }

    /// The user drags or resizes a window.
    /// Mark a window fixed-size (its app refuses `AXSize` writes). Set it
    /// before the reactor adopts the window.
    pub fn set_resizable(&self, w: WindowHandle, on: bool) {
        if let Ok(win) = self.st.borrow_mut().window(w) {
            win.info.resizable = on;
        }
    }

    pub fn user_set_frame(&self, w: WindowHandle, r: Rect) {
        self.st.borrow_mut().apply_frame(w, r);
    }

    /// The window enters (or leaves) macOS native fullscreen, landing on
    /// `frame`; the move/resize events follow as they would from AX.
    pub fn set_native_fullscreen(&self, w: WindowHandle, on: bool, frame: Rect) {
        let mut st = self.st.borrow_mut();
        if let Ok(win) = st.window(w) {
            win.info.fullscreen = on;
        }
        st.apply_frame(w, frame);
    }

    /// The frame the app accepted.
    pub fn frame(&self, w: WindowHandle) -> Option<Rect> {
        self.st
            .borrow()
            .windows
            .iter()
            .find(|x| x.info.handle == w)
            .map(|x| x.info.frame)
    }

    pub fn calls(&self) -> Vec<FakeCall> {
        self.st.borrow().calls.clone()
    }

    pub fn take_calls(&self) -> Vec<FakeCall> {
        std::mem::take(&mut self.st.borrow_mut().calls)
    }

    pub fn take_events(&self) -> Vec<PlatformEvent> {
        std::mem::take(&mut self.st.borrow_mut().events)
    }
}

impl Platform for FakePlatform {
    fn screens(&self) -> Vec<MonitorInfo> {
        self.st.borrow().screens.clone()
    }

    fn running_apps(&self) -> Vec<AppInfo> {
        self.st.borrow().apps.clone()
    }

    fn app_windows(&mut self, pid: i32) -> Vec<WindowInfo> {
        self.st
            .borrow()
            .windows
            .iter()
            .filter(|w| w.info.pid == pid)
            .map(|w| w.info.clone())
            .collect()
    }

    fn window_frame(&self, w: WindowHandle) -> Option<Rect> {
        self.frame(w)
    }

    fn set_window_frame(
        &self,
        w: WindowHandle,
        r: Rect,
        order: SetOrder,
    ) -> Result<(), PlatformError> {
        let mut st = self.st.borrow_mut();
        st.calls.push(FakeCall::SetFrame {
            window: w,
            rect: r,
            order,
        });
        let win = st.window(w)?;
        let got = accept(&win.behaviour, win.info.frame, r);
        st.apply_frame(w, got);
        Ok(())
    }

    fn raise(&self, w: WindowHandle) -> Result<(), PlatformError> {
        let mut st = self.st.borrow_mut();
        st.calls.push(FakeCall::Raise(w));
        st.window(w)?;
        if st.focused != Some(w) {
            st.focused = Some(w);
            st.events.push(PlatformEvent::WindowFocused(w));
        }
        Ok(())
    }

    fn set_minimized(&self, w: WindowHandle, on: bool) -> Result<(), PlatformError> {
        let mut st = self.st.borrow_mut();
        st.calls.push(FakeCall::SetMinimized(w, on));
        let win = st.window(w)?;
        if win.info.minimized != on {
            win.info.minimized = on;
            st.events.push(PlatformEvent::WindowMinimized(w, on));
        }
        Ok(())
    }

    fn close(&self, w: WindowHandle) -> Result<(), PlatformError> {
        self.st.borrow_mut().calls.push(FakeCall::Close(w));
        if self.frame(w).is_none() {
            return Err(PlatformError::NoWindow);
        }
        self.remove_window(w);
        let mut st = self.st.borrow_mut();
        if st.focused == Some(w) {
            st.focused = None;
        }
        Ok(())
    }

    fn focused_window(&self) -> Option<WindowHandle> {
        self.st.borrow().focused
    }

    fn observe_app(&mut self, pid: i32) -> Result<(), PlatformError> {
        let st = self.st.get_mut();
        st.calls.push(FakeCall::Observe(pid));
        if !st.apps.iter().any(|a| a.pid == pid) {
            return Err(PlatformError::NoApp(pid));
        }
        if !st.observed.contains(&pid) {
            st.observed.push(pid);
        }
        Ok(())
    }

    fn unobserve_app(&mut self, pid: i32) {
        let st = self.st.get_mut();
        st.calls.push(FakeCall::Unobserve(pid));
        st.observed.retain(|&p| p != pid);
    }

    fn launch(&self, bundle_id: &str) -> Result<(), PlatformError> {
        self.st
            .borrow_mut()
            .calls
            .push(FakeCall::Launch(bundle_id.to_string()));
        Ok(())
    }

    fn show_overlay(&mut self, content: Option<&OverlayContent>, monitor: &MonitorInfo) {
        let calls = &mut self.st.get_mut().calls;
        calls.push(FakeCall::Overlay(content.map(|c| c.title.clone())));
        if content.is_some() {
            calls.push(FakeCall::OverlayOn(monitor.uuid.clone()));
        }
    }

    fn mark_monitor(&mut self, monitor: Option<&MonitorInfo>) {
        self.st
            .get_mut()
            .calls
            .push(FakeCall::Mark(monitor.map(|m| m.uuid.clone())));
    }

    fn set_status(&mut self, text: &str) {
        self.st
            .get_mut()
            .calls
            .push(FakeCall::Status(text.to_string()));
    }

    fn set_menu(&mut self, items: &[MenuItem]) {
        self.st.get_mut().calls.push(FakeCall::Menu(items.len()));
    }

    fn mouse_location(&self) -> Option<tatami_protocol::Point> {
        self.st.borrow().mouse
    }

    fn warp_mouse(&self, p: tatami_protocol::Point) {
        self.st.borrow_mut().mouse = Some(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake() -> (FakePlatform, i32, WindowHandle) {
        let mut p = FakePlatform::new(Vec::new());
        let pid = p.add_app("com.example.term", "Term");
        p.observe_app(pid).unwrap();
        let w = p.add_window(pid, "shell", Rect::new(0.0, 0.0, 400.0, 300.0));
        (p, pid, w)
    }

    #[test]
    fn frames_follow_behaviour() {
        let (p, _, w) = fake();
        p.set_window_frame(w, Rect::new(10.0, 20.0, 500.0, 400.0), SetOrder::SizePos)
            .unwrap();
        assert_eq!(p.frame(w), Some(Rect::new(10.0, 20.0, 500.0, 400.0)));

        p.set_behaviour(
            w,
            Behaviour {
                grid: Some(8.0),
                min_size: Some(Size { w: 300.0, h: 0.0 }),
                ..Default::default()
            },
        );
        p.set_window_frame(w, Rect::new(0.0, 0.0, 205.0, 403.0), SetOrder::SizePos)
            .unwrap();
        assert_eq!(p.frame(w), Some(Rect::new(0.0, 0.0, 300.0, 400.0)));

        p.set_behaviour(
            w,
            Behaviour {
                ignore_frames: true,
                ..Default::default()
            },
        );
        p.set_window_frame(w, Rect::new(1.0, 1.0, 1.0, 1.0), SetOrder::PosSizePos)
            .unwrap();
        assert_eq!(p.frame(w), Some(Rect::new(0.0, 0.0, 300.0, 400.0)));
        assert_eq!(
            p.calls()
                .iter()
                .filter(|c| matches!(c, FakeCall::SetFrame { .. }))
                .count(),
            3
        );
    }

    #[test]
    fn events_mirror_what_macos_would_send() {
        let (mut p, pid, w) = fake();
        let ev = p.take_events();
        assert!(matches!(ev[0], PlatformEvent::AppLaunched(_)));
        assert!(matches!(&ev[1], PlatformEvent::WindowCreated(i) if i.handle == w));
        p.set_window_frame(w, Rect::new(5.0, 0.0, 400.0, 300.0), SetOrder::SizePos)
            .unwrap();
        assert_eq!(
            p.take_events(),
            vec![PlatformEvent::WindowMoved(
                w,
                Rect::new(5.0, 0.0, 400.0, 300.0)
            )]
        );
        p.raise(w).unwrap();
        assert_eq!(p.focused_window(), Some(w));
        p.close(w).unwrap();
        assert_eq!(
            p.take_events(),
            vec![
                PlatformEvent::WindowFocused(w),
                PlatformEvent::WindowDestroyed(w)
            ]
        );
        assert_eq!(p.close(w), Err(PlatformError::NoWindow));
        assert!(p.app_windows(pid).is_empty());
        assert_eq!(p.focused_window(), None);
        p.unobserve_app(pid);
        p.add_window(pid, "quiet", Rect::default());
        assert!(p.take_events().is_empty(), "unobserved apps emit nothing");
    }

    #[test]
    fn calls_are_logged_in_order() {
        let (mut p, pid, w) = fake();
        p.take_calls();
        p.set_minimized(w, true).unwrap();
        p.launch("com.example.mail").unwrap();
        p.set_status("1");
        assert_eq!(p.observe_app(9999), Err(PlatformError::NoApp(9999)));
        assert_eq!(
            p.take_calls(),
            vec![
                FakeCall::SetMinimized(w, true),
                FakeCall::Launch("com.example.mail".into()),
                FakeCall::Status("1".into()),
                FakeCall::Observe(9999),
            ]
        );
        assert!(p.app_windows(pid)[0].minimized);
    }
}
