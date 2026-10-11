//! Wire types. The CLI forwards its argv untouched in `Request::args`; the
//! daemon parses it with the same `Action` parser the keymap uses, so every
//! CLI command is bindable and vice versa.

use serde::{Deserialize, Serialize};

/// Opaque per-process window identity handed out by the platform layer
/// (the CGWindowID on macOS). Never persisted across daemon runs as an
/// identity; the state file matches windows by app and title instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WindowHandle(pub u64);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub id: u64,
    pub args: Vec<String>,
    /// Client's `PROTOCOL_VERSION`; the daemon answers a mismatch with an
    /// error naming both versions and never restarts itself.
    #[serde(default)]
    pub protocol: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    #[serde(default)]
    pub id: u64,
    pub ok: bool,
    /// Human text for the terminal; empty when `data` says it all.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stdout: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    /// Structured payload for `--json` style consumers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl Response {
    pub fn ok(id: u64, stdout: impl Into<String>) -> Self {
        Self {
            id,
            ok: true,
            stdout: stdout.into(),
            error: String::new(),
            data: None,
        }
    }
    pub fn with_data(id: u64, stdout: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            id,
            ok: true,
            stdout: stdout.into(),
            error: String::new(),
            data: Some(data),
        }
    }
    pub fn err(id: u64, error: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            stdout: String::new(),
            error: error.into(),
            data: None,
        }
    }
}

/// Events streamed to a `subscribe` connection, one frame each.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum IpcEvent {
    WorkspaceChanged {
        monitor: String,
        workspace: String,
        prev: Option<String>,
    },
    FocusChanged {
        window: Option<WindowHandle>,
        app: Option<String>,
        title: Option<String>,
        monitor: String,
    },
    ModeChanged {
        mode: Option<String>,
    },
    MonitorsChanged {
        monitors: Vec<String>,
    },
    Restore {
        pending: usize,
        total: usize,
    },
    ConfigReloaded {
        ok: bool,
        message: String,
    },
}

impl IpcEvent {
    /// The topic name used by `subscribe <topic>...`.
    pub fn topic(&self) -> &'static str {
        match self {
            IpcEvent::WorkspaceChanged { .. } => "workspace",
            IpcEvent::FocusChanged { .. } => "focus",
            IpcEvent::ModeChanged { .. } => "mode",
            IpcEvent::MonitorsChanged { .. } => "monitors",
            IpcEvent::Restore { .. } => "restore",
            IpcEvent::ConfigReloaded { .. } => "config",
        }
    }
    pub const TOPICS: &'static [&'static str] = &[
        "workspace",
        "focus",
        "mode",
        "monitors",
        "restore",
        "config",
    ];
}
