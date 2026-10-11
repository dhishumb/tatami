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

/// Checkpoint names are plain file stems; anything with a separator or a
/// leading dot is refused so a name can never escape the directory.
pub fn checkpoint_path(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.starts_with('.') || name.contains(['/', '\\']) {
        return None;
    }
    Some(checkpoints_dir().join(format!("{name}.json")))
}
