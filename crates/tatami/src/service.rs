//! `tatami service install|uninstall|restart|status`: a per-user launchd
//! agent that keeps `tatamid` running and starts it at login. The agent
//! points at the `tatamid` next to this binary, so installing new binaries
//! in place is enough; `restart` picks them up.

use std::fs;
use std::path::PathBuf;
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

pub fn run(args: &[String]) -> ExitCode {
    let Some(plist) = plist_path() else {
        eprintln!("tatami: HOME is not set");
        return ExitCode::FAILURE;
    };
    let target = format!("gui/{}/{LABEL}", uid());
    match args.first().map(String::as_str) {
        Some("install") => {
            let Some(daemon) = daemon_path() else {
                eprintln!(
                    "tatami: no `tatamid` next to this binary; install both to the same directory"
                );
                return ExitCode::FAILURE;
            };
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
	<key>KeepAlive</key><true/>
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
            if let Err(e) = fs::write(&plist, body) {
                eprintln!("tatami: write {}: {e}", plist.display());
                return ExitCode::FAILURE;
            }
            // Re-bootstrap cleanly if an older agent is loaded.
            let _ = launchctl(&["bootout", &target]);
            if !launchctl(&[
                "bootstrap",
                &format!("gui/{}", uid()),
                plist.to_str().unwrap_or_default(),
            ]) {
                eprintln!("tatami: launchctl bootstrap failed");
                return ExitCode::FAILURE;
            }
            println!("installed {} → {}", plist.display(), daemon.display());
            println!("logs: {}", logs.join("tatamid.log").display());
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
                "plist: {} ({})",
                plist.display(),
                if plist.exists() { "present" } else { "absent" }
            );
            let loaded = Command::new("launchctl")
                .args(["print", &target])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            println!("launchd: {}", if loaded { "loaded" } else { "not loaded" });
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!(
                "tatami: unknown service command `{other}` (install|uninstall|restart|status)"
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
    let running = Command::new("pgrep")
        .args(["-x", "tatamid"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    println!(
        "tatamid process: {}",
        if running { "running" } else { "not running" }
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
