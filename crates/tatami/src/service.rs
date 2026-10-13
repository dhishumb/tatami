//! `tatami service …`: a per-user launchd agent that starts `tatamid` at
//! login and restarts it after a crash, never after `quit`. `install` and
//! `uninstall` are the installer's; `enable`/`disable` toggle start at
//! login without touching a running daemon (the menu's "Start at login");
//! `start`/`stop`/`restart` are the daemon's lifecycle. The agent points
//! at the `tatamid` next to this binary, so installing new binaries in
//! place is enough.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const LABEL: &str = "com.vgupta.tatami";

fn plist_path() -> Option<PathBuf> {
    Some(
        PathBuf::from(std::env::var_os("HOME")?)
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist")),
    )
}

fn daemon_path() -> Option<PathBuf> {
    let me = std::env::current_exe().ok()?;
    let sibling = me.parent()?.join("tatamid");
    sibling.exists().then_some(sibling)
}

fn log_dir() -> PathBuf {
    tatami_state_dir().join("log")
}

/// Mirrors tatami-state's resolution without depending on the crate.
fn tatami_state_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("TATAMI_STATE_DIR") {
        return PathBuf::from(d);
    }
    if let Some(d) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(d).join("tatami");
    }
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state/tatami")
}

fn uid() -> u32 {
    // SAFETY: getuid cannot fail and has no preconditions.
    unsafe { getuid() }
}
unsafe extern "C" {
    fn getuid() -> u32;
}

fn launchctl(args: &[&str]) -> bool {
    Command::new("launchctl")
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn daemon_running() -> bool {
    Command::new("pgrep")
        .args(["-x", "tatamid"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn is_loaded(target: &str) -> bool {
    Command::new("launchctl")
        .args(["print", target])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The agent: start at login, restart only after a crash (`SuccessfulExit
/// = false`), so `tatami quit` stays quit and `tatami service start`
/// brings it back.
fn write_plist(plist: &Path) -> Result<PathBuf, String> {
    let daemon = daemon_path()
        .ok_or("no `tatamid` next to this binary; install both to the same directory")?;
    let logs = log_dir();
    let _ = fs::create_dir_all(&logs);
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key><string>{LABEL}</string>
	<key>ProgramArguments</key><array><string>{daemon}</string></array>
	<key>RunAtLoad</key><true/>
	<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
	<key>ProcessType</key><string>Interactive</string>
	<key>StandardOutPath</key><string>{out}</string>
	<key>StandardErrorPath</key><string>{out}</string>
	<key>EnvironmentVariables</key><dict><key>RUST_LOG</key><string>info</string></dict>
</dict>
</plist>
"#,
        daemon = daemon.display(),
        out = logs.join("tatamid.log").display(),
    );
    if let Some(dir) = plist.parent() {
        let _ = fs::create_dir_all(dir);
    }
    fs::write(plist, body).map_err(|e| format!("write {}: {e}", plist.display()))?;
    Ok(daemon)
}

fn bootstrap(plist: &Path) -> bool {
    launchctl(&[
        "bootstrap",
        &format!("gui/{}", uid()),
        plist.to_str().unwrap_or_default(),
    ])
}

pub fn run(args: &[String]) -> ExitCode {
    let Some(plist) = plist_path() else {
        eprintln!("tatami: HOME is not set");
        return ExitCode::FAILURE;
    };
    let target = format!("gui/{}/{LABEL}", uid());
    match args.first().map(String::as_str) {
        Some("install") => {
            let daemon = match write_plist(&plist) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("tatami: {e}");
                    return ExitCode::FAILURE;
                }
            };
            // Re-bootstrap cleanly if an older agent is loaded.
            let _ = launchctl(&["bootout", &target]);
            if !bootstrap(&plist) {
                eprintln!("tatami: launchctl bootstrap failed");
                return ExitCode::FAILURE;
            }
            println!("installed {} → {}", plist.display(), daemon.display());
            println!("logs: {}", log_dir().join("tatamid.log").display());
            ExitCode::SUCCESS
        }
        Some("uninstall") => {
            let _ = launchctl(&["bootout", &target]);
            match fs::remove_file(&plist) {
                Ok(()) => println!("removed {}", plist.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!("not installed"),
                Err(e) => {
                    eprintln!("tatami: {e}");
                    return ExitCode::FAILURE;
                }
            }
            ExitCode::SUCCESS
        }
        // Start at login, without disturbing a daemon that is running now.
        Some("enable") => {
            if let Err(e) = write_plist(&plist) {
                eprintln!("tatami: {e}");
                return ExitCode::FAILURE;
            }
            if is_loaded(&target) {
                println!("tatamid starts at login (already loaded)");
            } else if daemon_running() {
                println!("tatamid starts at login; the running daemon continues");
            } else if bootstrap(&plist) {
                println!("tatamid starts at login, and is starting now");
            } else {
                eprintln!("tatami: launchctl bootstrap failed; starts at next login");
            }
            ExitCode::SUCCESS
        }
        Some("disable") => {
            match fs::remove_file(&plist) {
                Ok(()) => println!(
                    "tatamid no longer starts at login; a running daemon continues until `tatami quit`"
                ),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!("not installed"),
                Err(e) => {
                    eprintln!("tatami: {e}");
                    return ExitCode::FAILURE;
                }
            }
            ExitCode::SUCCESS
        }
        Some("start") => {
            if daemon_running() {
                println!("tatamid is already running");
                return ExitCode::SUCCESS;
            }
            let ok = if is_loaded(&target) {
                launchctl(&["kickstart", &target])
            } else if plist.exists() {
                bootstrap(&plist)
            } else {
                eprintln!(
                    "tatami: no launchd agent; `tatami service enable` first, or run `tatamid`"
                );
                return ExitCode::FAILURE;
            };
            if ok {
                println!("started {LABEL}");
                ExitCode::SUCCESS
            } else {
                eprintln!("tatami: launchctl could not start {LABEL}");
                ExitCode::FAILURE
            }
        }
        Some("stop") => {
            if !daemon_running() {
                println!("tatamid is not running");
                return ExitCode::SUCCESS;
            }
            // `quit` saves the layout and un-parks every window; launchd
            // does not restart a clean exit.
            crate::request(vec!["quit".into()])
        }
        Some("restart") => {
            if launchctl(&["kickstart", "-k", &target]) {
                println!("restarted {LABEL}");
                ExitCode::SUCCESS
            } else {
                eprintln!("tatami: launchctl kickstart failed; is the service installed?");
                ExitCode::FAILURE
            }
        }
        Some("status") | None => {
            println!(
                "start at login: {} ({})",
                if plist.exists() { "yes" } else { "no" },
                plist.display()
            );
            println!(
                "launchd: {}",
                if is_loaded(&target) {
                    "loaded"
                } else {
                    "not loaded"
                }
            );
            println!(
                "tatamid: {}",
                if daemon_running() {
                    "running"
                } else {
                    "not running"
                }
            );
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!(
                "tatami: unknown service command `{other}` (install|uninstall|enable|disable|start|stop|restart|status)"
            );
            ExitCode::FAILURE
        }
    }
}

/// Quick environment checks that need no daemon.
pub fn doctor() -> ExitCode {
    let sock = tatami_protocol::socket_path();
    println!(
        "socket: {} ({})",
        sock.display(),
        if sock.exists() { "present" } else { "absent" }
    );
    println!(
        "tatamid process: {}",
        if daemon_running() {
            "running"
        } else {
            "not running"
        }
    );
    for other in ["Rectangle", "AeroSpace", "yabai", "skhd", "Amethyst"] {
        let on = Command::new("pgrep")
            .args(["-x", other])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if on {
            println!("warning: {other} is running and will fight over windows or Alt chords");
        }
    }
    let spaces = Command::new("defaults")
        .args(["read", "com.apple.spaces", "spans-displays"])
        .output()
        .ok();
    if let Some(o) = spaces {
        let v = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if !v.is_empty() {
            println!(
                "'Displays have separate Spaces': {}",
                if v == "0" {
                    "off (displays span)"
                } else {
                    "on"
                }
            );
        }
    }
    println!("Accessibility trust is reported by the daemon: `tatami ping`");
    ExitCode::SUCCESS
}
