//! Config hot reload. A debounced watcher on the config file's directory
//! (editors replace the file, so watching the path itself loses it after
//! the first save) posts `ReloadConfig`; the main thread re-reads and swaps
//! the keymap. The watcher handle must be kept alive by the caller.

use std::path::Path;
use std::sync::mpsc::Sender;
use std::time::Duration;

use notify_debouncer_mini::{
    DebounceEventResult, Debouncer, new_debouncer, notify::RecommendedWatcher,
    notify::RecursiveMode,
};

use crate::MainMsg;
use crate::ipc::Waker;

pub struct ConfigWatcher {
    _debouncer: Debouncer<RecommendedWatcher>,
}

pub fn start(config_path: &Path, tx: Sender<MainMsg>, wake: Waker) -> Option<ConfigWatcher> {
    let dir = config_path.parent()?.to_path_buf();
    // Watch the directory even before a config exists, so the first file
    // the user writes is picked up without `tatami reload-config`.
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("config dir {}: {e}; hot reload off", dir.display());
        return None;
    }
    let file_name = config_path.file_name()?.to_os_string();
    let keymaps_dir = dir.join("keymaps");
    let mut debouncer = new_debouncer(
        Duration::from_millis(150),
        move |res: DebounceEventResult| {
            let Ok(events) = res else { return };
            let relevant = events.iter().any(|e| {
                e.path.file_name() == Some(file_name.as_os_str())
                    || e.path.starts_with(&keymaps_dir)
            });
            if relevant && tx.send(MainMsg::ReloadConfig).is_ok() {
                wake();
            }
        },
    )
    .ok()?;
    debouncer
        .watcher()
        .watch(&dir, RecursiveMode::Recursive)
        .ok()?;
    Some(ConfigWatcher {
        _debouncer: debouncer,
    })
}
