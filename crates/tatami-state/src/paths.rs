//! `$TATAMI_STATE_DIR`, else `$XDG_STATE_HOME/tatami`, else `~/.local/state/tatami`.

use std::path::PathBuf;

pub fn state_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("TATAMI_STATE_DIR") {
        return PathBuf::from(d);
    }
    if let Some(d) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(d).join("tatami");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("tatami");
    }
    PathBuf::from("/tmp").join("tatami-state")
}

pub fn state_path() -> PathBuf {
    state_dir().join("state.json")
}

pub fn checkpoints_dir() -> PathBuf {
    state_dir().join("checkpoints")
}

/// One layout per set of monitors (`desk::desk_id_of`).
pub fn desks_dir() -> PathBuf {
    state_dir().join("desks")
}

/// `None` for anything but a desk id (16 hex digits), so a request can
/// never name a file outside the directory.
pub fn desk_path(id: &str) -> Option<PathBuf> {
    (id.len() == 16 && id.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| desks_dir().join(format!("{id}.json")))
}

/// Checkpoint names are plain file stems; anything with a separator or a
/// leading dot is refused so a name can never escape the directory.
pub fn checkpoint_path(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.starts_with('.') || name.contains(['/', '\\']) {
        return None;
    }
    Some(checkpoints_dir().join(format!("{name}.json")))
}
