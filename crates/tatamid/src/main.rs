//! `tatamid`: the daemon. Owns every window, workspace and keybinding on the
//! main thread's run loop; the socket server, signal handler and config
//! watcher run on std threads and hand work back through one channel.
//!
//! Everything that touches the reactor goes through `Daemon::step`, which
//! borrows the single `RefCell` once, runs one closure, handles the
//! outcomes and re-arms the timer. Platform callbacks only queue and poke.

mod handoff;
mod ipc;
mod signals;
mod watch;

// Gives the bare binary a bundle identity so the Accessibility permission
// is granted to "tatami" and survives rebuilds of a signed binary.
embed_plist::embed_info_plist!("../Info.plist");

use std::cell::RefCell;
use std::collections::VecDeque;
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::rc::{Rc, Weak};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use tatami_core::reactor::{Outcome, Reactor};
use tatami_core::{KeyDisposition, PlatformEvent};
use tatami_platform::MacPlatform;
use tatami_platform::runloop::MainLoop;
use tatami_protocol::{Action, CheckpointCmd, IpcEvent, Response, socket_path};
use tatami_state::SavedState;

use ipc::IpcMsg;

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Layout changes are saved this long after the last one settles.
const AUTOSAVE_DEBOUNCE: Duration = Duration::from_secs(1);
/// And in any case this often, when the bytes changed.
const AUTOSAVE_PERIOD: Duration = Duration::from_secs(5);

/// Everything the main thread can be asked to do from another thread.
pub enum MainMsg {
    Ipc(IpcMsg),
    ReloadConfig,
    Shutdown(String),
    /// Replace this process with the `tatamid` on disk (the reason, for the log).
    HotSwap(String),
}

impl From<IpcMsg> for MainMsg {
    fn from(m: IpcMsg) -> Self {
        MainMsg::Ipc(m)
    }
}

struct Daemon {
    reactor: Reactor<MacPlatform>,
    main_loop: MainLoop,
    ipc: ipc::IpcServer,
    rx: Receiver<MainMsg>,
    /// Platform events queued by callbacks while the daemon was busy.
    pending: Rc<RefCell<VecDeque<PlatformEvent>>>,
    subscribers: Vec<(Vec<String>, Sender<IpcEvent>)>,
    config_path: PathBuf,
    state_path: PathBuf,
    dirty_since: Option<Instant>,
    last_periodic: Instant,
    last_saved: Vec<u8>,
    tap_installed: bool,
    /// Set by outcomes; acted on by `step` after the borrow is released.
    want_reload: bool,
    want_exit: bool,
    want_swap: bool,
    main_loop_stopped: bool,
    _watcher: Option<watch::ConfigWatcher>,
}

fn main() {
    init_logging();
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        // The second line is what a hot swap checks before exec'ing us.
        println!("tatamid {VERSION}\nhandoff {}", handoff::HANDOFF_FORMAT);
        return;
    }
    // `--adopt <manifest>`: we were exec'd by a previous image (hot swap);
    // the listener is our standard input and the layout is in the manifest.
    let adopt = args
        .windows(2)
        .find(|w| w[0] == "--adopt")
        .map(|w| PathBuf::from(&w[1]));
    let manifest = adopt
        .as_deref()
        .and_then(|p| match handoff::read_and_remove(p) {
            Ok(m) => Some(m),
            Err(e) => {
                tracing::error!("hot swap: {e}; starting from state.json instead");
                None
            }
        });

    let mtm = objc2::MainThreadMarker::new().expect("tatamid must start on the main thread");
    let main_loop = MainLoop::new(mtm);
    let wake = main_loop.waker();
    let (tx, rx) = channel::<MainMsg>();

    let sock = socket_path();
    let ipc = listen(sock.clone(), tx.clone(), wake.clone(), adopt.is_some());
    if let Err(e) = signals::start(tx.clone(), wake.clone()) {
        tracing::warn!("signal handler not installed: {e}");
    }

    let config_path = tatami_config::config_path();
    let config = load_config(&config_path);
    let watcher = watch::start(&config_path, tx.clone(), wake.clone());

    let state_path = tatami_state::state_path();
    let mut enabled = true;
    let saved = match manifest {
        Some(m) => {
            tracing::info!(
                "hot swap: tatamid {} → {VERSION}, same pid, {} windows in the manifest",
                m.version,
                m.state.window_count()
            );
            enabled = m.enabled;
            Some(m.state)
        }
        None => match tatami_state::load(&state_path) {
            Ok(Some(s)) if !config.restore.enabled => {
                tracing::info!("restore disabled; ignoring {}", state_path.display());
                drop(s);
                None
            }
            Ok(s) => {
                if let Some(s) = &s {
                    tracing::info!(
                        "restoring {} windows from {}",
                        s.window_count(),
                        state_path.display()
                    );
                }
                s
            }
            Err(e) => {
                tracing::warn!("state not loaded: {e}");
                None
            }
        },
    };

    let mut platform = MacPlatform::new();
    let pending: Rc<RefCell<VecDeque<PlatformEvent>>> = Rc::default();
    let daemon_slot: Rc<RefCell<Option<Rc<RefCell<Daemon>>>>> = Rc::default();

    // Platform events: queue, then drain through the daemon if it is free.
    // A callback that fires while the daemon is already borrowed (it never
    // should, but AppKit has surprised people) just leaves its event queued
    // for the current step to pick up.
    {
        let pending = pending.clone();
        let slot = daemon_slot.clone();
        platform.set_event_sink(Box::new(move |ev| {
            pending.borrow_mut().push_back(ev);
            if let Some(d) = slot.borrow().as_ref() {
                drain_platform_events(d);
            }
        }));
    }
    platform.start_observing();

    // Without Accessibility nothing works; ask once, loudly, and let the
    // platform's trust poll tell the reactor when the user grants it.
    let trusted = tatami_platform::ax::is_trusted();
    if !trusted {
        tracing::warn!(
            "Accessibility not granted: System Settings → Privacy & Security → Accessibility → enable tatamid (or the terminal it runs from)"
        );
        tatami_platform::ax::prompt_trust();
    }
    let now = Instant::now();
    let mut reactor = Reactor::new(platform, config, saved, now);
    if !trusted {
        let _ = reactor.on_platform_event(PlatformEvent::TrustChanged(false), now);
    }
    if !enabled {
        reactor.set_enabled(false);
    }
    reactor.set_start_at_login(agent_installed());
    let daemon = Rc::new(RefCell::new(Daemon {
        reactor,
        main_loop: main_loop.clone(),
        ipc,
        rx,
        pending: pending.clone(),
        subscribers: Vec::new(),
        config_path,
        state_path,
        dirty_since: Some(now),
        last_periodic: now,
        last_saved: Vec::new(),
        tap_installed: false,
        want_reload: false,
        want_exit: false,
        want_swap: false,
        main_loop_stopped: false,
        _watcher: watcher,
    }));
    *daemon_slot.borrow_mut() = Some(daemon.clone());

    // Channel drain on wake; timer; key tap.
    {
        let d = Rc::downgrade(&daemon);
        main_loop.on_wake(Box::new(move || {
            if let Some(d) = d.upgrade() {
                drain_channel(&d);
            }
        }));
    }
    {
        let d = Rc::downgrade(&daemon);
        main_loop.on_timer(Box::new(move || {
            if let Some(d) = d.upgrade() {
                step(&d, |r, now| r.on_timer(now));
            }
        }));
    }
    install_tap(&daemon);
    // Anything the reactor queued during construction, and the first save.
    step(&daemon, |r, now| r.drain(now));
    drain_platform_events(&daemon);

    tracing::info!("tatamid {VERSION} ready on {}", sock.display());
    if adopt.is_some() {
        handoff::write_status(&format!("ok {VERSION}"));
    }
    main_loop.run();
}

/// The socket: bound fresh, or adopted from the previous image after a hot
/// swap. If the adoption fails (it should not: the old image checked this
/// build first) the stale socket file is replaced by a fresh one, so
/// clients keep working either way.
fn listen(sock: PathBuf, tx: Sender<MainMsg>, wake: ipc::Waker, adopting: bool) -> ipc::IpcServer {
    if adopting {
        match ipc::IpcServer::adopt(sock.clone(), tx.clone(), wake.clone()) {
            Ok(s) => return s,
            Err(e) => {
                tracing::error!("hot swap: cannot adopt the listener: {e}; binding anew");
                let _ = std::fs::remove_file(&sock);
            }
        }
    }
    match ipc::IpcServer::start(sock.clone(), tx, wake) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // A daemon is already serving this user. Leave quietly with a
            // clean exit: launchd restarts a job that fails, and a second
            // tatamid respawning every ten seconds helps nobody.
            tracing::error!("{e}; this one exits");
            std::process::exit(0);
        }
        Err(e) => {
            tracing::error!("cannot listen on {}: {e}", sock.display());
            std::process::exit(2);
        }
    }
}

fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}

/// The launchd agent `tatami service install|enable` writes; its presence
/// is what "start at login" means.
fn agent_installed() -> bool {
    std::env::var_os("HOME").is_some_and(|h| {
        PathBuf::from(h)
            .join("Library/LaunchAgents/com.vgupta.tatami.plist")
            .exists()
    })
}

/// The `tatami` CLI installed next to this binary.
fn sibling_cli() -> Option<PathBuf> {
    let cli = std::env::current_exe().ok()?.with_file_name("tatami");
    cli.exists().then_some(cli)
}

fn load_config(path: &std::path::Path) -> tatami_config::Config {
    match tatami_config::load(path) {
        Ok(c) => {
            for w in &c.warnings {
                tracing::warn!("config: {w}");
            }
            c
        }
        Err(e) => {
            tracing::error!("config: {e}; using defaults");
            tatami_config::Config::default()
        }
    }
}

/// Run one reactor operation under the single borrow, then handle what
/// came out of it. Every path into the reactor goes through here.
fn step<F>(daemon: &Rc<RefCell<Daemon>>, f: F)
where
    F: FnOnce(&mut Reactor<MacPlatform>, Instant) -> Vec<Outcome>,
{
    let Ok(mut d) = daemon.try_borrow_mut() else {
        tracing::warn!("reactor re-entered; step skipped");
        return;
    };
    let now = Instant::now();
    let outcomes = f(&mut d.reactor, now);
    d.handle_outcomes(outcomes, now);
    // Events that arrived while we held the borrow.
    loop {
        let ev = d.pending.borrow_mut().pop_front();
        let Some(ev) = ev else { break };
        let out = d.reactor.on_platform_event(ev, now);
        d.handle_outcomes(out, now);
    }
    d.after_step(now);
    let want_reload = std::mem::take(&mut d.want_reload);
    let want_swap = std::mem::take(&mut d.want_swap);
    let want_exit = d.want_exit;
    let want_tap = !d.tap_installed && d.reactor.trusted();
    let main_loop = d.main_loop.clone();
    drop(d);
    if want_tap {
        install_tap(daemon);
    }
    if want_reload {
        reload(daemon);
    }
    if want_exit {
        shutdown(daemon);
    } else if want_swap {
        // On the next run-loop pass, so the reply to `tatami hot-swap` is
        // on its way before this process is replaced.
        let d = daemon.clone();
        main_loop.perform(Box::new(move || hot_swap(&d)));
    }
}

fn drain_platform_events(daemon: &Rc<RefCell<Daemon>>) {
    if daemon.try_borrow().is_err() {
        return; // the running step drains the queue before it returns
    }
    step(daemon, |_, _| Vec::new());
}

fn drain_channel(daemon: &Rc<RefCell<Daemon>>) {
    loop {
        let msg = match daemon.try_borrow() {
            Ok(d) => d.rx.try_recv().ok(),
            Err(_) => return,
        };
        let Some(msg) = msg else { return };
        match msg {
            MainMsg::Ipc(IpcMsg::Request { req, reply }) => {
                let mut resp = None;
                step(daemon, |r, now| {
                    let (response, outcomes) = handle_request(r, &req.args, now);
                    resp = Some(response);
                    outcomes
                });
                let _ = reply.send(resp.unwrap_or_else(|| Response::err(0, "daemon busy")));
            }
            MainMsg::Ipc(IpcMsg::Subscribe { topics, ack, sink }) => {
                let bad: Vec<&String> = topics
                    .iter()
                    .filter(|t| !IpcEvent::TOPICS.contains(&t.as_str()))
                    .collect();
                if topics.is_empty() || !bad.is_empty() {
                    let _ = ack.send(Response::err(
                        0,
                        format!(
                            "subscribe: unknown topic(s) {bad:?}; known: {:?}",
                            IpcEvent::TOPICS
                        ),
                    ));
                } else {
                    let _ = ack.send(Response::ok(0, ""));
                    if let Ok(mut d) = daemon.try_borrow_mut() {
                        d.subscribers.push((topics, sink));
                    }
                }
            }
            MainMsg::ReloadConfig => reload(daemon),
            MainMsg::HotSwap(why) => {
                tracing::info!("hot swap requested: {why}");
                hot_swap(daemon);
            }
            MainMsg::Shutdown(why) => {
                tracing::info!("shutting down: {why}");
                shutdown(daemon);
                return;
            }
        }
    }
}

/// Requests the daemon answers itself, before the reactor sees them.
fn handle_request(
    r: &mut Reactor<MacPlatform>,
    args: &[String],
    now: Instant,
) -> (Response, Vec<Outcome>) {
    if args.first().map(String::as_str) == Some("checkpoint") {
        return match Action::parse(args) {
            Ok(Action::Checkpoint(c)) => checkpoint(r, c, now),
            Ok(_) => (Response::err(0, "checkpoint: unexpected parse"), Vec::new()),
            Err(e) => (Response::err(0, e.to_string()), Vec::new()),
        };
    }
    r.handle_request(args, now)
}

fn checkpoint(
    r: &mut Reactor<MacPlatform>,
    cmd: CheckpointCmd,
    now: Instant,
) -> (Response, Vec<Outcome>) {
    let dir = tatami_state::checkpoints_dir();
    match cmd {
        CheckpointCmd::Save(name) => {
            let name = name.unwrap_or_else(timestamp_name);
            let Some(path) = tatami_state::checkpoint_path(&name) else {
                return (
                    Response::err(0, format!("bad checkpoint name `{name}`")),
                    Vec::new(),
                );
            };
            let mut s = r.snapshot();
            s.saved_at = rfc3339_now();
            match tatami_state::save(&path, &s) {
                Ok(()) => (
                    Response::ok(
                        0,
                        format!("saved {} ({} windows)", path.display(), s.window_count()),
                    ),
                    Vec::new(),
                ),
                Err(e) => (Response::err(0, e.to_string()), Vec::new()),
            }
        }
        CheckpointCmd::Restore(name) => {
            let Some(path) = tatami_state::checkpoint_path(&name) else {
                return (
                    Response::err(0, format!("bad checkpoint name `{name}`")),
                    Vec::new(),
                );
            };
            match tatami_state::load(&path) {
                Ok(Some(s)) => {
                    let n = s.window_count();
                    let out = r.restore(s, now);
                    (
                        Response::ok(0, format!("restoring {n} windows from {name}")),
                        out,
                    )
                }
                Ok(None) => (
                    Response::err(0, format!("no checkpoint `{name}`")),
                    Vec::new(),
                ),
                Err(e) => (Response::err(0, e.to_string()), Vec::new()),
            }
        }
        CheckpointCmd::List => {
            let list = tatami_state::list_checkpoints(&dir);
            let text: Vec<String> = list.iter().map(|(n, _)| n.clone()).collect();
            let data = serde_json::json!(text);
            (Response::with_data(0, text.join("\n"), data), Vec::new())
        }
        CheckpointCmd::Delete(name) => {
            let Some(path) = tatami_state::checkpoint_path(&name) else {
                return (
                    Response::err(0, format!("bad checkpoint name `{name}`")),
                    Vec::new(),
                );
            };
            match std::fs::remove_file(&path) {
                Ok(()) => (Response::ok(0, format!("deleted {name}")), Vec::new()),
                Err(e) => (
                    Response::err(0, format!("{}: {e}", path.display())),
                    Vec::new(),
                ),
            }
        }
    }
}

fn reload(daemon: &Rc<RefCell<Daemon>>) {
    let path = match daemon.try_borrow() {
        Ok(d) => d.config_path.clone(),
        Err(_) => return,
    };
    let (config, msg, ok) = match tatami_config::load(&path) {
        Ok(c) => {
            for w in &c.warnings {
                tracing::warn!("config: {w}");
            }
            let msg = if c.warnings.is_empty() {
                "config reloaded".to_string()
            } else {
                format!("config reloaded with {} warning(s)", c.warnings.len())
            };
            (Some(c), msg, true)
        }
        Err(e) => (None, e.to_string(), false),
    };
    tracing::info!("{msg}");
    step(daemon, |r, now| {
        let mut out = match config {
            Some(c) => r.reload_config(c, now),
            None => Vec::new(),
        };
        out.push(Outcome::Broadcast(IpcEvent::ConfigReloaded {
            ok,
            message: msg,
        }));
        out
    });
}

fn shutdown(daemon: &Rc<RefCell<Daemon>>) {
    let Ok(mut d) = daemon.try_borrow_mut() else {
        return;
    };
    if d.want_exit && d.main_loop_stopped {
        return;
    }
    tracing::info!("shutting down: quit requested");
    d.want_exit = true;
    let out = d.reactor.shutdown();
    let now = Instant::now();
    d.handle_outcomes(out, now);
    d.save_now(true);
    d.main_loop_stopped = true;
    d.main_loop.stop();
}

/// Replace this process with the `tatamid` on disk, same pid: the layout
/// crosses in a manifest and the listening socket as standard input;
/// every window stays exactly where it is, parked ones included, and the
/// new image finds them again by pid and window id. Only ever execs our
/// own path. If anything fails before the exec, the old image keeps
/// serving and `swap-status` says why.
fn hot_swap(daemon: &Rc<RefCell<Daemon>>) {
    let Ok(mut d) = daemon.try_borrow_mut() else {
        tracing::warn!("hot swap: daemon busy; not swapped");
        return;
    };
    if d.want_exit {
        return;
    }
    let e = d.try_hot_swap();
    tracing::warn!("hot swap failed: {e}; still running tatamid {VERSION}");
    handoff::write_status(&format!("failed {e}"));
}

fn install_tap(daemon: &Rc<RefCell<Daemon>>) {
    let weak: Weak<RefCell<Daemon>> = Rc::downgrade(daemon);
    let Ok(mut d) = daemon.try_borrow_mut() else {
        return;
    };
    if d.tap_installed {
        return;
    }
    let main_loop = d.main_loop.clone();
    let sink = Box::new(move |raw| -> KeyDisposition {
        let Some(d) = weak.upgrade() else {
            return KeyDisposition::Pass;
        };
        // The tap callback runs on the main thread between run-loop
        // callouts, so the daemon is free; if it somehow is not, the key
        // must reach the app rather than vanish.
        let Ok(mut dm) = d.try_borrow_mut() else {
            return KeyDisposition::Pass;
        };
        let disp = dm.reactor.on_key(raw, Instant::now());
        drop(dm);
        // Run the actions (and overlay changes, which an Alt press alone
        // can cause) after returning from the tap, never inside it.
        let d2 = d.clone();
        main_loop.perform(Box::new(move || step(&d2, |r, now| r.drain(now))));
        disp
    });
    match d.reactor.platform().install_key_tap(sink) {
        Ok(()) => {
            d.tap_installed = true;
            tracing::info!("key tap installed");
        }
        Err(e) => {
            tracing::warn!("key tap not installed: {e} (will retry when Accessibility is granted)")
        }
    }
}

impl Daemon {
    fn handle_outcomes(&mut self, outcomes: Vec<Outcome>, now: Instant) {
        for o in outcomes {
            match o {
                // Trailing: the clock restarts on every change, and the 5 s
                // periodic save bounds the wait under continuous change.
                Outcome::Dirty => self.dirty_since = Some(now),
                Outcome::Broadcast(ev) => self.broadcast(ev),
                Outcome::Checkpoint(cmd) => {
                    let (resp, more) = checkpoint(&mut self.reactor, cmd, now);
                    if resp.ok {
                        tracing::info!("{}", resp.stdout);
                    } else {
                        tracing::warn!("checkpoint: {}", resp.error);
                    }
                    self.handle_outcomes(more, now);
                }
                Outcome::Exec(cmd) => {
                    if let Err(e) = std::process::Command::new("/bin/sh")
                        .arg("-c")
                        .arg(&cmd)
                        .spawn()
                    {
                        tracing::warn!("exec `{cmd}`: {e}");
                    }
                }
                // These need the Rc, so `step` acts on them after the borrow ends.
                Outcome::ReloadConfig => self.want_reload = true,
                Outcome::Exit => self.want_exit = true,
                Outcome::HotSwap => self.want_swap = true,
                Outcome::EditConfig(editor) => self.edit_config(&editor),
                Outcome::StartAtLogin(on) => {
                    // The CLI next to us owns the launchd agent; run it and
                    // believe the file system, not the exit code.
                    match sibling_cli() {
                        Some(cli) => {
                            let out = Command::new(cli)
                                .args(["service", if on { "enable" } else { "disable" }])
                                .output();
                            match out {
                                Ok(o) if o.status.success() => {
                                    tracing::info!("{}", String::from_utf8_lossy(&o.stdout).trim())
                                }
                                Ok(o) => tracing::warn!(
                                    "start-at-login: {}",
                                    String::from_utf8_lossy(&o.stderr).trim()
                                ),
                                Err(e) => tracing::warn!("start-at-login: {e}"),
                            }
                        }
                        None => tracing::warn!("start-at-login: no `tatami` next to tatamid"),
                    }
                    self.reactor.set_start_at_login(agent_installed());
                }
            }
        }
    }

    /// `edit-config`: make sure there is a file to edit (the commented
    /// template, every option at its default), then open it. `editor` is
    /// `general.editor`; empty means TextEdit, which every Mac has.
    fn edit_config(&self, editor: &str) {
        let path = &self.config_path;
        if !path.exists() {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            match std::fs::write(path, tatami_config::CONFIG_TEMPLATE) {
                Ok(()) => tracing::info!("wrote the config template to {}", path.display()),
                Err(e) => {
                    tracing::warn!("edit-config: cannot write {}: {e}", path.display());
                    return;
                }
            }
        }
        let cmd = if editor.trim().is_empty() {
            format!("open -t '{}'", path.display())
        } else {
            format!("{editor} '{}'", path.display())
        };
        if let Err(e) = Command::new("/bin/sh").arg("-c").arg(&cmd).spawn() {
            tracing::warn!("edit-config: `{cmd}`: {e}");
        }
    }

    fn broadcast(&mut self, ev: IpcEvent) {
        let topic = ev.topic();
        self.subscribers.retain(|(topics, sink)| {
            if !topics.iter().any(|t| t == topic) {
                return true;
            }
            sink.send(ev.clone()).is_ok()
        });
    }

    /// Timer re-arm and autosave, after every step.
    fn after_step(&mut self, now: Instant) {
        if self.want_exit {
            return;
        }
        let mut due = self.reactor.next_due();
        if let Some(since) = self.dirty_since {
            let t = since + AUTOSAVE_DEBOUNCE;
            if t <= now {
                self.save_now(false);
            } else {
                due = Some(due.map_or(t, |d| d.min(t)));
            }
        }
        let periodic = self.last_periodic + AUTOSAVE_PERIOD;
        if periodic <= now {
            self.last_periodic = now;
            self.save_now(false);
        } else {
            due = Some(due.map_or(periodic, |d| d.min(periodic)));
        }
        self.main_loop.set_timer(due);
    }

    /// Returns only on failure: a successful exec never comes back.
    fn try_hot_swap(&mut self) -> String {
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => return format!("current_exe: {e}"),
        };
        let next = match handoff::preflight(&exe) {
            Ok(v) => v,
            Err(e) => return e,
        };
        // The layout as of this instant, to state.json too in case the
        // new image never comes up.
        self.save_now(true);
        let manifest = handoff::Manifest {
            handoff: handoff::HANDOFF_FORMAT,
            version: VERSION.to_string(),
            enabled: self.reactor.enabled(),
            state: self.reactor.snapshot(),
        };
        let path = handoff::path();
        if let Err(e) = handoff::write(&path, &manifest) {
            return e;
        }
        let listener = match self.ipc.listener().try_clone() {
            Ok(l) => l,
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                return format!("dup listener: {e}");
            }
        };
        let _ = std::fs::remove_file(handoff::status_path());
        tracing::info!(
            "hot swap: exec {} (tatamid {VERSION} → {next}), {} windows",
            exe.display(),
            manifest.state.window_count()
        );
        // `exec` replaces this process. The listener lands on fd 0 through
        // `Stdio` (a dup, so it loses its close-on-exec), every other
        // descriptor closes, the environment carries over.
        let err = Command::new(&exe)
            .arg("--adopt")
            .arg(&path)
            .stdin(Stdio::from(OwnedFd::from(listener)))
            .exec();
        let _ = std::fs::remove_file(&path);
        format!("exec {}: {err}", exe.display())
    }

    /// Serialize and write if the bytes changed (or `force`).
    fn save_now(&mut self, force: bool) {
        self.dirty_since = None;
        let mut s: SavedState = self.reactor.snapshot();
        let bytes = serde_json::to_vec(&s).unwrap_or_default();
        if !force && bytes == self.last_saved {
            return;
        }
        s.saved_at = rfc3339_now();
        match tatami_state::save(&self.state_path, &s) {
            Ok(()) => {
                self.last_saved = bytes;
                tracing::debug!("state saved ({} windows)", s.window_count());
            }
            Err(e) => tracing::warn!("state not saved: {e}"),
        }
    }
}

fn timestamp_name() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("auto-{secs}")
}

/// RFC 3339 in UTC without pulling in a time crate.
fn rfc3339_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}
