//! Where the daemon listens. `$XDG_RUNTIME_DIR/tatami/sock`, else
//! `~/.tatami/run/sock`, else `/tmp/tatami-$USER/sock`. The directory is
//! owner-only; the daemon chmods the socket to 0600 after bind and checks
//! the peer uid, because the socket grants control of every window.

use std::path::PathBuf;

pub fn socket_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(xdg).join("tatami");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".tatami").join("run");
    }
    PathBuf::from("/tmp").join(format!(
        "tatami-{}",
        std::env::var("USER").unwrap_or_else(|_| "anon".into())
    ))
}

pub fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("TATAMI_SOCK") {
        return PathBuf::from(p);
    }
    socket_dir().join("sock")
}
