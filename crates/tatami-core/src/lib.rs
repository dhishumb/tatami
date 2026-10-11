//! Pure window-manager logic: ids, the layout tree, the workspace model,
//! off-screen parking, and the reactor that turns events into effects.
//! No macOS dependency; everything here is unit-tested against
//! `fake::FakePlatform`.

#[cfg(any(test, feature = "fake"))]
pub mod fake;
pub mod ids;
pub mod layout;
pub mod parking;
pub mod platform;
pub mod reactor;
pub mod tree;
pub mod window;
pub mod workspace;

pub use ids::{MonitorId, NodeId, WindowId, WorkspaceId};
pub use platform::{
    AppInfo, EventSink, KeyDisposition, KeySink, MenuItem, Mods, MonitorInfo, OverlayContent,
    OverlayRow, Platform, PlatformError, PlatformEvent, RawKeyEvent, SetOrder, WindowInfo,
};
pub use reactor::{Outcome, Reactor};
