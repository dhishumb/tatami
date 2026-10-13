//! The vocabulary shared by every tatami crate: geometry, the `Action`
//! command set (what a keybinding runs and what the CLI sends), the IPC
//! request/response/event types, the length-prefixed JSON frame codec, and
//! the socket path.
//!
//! Nothing here touches macOS. Actions are parsed once, when a config or a
//! CLI request arrives; the key path only ever clones an already-parsed
//! `Action`.

pub mod action;
pub mod frame;
pub mod geom;
pub mod ipc;
pub mod sock;

pub use action::{
    Action, ActionError, DeskCmd, Dir, Layout, LayoutArg, MonitorTarget, ProfileCmd, ResizeDelta,
    ResizeDim, SplitArg, Toggle, WorkspaceTarget,
};
pub use frame::{FrameError, MAX_FRAME, read_frame, write_frame};
pub use geom::{Axis, Point, Rect, Size};
pub use ipc::{IpcEvent, Request, Response, WindowHandle};
pub use sock::{socket_dir, socket_path};

/// Bumped whenever `Request`, `Response` or `IpcEvent` change shape.
pub const PROTOCOL_VERSION: u32 = 1;
