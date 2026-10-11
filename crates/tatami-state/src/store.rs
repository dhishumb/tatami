//! Atomic save (temp file in the same directory, then rename) and load with
//! a version check. A corrupt file is renamed aside rather than deleted, so
//! a bad write never costs the user their layout silently.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::schema::{STATE_VERSION, SavedState};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("{path}: {source}")]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("{path}: state version {found} is newer than this daemon's {STATE_VERSION}")]
    Version { path: PathBuf, found: u32 },
}

pub fn save(path: &Path, state: &SavedState) -> Result<(), StoreError> {
    let io_err = |source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    };
    let dir = path
        .parent()
        .ok_or_else(|| io_err(io::Error::other("state path has no parent")))?;
    fs::create_dir_all(dir).map_err(io_err)?;
    let body = serde_json::to_vec_pretty(state).map_err(|source| StoreError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    let tmp = tempfile::Builder::new()
        .prefix(".state-")
        .suffix(".tmp")
        .tempfile_in(dir)
        .map_err(io_err)?;
    fs::write(tmp.path(), &body).map_err(io_err)?;
    tmp.persist(path).map_err(|e| io_err(e.error))?;
    Ok(())
}

/// `Ok(None)` when there is no file yet.
pub fn load(path: &Path) -> Result<Option<SavedState>, StoreError> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(StoreError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let state: SavedState = match serde_json::from_slice(&bytes) {
        Ok(s) => s,
        Err(source) => {
            let aside = path.with_extension("corrupt.json");
            tracing::warn!(
                "{}: unreadable ({source}); moved to {}",
                path.display(),
                aside.display()
            );
            let _ = fs::rename(path, &aside);
            return Err(StoreError::Json {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if state.version > STATE_VERSION {
        return Err(StoreError::Version {
            path: path.to_path_buf(),
            found: state.version,
        });
    }
    Ok(Some(state))
}

/// Names of saved checkpoints, newest first.
pub fn list_checkpoints(dir: &Path) -> Vec<(String, std::time::SystemTime)> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, std::time::SystemTime)> = rd
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let stem = p.file_stem()?.to_str()?.to_string();
            (p.extension()?.to_str()? == "json" && !stem.starts_with('.')).then(|| {
                (
                    stem,
                    e.metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::UNIX_EPOCH),
                )
            })
        })
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::*;
    use tatami_protocol::{Layout, Rect};

    fn sample() -> SavedState {
        SavedState {
            version: STATE_VERSION,
            saved_at: String::new(),
            model: "per-monitor".into(),
            focused_monitor: None,
            monitors: vec![SavedMonitor {
                uuid: "u".into(),
                name: "n".into(),
                frame: Rect::new(0.0, 0.0, 10.0, 10.0),
                active: "1".into(),
                workspaces: vec![SavedWorkspace {
                    name: "1".into(),
                    root: SavedNode::Container {
                        layout: Layout::HTiles,
                        weight: 1.0,
                        children: vec![],
                    },
                    floating: vec![],
                    focused: None,
                }],
            }],
        }
    }

    #[test]
    fn save_load_round_trip_and_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("deep").join("state.json");
        assert!(load(&p).unwrap().is_none());
        save(&p, &sample()).unwrap();
        assert_eq!(load(&p).unwrap().unwrap(), sample());
        // No temp files left behind.
        let leftovers: Vec<_> = fs::read_dir(p.parent().unwrap())
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(leftovers.len(), 1);
    }

    #[test]
    fn corrupt_file_is_set_aside_and_newer_version_refused() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.json");
        fs::write(&p, b"{ not json").unwrap();
        assert!(matches!(load(&p), Err(StoreError::Json { .. })));
        assert!(!p.exists());
        assert!(p.with_extension("corrupt.json").exists());
        let mut s = sample();
        s.version = STATE_VERSION + 1;
        save(&p, &s).unwrap();
        assert!(matches!(load(&p), Err(StoreError::Version { .. })));
    }

    #[test]
    fn checkpoints_listed_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        save(&dir.path().join("old.json"), &sample()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        save(&dir.path().join("new.json"), &sample()).unwrap();
        fs::write(dir.path().join("notes.txt"), b"x").unwrap();
        let names: Vec<_> = list_checkpoints(dir.path())
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names, vec!["new", "old"]);
    }
}
