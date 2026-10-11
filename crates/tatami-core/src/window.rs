//! What the core remembers about a window and a monitor.

use tatami_protocol::{Rect, Size, WindowHandle};

use crate::platform::{AppInfo, MonitorInfo, WindowInfo};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowKind {
    Tiled,
    Floating,
    /// Our fullscreen: fills the monitor but keeps its slot in the tree so
    /// leaving fullscreen restores the layout.
    Fullscreen,
    /// macOS native fullscreen (its own Space); we leave it alone.
    NativeFullscreen,
    /// Dialogs, panels and anything else we never move.
    Unmanaged,
}

impl WindowKind {
    /// Kinds that own a leaf in the workspace tree.
    pub fn in_tree(self) -> bool {
        matches!(self, WindowKind::Tiled | WindowKind::Fullscreen)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub handle: WindowHandle,
    pub pid: i32,
    pub bundle_id: Option<String>,
    pub app_name: String,
    pub title: String,
    pub subrole: String,
    /// Last frame the platform reported.
    pub frame: Rect,
    pub kind: WindowKind,
    /// Frame we last asked for; a move/resize event matching it is our own echo.
    pub expected: Option<Rect>,
    /// Size the app insists on (it refused ours); layout stops fighting it.
    pub stubborn: Option<Size>,
    pub minimized: bool,
}

impl Window {
    pub fn new(info: &WindowInfo, app: &AppInfo, kind: WindowKind) -> Self {
        Self {
            handle: info.handle,
            pid: info.pid,
            bundle_id: app.bundle_id.clone(),
            app_name: app.name.clone(),
            title: info.title.clone(),
            subrole: info.subrole.clone(),
            frame: info.frame,
            kind,
            expected: None,
            stubborn: None,
            minimized: info.minimized,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Monitor {
    pub info: MonitorInfo,
}
