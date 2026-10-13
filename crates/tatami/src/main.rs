//! `tatami`: the thin client. Everything after the program name is sent to
//! the daemon as one request, untouched, and the daemon parses it with the
//! same `Action` parser the keymap uses. Only a handful of words are handled
//! locally because they make no sense over the socket.

mod service;

use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::time::Duration;

use tatami_protocol::{
    IpcEvent, PROTOCOL_VERSION, Request, Response, read_frame, socket_path, write_frame,
};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("help") | Some("--help") | Some("-h") => {
            print!("{}", HELP);
            ExitCode::SUCCESS
        }
        Some("--version") | Some("-V") => {
            println!("tatami {VERSION} (protocol {PROTOCOL_VERSION})");
            ExitCode::SUCCESS
        }
        Some("socket-path") => {
            println!("{}", socket_path().display());
            ExitCode::SUCCESS
        }
        Some("config-template") => {
            print!("{}", tatami_config::CONFIG_TEMPLATE);
            ExitCode::SUCCESS
        }
        Some("keymap") => keymap(args.get(1).map(String::as_str)),
        Some("keymaps") => {
            for k in tatami_config::BUILTIN_KEYMAPS {
                println!("{k}");
            }
            ExitCode::SUCCESS
        }
        Some("service") => service::run(&args[1..]),
        Some("doctor") => service::doctor(),
        Some("subscribe") => subscribe(args),
        Some(_) => request(args),
    }
}

fn connect() -> Result<UnixStream, String> {
    let path = socket_path();
    let s = UnixStream::connect(&path).map_err(|e| {
        format!(
            "cannot connect to tatamid at {}: {e}\nIs the daemon running? Start it with `tatamid`.",
            path.display()
        )
    })?;
    // A healthy daemon answers in microseconds; a wedged one should not hang the shell.
    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
    Ok(s)
}

pub(crate) fn request(args: Vec<String>) -> ExitCode {
    let mut s = match connect() {
        Ok(s) => s,
        Err(e) => return fail(&e),
    };
    let req = Request {
        id: 1,
        args,
        protocol: PROTOCOL_VERSION,
    };
    if let Err(e) = write_frame(&mut s, &req) {
        return fail(&format!("send: {e}"));
    }
    match read_frame::<_, Response>(&mut s) {
        Ok(Some(r)) => {
            if !r.stdout.is_empty() {
                let mut out = io::stdout().lock();
                let _ = out.write_all(r.stdout.as_bytes());
                if !r.stdout.ends_with('\n') {
                    let _ = out.write_all(b"\n");
                }
            } else if let Some(data) = r.data.as_ref() {
                println!("{}", serde_json::to_string_pretty(data).unwrap_or_default());
            }
            if r.ok {
                ExitCode::SUCCESS
            } else {
                fail(&r.error)
            }
        }
        Ok(None) => fail("daemon closed the connection without answering"),
        Err(e) => fail(&format!("recv: {e}")),
    }
}

/// `subscribe` keeps the connection open and prints one JSON object per line
/// until the daemon goes away, so shell loops can `while read -r line`.
fn subscribe(args: Vec<String>) -> ExitCode {
    let mut s = match connect() {
        Ok(s) => s,
        Err(e) => return fail(&e),
    };
    let _ = s.set_read_timeout(None);
    let req = Request {
        id: 1,
        args,
        protocol: PROTOCOL_VERSION,
    };
    if let Err(e) = write_frame(&mut s, &req) {
        return fail(&format!("send: {e}"));
    }
    // The first frame is the acknowledgement (or the error).
    match read_frame::<_, Response>(&mut s) {
        Ok(Some(r)) if r.ok => {}
        Ok(Some(r)) => return fail(&r.error),
        Ok(None) => return fail("daemon closed the connection"),
        Err(e) => return fail(&format!("recv: {e}")),
    }
    let mut out = io::stdout().lock();
    loop {
        match read_frame::<_, IpcEvent>(&mut s) {
            Ok(Some(ev)) => {
                if writeln!(out, "{}", serde_json::to_string(&ev).unwrap_or_default()).is_err()
                    || out.flush().is_err()
                {
                    return ExitCode::SUCCESS; // reader went away (pipe closed)
                }
            }
            Ok(None) => return ExitCode::SUCCESS,
            Err(e) => return fail(&format!("recv: {e}")),
        }
    }
}

/// Print a preset (built-in, `~/.config/tatami/keymaps/<name>.toml`, or a
/// path) so it can be copied and edited.
fn keymap(name: Option<&str>) -> ExitCode {
    let name = name.unwrap_or(tatami_config::DEFAULT_PRESET);
    match tatami_config::resolve_keymap(name) {
        Some((source, text)) => {
            eprintln!("# {source}");
            print!("{text}");
            ExitCode::SUCCESS
        }
        None => fail(&format!(
            "no keymap `{name}` (built-ins: {})",
            tatami_config::BUILTIN_KEYMAPS.join(", ")
        )),
    }
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("tatami: {msg}");
    ExitCode::FAILURE
}

const HELP: &str = "\
tatami — tiling window manager for macOS

usage: tatami <command> [args...]      send a command to the running daemon
       tatami subscribe <topic>...     stream events as JSON lines (--all for every topic)
       tatami keymap [name]            print a keymap preset to copy and edit
       tatami socket-path              print the daemon socket path
       tatami service enable|disable   start tatamid at login (the launchd agent), or not
       tatami service start|stop|restart|status   the daemon through launchd
       tatami config-template          print the commented default config
       tatami doctor                   environment checks
       tatami --version

Commands are the same words you bind in config.toml: focus left, move right,
workspace 3, move-node-to-workspace 3, layout tiles horizontal vertical,
fullscreen, resize width +50, list-windows --json, reload-config,
checkpoint save <name>, checkpoint restore <name>, desk list|save|restore <id>,
ping, quit,
hot-swap (replace the running daemon with the installed build, in place),
edit-config (open config.toml, writing the template first if needed) ...
Run `tatami list-modes` or see the README for the full list.
";
