//! The handoff: what one tatamid image tells the next across `execv`.
//!
//! A hot swap replaces the running daemon with the `tatamid` on disk under
//! the same pid, so launchd sees one long-lived process, no window moves
//! (parked ones stay parked) and the Accessibility grant, keyed to the
//! signing identity, carries over. Two things cross the exec: the
//! listening socket, as the new image's standard input (the kernel queues
//! clients in its backlog meanwhile, so `tatami` never sees a refusal),
//! and this manifest, written next to the socket and named on the command
//! line (`--adopt <path>`). The manifest carries the layout, as a
//! `SavedState`, because a disabled `restore` must not lose it, and the
//! `enable` flag. Window identity crosses on its own: the restore matcher
//! finds every window again by pid and window id.
//!
//! `tatamid --version` prints `handoff <format>` on its second line, and a
//! swap refuses a binary that does not: a truncated file or a build from
//! before hot swap would ignore `--adopt`, find the socket taken, and exit,
//! taking the window manager with it.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tatami_state::SavedState;

/// Bumps when the manifest's shape changes incompatibly. Fields added with
/// `#[serde(default)]` do not need a bump.
pub const HANDOFF_FORMAT: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub handoff: u32,
    /// The writer's release, for the log.
    pub version: String,
    /// `enable` state; a disabled daemon stays disabled across the swap.
    #[serde(default = "yes")]
    pub enabled: bool,
    pub state: SavedState,
}

fn yes() -> bool {
    true
}

/// Next to the socket, in the owner-only runtime dir.
pub fn path() -> PathBuf {
    tatami_protocol::socket_path().with_file_name("handoff")
}

/// How the last swap went, for whoever asked without a socket to hear
/// back on (the installer, after `SIGUSR1`). The new image writes
/// `ok <version>` once it is serving; the old one `failed <why>` when it
/// gives up and keeps serving.
pub fn status_path() -> PathBuf {
    tatami_protocol::socket_path().with_file_name("swap-status")
}

pub fn write_status(line: &str) {
    let p = status_path();
    let tmp = p.with_extension("tmp");
    if std::fs::write(&tmp, format!("{line}\n")).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

/// Mode 0600, written whole and renamed into place.
pub fn write(path: &Path, m: &Manifest) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let bytes = serde_json::to_vec(m).map_err(|e| format!("encode manifest: {e}"))?;
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| format!("create {}: {e}", tmp.display()))?;
    f.write_all(&bytes)
        .and_then(|()| f.sync_all())
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    drop(f);
    std::fs::rename(&tmp, path).map_err(|e| format!("rename to {}: {e}", path.display()))
}

/// Read and delete in one step: a manifest is for the one image that
/// inherited the socket with it.
pub fn read_and_remove(path: &Path) -> Result<Manifest, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let _ = std::fs::remove_file(path);
    let m: Manifest =
        serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    if m.handoff != HANDOFF_FORMAT {
        return Err(format!(
            "manifest is handoff format {}, this build reads {HANDOFF_FORMAT}",
            m.handoff
        ));
    }
    Ok(m)
}

/// What `tatamid --version` must say for a swap to it to be safe.
pub fn check_version_output(out: &str) -> Result<String, String> {
    let mut lines = out.lines();
    let version = match lines.next().and_then(|l| l.strip_prefix("tatamid ")) {
        Some(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => return Err(format!("not a tatamid (`--version` said {out:?})")),
    };
    let fmt = lines
        .find_map(|l| l.strip_prefix("handoff "))
        .and_then(|f| f.trim().parse::<u32>().ok());
    match fmt {
        Some(f) if f == HANDOFF_FORMAT => Ok(version),
        Some(f) => Err(format!(
            "tatamid {version} speaks handoff {f}; this build hands off {HANDOFF_FORMAT}"
        )),
        None => Err(format!(
            "tatamid {version} predates hot swap; restart it instead"
        )),
    }
}

/// Run `<exe> --version` and check it before anything is touched.
pub fn preflight(exe: &Path) -> Result<String, String> {
    let out = std::process::Command::new(exe)
        .arg("--version")
        .output()
        .map_err(|e| format!("run {} --version: {e}", exe.display()))?;
    if !out.status.success() {
        return Err(format!(
            "{} --version exited with {}",
            exe.display(),
            out.status
        ));
    }
    check_version_output(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tatami_state::STATE_VERSION;

    fn sample() -> Manifest {
        Manifest {
            handoff: HANDOFF_FORMAT,
            version: "0.1.0".into(),
            enabled: false,
            state: SavedState {
                version: STATE_VERSION,
                saved_at: String::new(),
                model: "per-monitor".into(),
                focused_monitor: None,
                monitors: Vec::new(),
            },
        }
    }

    #[test]
    fn manifest_round_trips_and_is_consumed() {
        let dir = std::env::temp_dir().join(format!("tatami-handoff-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("handoff");
        write(&p, &sample()).unwrap();
        assert_eq!(read_and_remove(&p).unwrap(), sample());
        assert!(!p.exists(), "read once, then gone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_foreign_format_is_refused() {
        let dir = std::env::temp_dir().join(format!("tatami-handoff2-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("handoff");
        let mut m = sample();
        m.handoff = HANDOFF_FORMAT + 1;
        write(&p, &m).unwrap();
        assert!(read_and_remove(&p).unwrap_err().contains("handoff format"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn version_output_must_name_tatamid_and_the_handoff_format() {
        assert_eq!(
            check_version_output("tatamid 0.2.0\nhandoff 1\n").unwrap(),
            "0.2.0"
        );
        assert!(
            check_version_output("tatamid 0.1.0\n")
                .unwrap_err()
                .contains("predates")
        );
        assert!(
            check_version_output("tatamid 0.3.0\nhandoff 2\n")
                .unwrap_err()
                .contains("handoff 2")
        );
        assert!(
            check_version_output("plyd 2.0.0\nhandoff 1\n")
                .unwrap_err()
                .contains("not a tatamid")
        );
        assert!(check_version_output("").is_err());
    }
}
