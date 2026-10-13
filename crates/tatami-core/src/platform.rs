//! The contract between the pure core and the macOS layer. The platform
//! crate implements `Platform` and pushes `PlatformEvent`s; nothing above
//! this line knows what an AXUIElement is.
//!
//! Coordinates are Accessibility coordinates (global, top-left origin).

use std::path::PathBuf;

use tatami_protocol::{Rect, WindowHandle};

#[derive(Debug, Clone, PartialEq)]
pub struct MonitorInfo {
    /// Stable across reboots (`CGDisplayCreateUUIDFromDisplayID`).
    pub uuid: String,
    pub name: String,
    /// Full frame.
    pub frame: Rect,
    /// Frame minus menu bar and dock.
    pub visible: Rect,
    pub is_main: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AppInfo {
    pub pid: i32,
    pub bundle_id: Option<String>,
    pub name: String,
    pub exe: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WindowInfo {
    pub handle: WindowHandle,
    pub pid: i32,
    pub title: String,
    /// `AXWindow`, `AXSheet`, ...
    pub role: String,
    /// `AXStandardWindow`, `AXDialog`, `AXFloatingWindow`, ...
    pub subrole: String,
    pub frame: Rect,
    pub minimized: bool,
    /// macOS native fullscreen (`AXFullScreen`).
    pub fullscreen: bool,
    /// Whether the app lets its size be set (`AXSize` settable). Fixed-size
    /// windows (System Settings, Calculator, most dialogs) float.
    pub resizable: bool,
}

/// Order of AX attribute writes. Some apps clamp a size set before the
/// position or the other way round; `PosSizePos` is the retry order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOrder {
    SizePos,
    PosSizePos,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Mods {
    pub alt: bool,
    pub shift: bool,
    pub ctrl: bool,
    pub cmd: bool,
}

impl Mods {
    pub fn any(self) -> bool {
        self.alt || self.shift || self.ctrl || self.cmd
    }
}

/// What the event tap saw. `keycode` is the hardware virtual keycode; the
/// keymap crate maps it to a key name, so Option-translated characters
/// (`alt+h` → `˙`) never matter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawKeyEvent {
    Down {
        keycode: u16,
        mods: Mods,
        repeat: bool,
    },
    Up {
        keycode: u16,
        mods: Mods,
    },
    /// A modifier changed. `mods` is the new state.
    Flags {
        mods: Mods,
    },
}

/// Answered synchronously inside the event tap callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyDisposition {
    Pass,
    Swallow,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlatformEvent {
    AppLaunched(AppInfo),
    AppTerminated(i32),
    AppActivated(i32),
    WindowCreated(WindowInfo),
    WindowDestroyed(WindowHandle),
    WindowFocused(WindowHandle),
    WindowMoved(WindowHandle, Rect),
    WindowResized(WindowHandle, Rect),
    WindowMinimized(WindowHandle, bool),
    TitleChanged(WindowHandle, String),
    /// Monitors added, removed or rearranged; call `Platform::screens`.
    ScreensChanged,
    /// Accessibility trust was granted (or lost).
    TrustChanged(bool),
    /// The user picked a menu item (or another platform UI) that maps to a
    /// command line, e.g. `["workspace", "3"]`. Routed like a CLI request.
    Command(Vec<String>),
}

/// One row of the status item's menu. `command` is empty for a disabled
/// header row; `separator` rows have no title.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MenuItem {
    pub title: String,
    pub command: Vec<String>,
    pub checked: bool,
    pub separator: bool,
    /// Indent level for grouping (workspaces under a monitor header).
    pub indent: u8,
}

impl MenuItem {
    pub fn separator() -> Self {
        Self {
            separator: true,
            ..Default::default()
        }
    }
    pub fn header(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            ..Default::default()
        }
    }
    pub fn action(title: impl Into<String>, command: &[&str]) -> Self {
        Self {
            title: title.into(),
            command: command.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }
}

/// Called on the main thread for every event.
pub type EventSink = Box<dyn FnMut(PlatformEvent)>;
/// Called on the main thread from inside the event tap. Must be fast and
/// must not touch the platform.
pub type KeySink = Box<dyn FnMut(RawKeyEvent) -> KeyDisposition>;

#[derive(Debug, Clone, PartialEq)]
pub struct OverlayRow {
    pub key: String,
    pub desc: String,
    /// A row that opens another level, drawn in the second accent.
    pub prefix: bool,
    /// Run when the row is clicked (as `PlatformEvent::Command`); empty =
    /// not clickable.
    pub command: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct OverlayContent {
    pub title: String,
    /// How to leave, shown dim on the right (`Esc  back`).
    pub exit_hint: String,
    pub rows: Vec<OverlayRow>,
    /// At most this many columns (`None`: as many as fit).
    pub columns: Option<usize>,
    /// Description cut-off in characters (`None`: the sheet's default).
    pub max_desc: Option<usize>,
    /// A list to read (the overview) rather than a sheet to glance at:
    /// no arrows between key and text, a fixed highlight column, up to
    /// half the screen.
    pub list: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlatformError {
    #[error("no such window")]
    NoWindow,
    #[error("no such app (pid {0})")]
    NoApp(i32),
    #[error("accessibility not trusted")]
    NotTrusted,
    #[error("ax error {0}")]
    Ax(i32),
    #[error("{0}")]
    Other(String),
}

/// Everything the reactor needs from the OS. All calls happen on the main
/// thread. Implementations must never block for long: the event tap is
/// disabled by macOS if the main loop stalls.
pub trait Platform {
    fn screens(&self) -> Vec<MonitorInfo>;
    fn running_apps(&self) -> Vec<AppInfo>;
    /// Windows of one app, in no particular order. Only windows worth
    /// managing are returned (role `AXWindow`); the reactor filters by subrole.
    fn app_windows(&mut self, pid: i32) -> Vec<WindowInfo>;
    fn window_frame(&self, w: WindowHandle) -> Option<Rect>;
    fn set_window_frame(
        &self,
        w: WindowHandle,
        r: Rect,
        order: SetOrder,
    ) -> Result<(), PlatformError>;
    /// Activate the owning app, raise the window, make it main.
    fn raise(&self, w: WindowHandle) -> Result<(), PlatformError>;
    fn set_minimized(&self, w: WindowHandle, on: bool) -> Result<(), PlatformError>;
    fn close(&self, w: WindowHandle) -> Result<(), PlatformError>;
    fn focused_window(&self) -> Option<WindowHandle>;
    /// Start delivering `PlatformEvent`s for this app's windows.
    fn observe_app(&mut self, pid: i32) -> Result<(), PlatformError>;
    fn unobserve_app(&mut self, pid: i32);
    /// Launch (or do nothing if running) without activating.
    fn launch(&self, bundle_id: &str) -> Result<(), PlatformError>;
    /// `None` hides the which-key sheet.
    fn show_overlay(&mut self, content: Option<&OverlayContent>, monitor: &MonitorInfo);
    /// Menu bar status item text (shown next to the icon).
    fn set_status(&mut self, text: &str);
    /// Replace the status item's menu. Picking an item emits
    /// `PlatformEvent::Command(item.command)`.
    fn set_menu(&mut self, items: &[MenuItem]);
    /// Where the pointer is, in AX coordinates. "Where control is" when
    /// macOS cannot tell us which app has focus.
    fn mouse_location(&self) -> Option<tatami_protocol::Point>;
    /// Move the pointer (AX coordinates). Keyboard focus crossing to another
    /// monitor brings the pointer along, so "act where the pointer is" and
    /// the keys agree.
    fn warp_mouse(&self, p: tatami_protocol::Point);
}
