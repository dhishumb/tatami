//! tatami-platform: the macOS implementation of `tatami_core::Platform`,
//! built on the public Accessibility API plus `_AXUIElementGetWindow` for
//! window identity. Everything runs on the main thread.

pub mod apps;
pub mod ax;
pub mod event_tap;
mod mark;
mod observers;
mod overlay;
pub mod runloop;
pub mod screens;
mod statusbar;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use objc2::MainThreadMarker;
use objc2_application_services::AXError;
use objc2_core_foundation::{CFArray, CFRetained, CFString, CGPoint, CGSize};
use tatami_core::{
    AppInfo, EventSink, KeySink, MenuItem, MonitorInfo, OverlayContent, Platform, PlatformError,
    PlatformEvent, SetOrder, WindowInfo,
};
use tatami_protocol::{Rect, WindowHandle};

use crate::ax::{AxElement, AxResult, to_platform_error};
use crate::event_tap::KeyTap;

/// Upper bound on any single AX call, so a hung app cannot stall the main loop
/// (the default is 6 s, long enough for macOS to disable the event tap).
const AX_TIMEOUT_SECS: f32 = 1.0;

/// Attributes read per window in one round trip; order matches `WIN_*`.
const WINDOW_ATTRS: [&str; 7] = [
    "AXRole",
    "AXSubrole",
    "AXTitle",
    "AXPosition",
    "AXSize",
    "AXMinimized",
    "AXFullScreen",
];
const FRAME_ATTRS: [&str; 2] = ["AXPosition", "AXSize"];

struct AppEntry {
    el: AxElement,
    observer: Option<observers::AppObserver>,
}

struct WinEntry {
    pid: i32,
    el: AxElement,
}

/// State shared with the run-loop callbacks (AX observers, workspace
/// notifications). Main thread only.
pub(crate) struct Shared {
    apps: RefCell<HashMap<i32, AppEntry>>,
    windows: RefCell<HashMap<WindowHandle, WinEntry>>,
    window_attrs: CFRetained<CFArray<CFString>>,
    frame_attrs: CFRetained<CFArray<CFString>>,
    sink: RefCell<Option<EventSink>>,
}

pub struct MacPlatform {
    mtm: MainThreadMarker,
    shared: Rc<Shared>,
    observing: Option<observers::Global>,
    key_tap: Option<KeyTap>,
    overlay: overlay::Overlay,
    mark: mark::Mark,
    status: statusbar::Status,
}

fn names(list: &[&'static str]) -> CFRetained<CFArray<CFString>> {
    let v: Vec<_> = list.iter().map(|s| CFString::from_static_str(s)).collect();
    CFArray::from_retained_objects(&v)
}

impl Default for MacPlatform {
    fn default() -> Self {
        Self::new()
    }
}

impl MacPlatform {
    /// Panics off the main thread: AppKit and the AX tables are main-thread only.
    pub fn new() -> Self {
        let mtm = MainThreadMarker::new().expect("MacPlatform must be created on the main thread");
        if let Err(e) = AxElement::system_wide().set_timeout(AX_TIMEOUT_SECS) {
            tracing::warn!("AX messaging timeout not set: {}", e.0);
        }
        let shared = Rc::new(Shared {
            apps: RefCell::default(),
            windows: RefCell::default(),
            window_attrs: names(&WINDOW_ATTRS),
            frame_attrs: names(&FRAME_ATTRS),
            sink: RefCell::default(),
        });
        observers::set_current(&shared);
        let status = statusbar::Status::new(Rc::downgrade(&shared));
        let overlay = overlay::Overlay::new(Rc::downgrade(&shared));
        Self {
            mtm,
            shared,
            observing: None,
            key_tap: None,
            overlay,
            mark: mark::Mark::new(),
            status,
        }
    }

    pub fn is_trusted(&self) -> bool {
        ax::is_trusted()
    }

    /// Where `PlatformEvent`s go. Replaces any previous sink.
    /// The app whose AX element says it is frontmost (probe/debugging).
    pub fn ax_frontmost_pid(&self) -> Option<i32> {
        apps::ax_frontmost_pid(&self.shared)
    }

    pub fn set_event_sink(&mut self, sink: EventSink) {
        *self.shared.sink.borrow_mut() = Some(sink);
    }

    /// Workspace (launch/terminate/activate), screen-change and trust events.
    /// Per-app window events need `observe_app`. Idempotent.
    pub fn start_observing(&mut self) {
        if self.observing.is_none() {
            self.observing = Some(observers::Global::start(&self.shared));
        }
    }

    /// Install the global key tap; `NotTrusted` until Accessibility is granted.
    /// Replaces any previous tap.
    pub fn install_key_tap(&mut self, sink: KeySink) -> Result<(), PlatformError> {
        self.key_tap = None;
        self.key_tap = Some(KeyTap::install(sink)?);
        Ok(())
    }
}

impl Shared {
    pub(crate) fn emit(&self, ev: PlatformEvent) {
        // A sink that re-enters (emits from inside itself) drops the inner event.
        match self.sink.try_borrow_mut() {
            Ok(mut s) => {
                if let Some(f) = s.as_mut() {
                    f(ev);
                }
            }
            Err(_) => tracing::warn!(?ev, "event sink busy; dropped"),
        }
    }

    fn app_el(&self, pid: i32) -> AxElement {
        self.apps
            .borrow_mut()
            .entry(pid)
            .or_insert_with(|| AppEntry {
                el: AxElement::application(pid),
                observer: None,
            })
            .el
            .clone()
    }

    fn win(&self, w: WindowHandle) -> Result<(i32, AxElement), PlatformError> {
        self.windows
            .borrow()
            .get(&w)
            .map(|e| (e.pid, e.el.clone()))
            .ok_or(PlatformError::NoWindow)
    }

    /// Map an AX error on window `w`; a dead element is evicted from the table.
    fn win_err(&self, w: WindowHandle, e: AXError) -> PlatformError {
        if e == AXError::InvalidUIElement {
            self.windows.borrow_mut().remove(&w);
            return PlatformError::NoWindow;
        }
        to_platform_error(e)
    }

    fn track(&self, pid: i32, el: &AxElement) -> Option<WindowHandle> {
        let h = WindowHandle(el.window_id().ok()? as u64);
        self.windows.borrow_mut().insert(
            h,
            WinEntry {
                pid,
                el: el.clone(),
            },
        );
        Some(h)
    }

    fn read_frame(&self, el: &AxElement) -> AxResult<Rect> {
        let v = el.attrs(&self.frame_attrs)?;
        let p = v[0]
            .as_deref()
            .and_then(ax::as_point)
            .ok_or(AXError::NoValue)?;
        let s = v[1]
            .as_deref()
            .and_then(ax::as_size)
            .ok_or(AXError::NoValue)?;
        Ok(Rect::new(p.x, p.y, s.width, s.height))
    }

    fn window_info(&self, pid: i32, el: &AxElement) -> Option<WindowInfo> {
        let v = el.attrs(&self.window_attrs).ok()?;
        let s = |i: usize| v[i].as_deref().and_then(ax::as_string);
        let b = |i: usize| v[i].as_deref().and_then(ax::as_bool).unwrap_or(false);
        let role = s(0)?;
        if role != "AXWindow" {
            return None;
        }
        let p = v[3].as_deref().and_then(ax::as_point)?;
        let sz = v[4].as_deref().and_then(ax::as_size)?;
        Some(WindowInfo {
            handle: self.track(pid, el)?,
            pid,
            title: s(2).unwrap_or_default(),
            role,
            subrole: s(1).unwrap_or_default(),
            frame: Rect::new(p.x, p.y, sz.width, sz.height),
            minimized: b(5),
            fullscreen: b(6),
            resizable: el.is_settable("AXSize"),
        })
    }

    fn write_frame(el: &AxElement, r: Rect, order: SetOrder) -> AxResult<()> {
        let pos = CGPoint { x: r.x, y: r.y };
        let size = CGSize {
            width: r.w,
            height: r.h,
        };
        match order {
            SetOrder::SizePos => {
                el.set_size("AXSize", size)?;
                el.set_point("AXPosition", pos)
            }
            SetOrder::PosSizePos => {
                el.set_point("AXPosition", pos)?;
                el.set_size("AXSize", size)?;
                el.set_point("AXPosition", pos)
            }
        }
    }
}

impl Platform for MacPlatform {
    fn screens(&self) -> Vec<MonitorInfo> {
        screens::screens(self.mtm)
    }

    fn running_apps(&self) -> Vec<AppInfo> {
        apps::running_apps()
    }

    /// Only windows on the current Space are visible to `AXWindows`.
    fn app_windows(&mut self, pid: i32) -> Vec<WindowInfo> {
        if pid <= 0 {
            return Vec::new();
        }
        let sh = &self.shared;
        match sh.app_el(pid).attr_elements("AXWindows") {
            Ok(ws) => ws.iter().filter_map(|w| sh.window_info(pid, w)).collect(),
            Err(e) => {
                tracing::debug!(pid, err = e.0, "AXWindows failed");
                Vec::new()
            }
        }
    }

    fn window_frame(&self, w: WindowHandle) -> Option<Rect> {
        let sh = &self.shared;
        let (_, el) = sh.win(w).ok()?;
        sh.read_frame(&el).map_err(|e| sh.win_err(w, e)).ok()
    }

    /// AX reports success even when the app ignores the write; the caller
    /// verifies with `window_frame`.
    fn set_window_frame(
        &self,
        w: WindowHandle,
        r: Rect,
        order: SetOrder,
    ) -> Result<(), PlatformError> {
        let sh = &self.shared;
        let (pid, el) = sh.win(w)?;
        // Chromium/Electron animate frame changes while AXEnhancedUserInterface
        // is on (set by assistive tools); turn it off for the write.
        let app = sh.app_el(pid);
        let eui = app.attr_bool("AXEnhancedUserInterface").unwrap_or(false);
        if eui {
            let _ = app.set_bool("AXEnhancedUserInterface", false);
        }
        let res = Shared::write_frame(&el, r, order);
        if eui {
            let _ = app.set_bool("AXEnhancedUserInterface", true);
        }
        res.map_err(|e| sh.win_err(w, e))
    }

    fn raise(&self, w: WindowHandle) -> Result<(), PlatformError> {
        let sh = &self.shared;
        let (pid, el) = sh.win(w)?;
        // Since macOS 14 `activateWithOptions` from a background process may be
        // declined; AXFrontmost is the public fallback that still works.
        apps::activate(pid);
        let _ = sh.app_el(pid).set_bool("AXFrontmost", true);
        let _ = el.set_bool("AXMain", true);
        el.perform("AXRaise").map_err(|e| sh.win_err(w, e))
    }

    fn set_minimized(&self, w: WindowHandle, on: bool) -> Result<(), PlatformError> {
        let (_, el) = self.shared.win(w)?;
        el.set_bool("AXMinimized", on)
            .map_err(|e| self.shared.win_err(w, e))
    }

    fn close(&self, w: WindowHandle) -> Result<(), PlatformError> {
        let (_, el) = self.shared.win(w)?;
        el.attr_element("AXCloseButton")
            .and_then(|b| b.perform("AXPress"))
            .map_err(|e| self.shared.win_err(w, e))
    }

    fn focused_window(&self) -> Option<WindowHandle> {
        // Keyboard focus per Accessibility first. AppKit's idea of the
        // frontmost app can be a stale record (pid -1) for an app that
        // re-exec'd itself, and the window server's front window may be a
        // floating utility window, so those come after.
        if let Some(pid) = apps::ax_focused_pid()
            && let Ok(el) = self.shared.app_el(pid).attr_element("AXFocusedWindow")
            && let Some(h) = self.shared.track(pid, &el)
        {
            return Some(h);
        }
        if let Some(pid) = apps::ax_frontmost_pid(&self.shared)
            && let Ok(el) = self.shared.app_el(pid).attr_element("AXFocusedWindow")
            && let Some(h) = self.shared.track(pid, &el)
        {
            return Some(h);
        }
        if let Some((pid, id)) = apps::front_window() {
            let h = WindowHandle(u64::from(id));
            if self.shared.windows.borrow().contains_key(&h) {
                return Some(h);
            }
            if let Ok(el) = self.shared.app_el(pid).attr_element("AXFocusedWindow")
                && let Some(h) = self.shared.track(pid, &el)
            {
                return Some(h);
            }
        }
        let pid = apps::frontmost_pid().or_else(apps::menu_bar_owner_pid)?;
        let el = self
            .shared
            .app_el(pid)
            .attr_element("AXFocusedWindow")
            .ok()?;
        self.shared.track(pid, &el)
    }

    fn observe_app(&mut self, pid: i32) -> Result<(), PlatformError> {
        observers::observe_app(&self.shared, pid)
    }

    fn unobserve_app(&mut self, pid: i32) {
        self.shared.apps.borrow_mut().remove(&pid);
        self.shared.windows.borrow_mut().retain(|_, e| e.pid != pid);
    }

    fn launch(&self, bundle_id: &str) -> Result<(), PlatformError> {
        apps::launch(bundle_id)
    }

    fn show_overlay(&mut self, content: Option<&OverlayContent>, monitor: &MonitorInfo) {
        self.overlay.show(self.mtm, content, monitor);
    }

    fn mark_monitor(&mut self, monitor: Option<&MonitorInfo>) {
        self.mark.show(self.mtm, monitor);
    }

    fn set_status(&mut self, text: &str) {
        self.status.set(self.mtm, text);
    }

    fn mouse_location(&self) -> Option<tatami_protocol::Point> {
        // AppKit reports the pointer in bottom-left coordinates of the primary display.
        let p = objc2_app_kit::NSEvent::mouseLocation();
        let main_h = objc2_core_graphics::CGDisplayBounds(objc2_core_graphics::CGMainDisplayID())
            .size
            .height;
        Some(tatami_protocol::Point {
            x: p.x,
            y: main_h - p.y,
        })
    }

    fn warp_mouse(&self, p: tatami_protocol::Point) {
        // CG's global space is AX's: top-left origin of the primary display.
        let err = objc2_core_graphics::CGWarpMouseCursorPosition(CGPoint { x: p.x, y: p.y });
        if err != objc2_core_graphics::CGError::Success {
            tracing::debug!(?err, "warp mouse failed");
        }
    }

    fn set_menu(&mut self, items: &[MenuItem]) {
        self.status.set_menu(self.mtm, items);
    }
}
