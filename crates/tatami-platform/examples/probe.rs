//! Manual probe of the macOS platform layer.
//!
//!   cargo run -p tatami-platform --example probe -- trust [--prompt]
//!   cargo run -p tatami-platform --example probe -- monitors
//!   cargo run -p tatami-platform --example probe -- windows [pid]
//!   cargo run -p tatami-platform --example probe -- move <handle> <x> <y> <w> <h> [psp]
//!   cargo run -p tatami-platform --example probe -- focus <handle>
//!   cargo run -p tatami-platform --example probe -- loop
//!   cargo run -p tatami-platform --example probe -- events [secs]
//!   cargo run -p tatami-platform --example probe -- keys [secs]
//!   cargo run -p tatami-platform --example probe -- overlay [secs]
//!   cargo run -p tatami-platform --example probe -- status <text> [secs]
//!   cargo run -p tatami-platform --example probe -- menu [secs]

use std::process::ExitCode;
use std::time::{Duration, Instant};

use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;
use objc2_core_graphics::{CGDisplayBounds, CGMainDisplayID};
use tatami_core::{
    KeyDisposition, MenuItem, OverlayContent, OverlayRow, Platform, PlatformEvent, RawKeyEvent,
    SetOrder,
};
use tatami_platform::runloop::MainLoop;
use tatami_platform::{MacPlatform, ax};
use tatami_protocol::{Rect, WindowHandle};

const USAGE: &str = "usage: probe trust [--prompt] | monitors | windows [pid] | move <handle> <x> <y> <w> <h> [psp] | focus <handle>\n       | loop | events [secs] | keys [secs] | overlay [secs] | status <text> [secs] | menu [secs]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let res = match a.as_slice() {
        ["trust"] => trust(false),
        ["trust", "--prompt"] => trust(true),
        ["monitors"] => monitors(),
        ["windows"] => trusted().and_then(|p| windows(p, None)),
        ["windows", pid] => trusted().and_then(|p| windows(p, Some(num(pid)?))),
        ["move", h, x, y, w, hh, rest @ ..] => {
            let order = match rest {
                [] => SetOrder::SizePos,
                ["psp"] => SetOrder::PosSizePos,
                _ => return fail(USAGE),
            };
            let r = (|| Ok(Rect::new(num(x)?, num(y)?, num(w)?, num(hh)?)))();
            r.and_then(|r| trusted().and_then(|p| move_window(p, WindowHandle(num(h)?), r, order)))
        }
        ["focus", h] => trusted().and_then(|p| focus(p, WindowHandle(num(h)?))),
        ["loop"] => main_loop_check(),
        ["events", rest @ ..] => secs(rest).and_then(events),
        ["keys", rest @ ..] => secs(rest).and_then(keys),
        ["overlay", rest @ ..] => secs(rest).and_then(overlay),
        ["status", text, rest @ ..] => secs(rest).and_then(|d| status(text, d)),
        ["menu", rest @ ..] => secs(rest).and_then(menu),
        ["press", code, rest @ ..] => num(code).and_then(|c| press(c, rest)),
        ["front"] => {
            println!(
                "ax focused pid: {:?}",
                tatami_platform::apps::ax_focused_pid()
            );
            println!("front window: {:?}", tatami_platform::apps::front_window());
            let p = MacPlatform::new();
            println!("ax frontmost scan: {:?}", p.ax_frontmost_pid());
            for (pid, name) in tatami_platform::apps::window_owners().iter().take(8) {
                println!("{pid:>6} {name}");
            }
            Ok(())
        }
        ["apps"] => {
            apps_all();
            Ok(())
        }
        _ => Err(USAGE.into()),
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e),
    }
}

/// Post one key press (down + up) through the session event tap, with
/// modifiers given as words: `press 4 alt` is alt+h. Used to test the
/// daemon's tap without a human at the keyboard.
fn press(code: u16, mods: &[&str]) -> Result<(), String> {
    use objc2_core_graphics::{
        CGEvent, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
    };
    let mut flags = CGEventFlags::empty();
    for m in mods {
        flags |= match *m {
            "alt" | "option" => CGEventFlags::MaskAlternate,
            "shift" => CGEventFlags::MaskShift,
            "ctrl" | "control" => CGEventFlags::MaskControl,
            "cmd" | "command" => CGEventFlags::MaskCommand,
            _ => return Err(format!("unknown modifier {m}")),
        };
    }
    let src = CGEventSource::new(CGEventSourceStateID::HIDSystemState);
    for down in [true, false] {
        let ev = CGEvent::new_keyboard_event(src.as_deref(), code, down)
            .ok_or("CGEventCreateKeyboardEvent failed")?;
        CGEvent::set_flags(Some(&ev), flags);
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

/// Every running application as NSWorkspace reports it, with its
/// activation policy, to see which apps the Regular filter drops.
fn apps_all() {
    use objc2_app_kit::NSWorkspace;
    for a in NSWorkspace::sharedWorkspace().runningApplications().iter() {
        let name = a.localizedName().map(|n| n.to_string()).unwrap_or_default();
        let bid = a
            .bundleIdentifier()
            .map(|n| n.to_string())
            .unwrap_or_default();
        println!(
            "{:>6} {:?} {name} {bid}",
            a.processIdentifier(),
            a.activationPolicy()
        );
    }
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("{msg}");
    ExitCode::FAILURE
}

fn num<T: std::str::FromStr>(s: &str) -> Result<T, String> {
    s.parse().map_err(|_| format!("not a number: {s}"))
}

fn grant_hint() -> String {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    format!(
        "Grant Accessibility: System Settings → Privacy & Security → Accessibility → add the binary\n  {exe}"
    )
}

fn trusted() -> Result<MacPlatform, String> {
    if ax::is_trusted() {
        Ok(MacPlatform::new())
    } else {
        Err(format!("not trusted.\n{}", grant_hint()))
    }
}

fn trust(prompt: bool) -> Result<(), String> {
    let ok = if prompt {
        ax::prompt_trust()
    } else {
        ax::is_trusted()
    };
    println!("trusted: {ok}");
    if !ok {
        println!("{}", grant_hint());
    }
    Ok(())
}

fn fmt_rect(r: &Rect) -> String {
    format!("{:.0},{:.0} {:.0}x{:.0}", r.x, r.y, r.w, r.h)
}

fn monitors() -> Result<(), String> {
    let p = MacPlatform::new();
    let t = Instant::now();
    let ms = p.screens();
    let dt = t.elapsed();
    for m in &ms {
        println!(
            "{} {:<24} frame={:<22} visible={:<22} main={}",
            m.uuid,
            m.name,
            fmt_rect(&m.frame),
            fmt_rect(&m.visible),
            m.is_main
        );
    }
    println!("{} monitors in {dt:?}", ms.len());
    Ok(())
}

fn windows(mut p: MacPlatform, only: Option<i32>) -> Result<(), String> {
    let t = Instant::now();
    let apps = p.running_apps();
    let mut n = 0;
    for app in apps.iter().filter(|a| only.is_none_or(|pid| a.pid == pid)) {
        let ta = Instant::now();
        let ws = p.app_windows(app.pid);
        println!(
            "# {} (pid {}, {:?}): {} windows in {:?}",
            app.name,
            app.pid,
            app.bundle_id.as_deref().unwrap_or("-"),
            ws.len(),
            ta.elapsed()
        );
        for w in ws {
            n += 1;
            println!(
                "{:>7} {:>6} {:<18.18} {:<20} {:<22} min={} fs={} rz={} {:?}",
                w.handle.0,
                w.pid,
                app.name,
                w.subrole,
                fmt_rect(&w.frame),
                w.minimized as u8,
                w.fullscreen as u8,
                w.resizable as u8,
                w.title
            );
        }
    }
    println!(
        "{n} windows across {} apps in {:?}",
        apps.len(),
        t.elapsed()
    );
    let t = Instant::now();
    println!(
        "focused: {:?} ({:?})",
        p.focused_window().map(|w| w.0),
        t.elapsed()
    );
    Ok(())
}

/// Populate the handle table by enumerating apps until `h` shows up.
fn find(p: &mut MacPlatform, h: WindowHandle) -> Result<(), String> {
    for app in p.running_apps() {
        if p.app_windows(app.pid).iter().any(|w| w.handle == h) {
            println!("window {} belongs to {} (pid {})", h.0, app.name, app.pid);
            return Ok(());
        }
    }
    Err(format!(
        "no window with handle {} on the current Space",
        h.0
    ))
}

fn move_window(
    mut p: MacPlatform,
    h: WindowHandle,
    r: Rect,
    order: SetOrder,
) -> Result<(), String> {
    find(&mut p, h)?;
    let before = p.window_frame(h).ok_or("cannot read frame")?;
    let t = Instant::now();
    p.set_window_frame(h, r, order).map_err(|e| e.to_string())?;
    let dt = t.elapsed();
    let after = p.window_frame(h).ok_or("cannot read frame back")?;
    println!("before:    {}", fmt_rect(&before));
    println!("requested: {} ({order:?}, {dt:?})", fmt_rect(&r));
    println!("after:     {}", fmt_rect(&after));
    println!("honoured:  {}", after.approx_eq(&r, 1.0));
    Ok(())
}

fn focus(mut p: MacPlatform, h: WindowHandle) -> Result<(), String> {
    find(&mut p, h)?;
    p.raise(h).map_err(|e| e.to_string())?;
    println!(
        "raised {}; focused now: {:?}",
        h.0,
        p.focused_window().map(|w| w.0)
    );
    Ok(())
}

fn secs(rest: &[&str]) -> Result<Duration, String> {
    match rest {
        [] => Ok(Duration::from_secs(5)),
        [s] => Ok(Duration::from_secs_f64(num(s)?)),
        _ => Err(USAGE.into()),
    }
}

/// Run the main loop for `d`, then stop it.
fn run_for(ml: MainLoop, d: Duration) {
    let stopper = ml.clone();
    ml.on_timer(Box::new(move || stopper.stop()));
    ml.set_timer(Some(Instant::now() + d));
    ml.run();
}

fn main_loop() -> MainLoop {
    MainLoop::new(MainThreadMarker::new().expect("main thread"))
}

/// Wake from another thread, a queued one-shot, and a re-armed timer.
fn main_loop_check() -> Result<(), String> {
    let ml = main_loop();
    let t0 = Instant::now();
    let at = move || t0.elapsed().as_secs_f64() * 1000.0;
    let woken = std::rc::Rc::new(std::cell::Cell::new(0));
    let w = woken.clone();
    ml.on_wake(Box::new(move || {
        w.set(w.get() + 1);
        println!("{:>7.2}ms on_wake #{}", at(), w.get());
    }));
    let waker = ml.waker();
    std::thread::spawn(move || {
        for _ in 0..3 {
            std::thread::sleep(Duration::from_millis(100));
            let t = Instant::now();
            waker();
            println!("          waker() from thread took {:?}", t.elapsed());
        }
    });
    let ml2 = ml.clone();
    ml.perform(Box::new(move || println!("{:>7.2}ms perform ran", at())));
    // First fire at 50 ms re-arms for 500 ms; the second stops the loop.
    let fired = std::cell::Cell::new(0);
    ml.on_timer(Box::new(move || {
        fired.set(fired.get() + 1);
        println!("{:>7.2}ms timer #{}", at(), fired.get());
        if fired.get() == 1 {
            ml2.set_timer(Some(Instant::now() + Duration::from_millis(500)));
        } else {
            ml2.stop();
        }
    }));
    ml.set_timer(Some(Instant::now() + Duration::from_millis(50)));
    ml.run();
    println!(
        "{:>7.2}ms run returned; on_wake ran {} times",
        at(),
        woken.get()
    );
    Ok(())
}

fn events(d: Duration) -> Result<(), String> {
    let ml = main_loop();
    let mut p = trusted()?;
    let t0 = Instant::now();
    p.set_event_sink(Box::new(move |ev| {
        println!("{:>7.3}s {ev:?}", t0.elapsed().as_secs_f64());
    }));
    p.start_observing();
    let t = Instant::now();
    let apps = p.running_apps();
    let mut ok = 0;
    for a in &apps {
        match p.observe_app(a.pid) {
            Ok(()) => ok += 1,
            Err(e) => println!("observe {} (pid {}): {e}", a.name, a.pid),
        }
        // Populate the table so destroyed windows can be matched.
        p.app_windows(a.pid);
    }
    println!(
        "observing {ok}/{} apps ({:?}); listening {d:?}",
        apps.len(),
        t.elapsed()
    );
    run_for(ml, d);
    println!("done");
    Ok(())
}

fn keys(d: Duration) -> Result<(), String> {
    let ml = main_loop();
    let mut p = MacPlatform::new();
    p.install_key_tap(Box::new(|ev| {
        let q = match ev {
            RawKeyEvent::Down { keycode, mods, .. } | RawKeyEvent::Up { keycode, mods } => {
                keycode == 12 && mods.alt && mods.shift
            }
            RawKeyEvent::Flags { .. } => false,
        };
        println!("{ev:?}");
        if q {
            println!("swallowed");
            KeyDisposition::Swallow
        } else {
            KeyDisposition::Pass
        }
    }))
    .map_err(|e| e.to_string())?;
    println!("key tap installed; listening {d:?}");
    run_for(ml, d);
    println!("done");
    Ok(())
}

fn sample_sheet() -> OverlayContent {
    let rows = [
        ("h", "focus left", false),
        ("j", "focus down", false),
        ("k", "focus up", false),
        ("l", "focus right", false),
        ("H", "move left", false),
        ("J", "move down", false),
        ("K", "move up", false),
        ("L", "move right", false),
        ("f", "toggle fullscreen", false),
        ("1-9", "workspace 1-9", false),
        ("tab", "workspace back and forth across monitors", false),
        ("r", "+resize", true),
        ("s", "+service", true),
        ("w", "+workspace", true),
    ];
    OverlayContent {
        title: "⌥  tatami".into(),
        exit_hint: "Esc  close".into(),
        rows: rows
            .iter()
            .map(|&(k, d, p)| OverlayRow {
                key: k.into(),
                desc: d.into(),
                prefix: p,
                // The first two rows are clickable, like the overview's window rows.
                command: match k {
                    "h" => vec!["focus".into(), "left".into()],
                    "j" => vec!["focus".into(), "down".into()],
                    _ => Vec::new(),
                },
            })
            .collect(),
        columns: None,
        max_desc: None,
        list: false,
    }
}

/// Shows the sample sheet; prints its frame (AX coordinates) and the
/// `Command` of any clicked row, and checks that clicking left focus alone.
fn overlay(d: Duration) -> Result<(), String> {
    let ml = main_loop();
    let mut p = MacPlatform::new();
    p.set_event_sink(Box::new(|ev| {
        if let PlatformEvent::Command(c) = ev {
            println!("command {c:?}");
        }
    }));
    let front = tatami_platform::apps::front_window();
    let main = p
        .screens()
        .into_iter()
        .find(|m| m.is_main)
        .ok_or("no main monitor")?;
    let sheet = sample_sheet();
    let t = Instant::now();
    p.show_overlay(Some(&sheet), &main);
    let first = t.elapsed();
    let t = Instant::now();
    p.show_overlay(Some(&sheet), &main);
    let same = t.elapsed();
    let mut fewer = sheet.clone();
    fewer.rows.pop();
    let t = Instant::now();
    p.show_overlay(Some(&fewer), &main);
    let changed = t.elapsed();
    p.show_overlay(Some(&sheet), &main);
    println!(
        "overlay on {} ({}): first show {first:?}, identical re-show {same:?}, changed content {changed:?}",
        main.name,
        fmt_rect(&main.visible),
    );
    let mtm = MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    let main_h = CGDisplayBounds(CGMainDisplayID()).size.height;
    for w in app.windows().iter().filter(|w| w.isVisible()) {
        let f = w.frame();
        let ax = Rect::new(
            f.origin.x,
            main_h - f.origin.y - f.size.height,
            f.size.width,
            f.size.height,
        );
        println!(
            "panel {} canBecomeKey={} ignoresMouse={}; listening {d:?}",
            fmt_rect(&ax),
            w.canBecomeKeyWindow(),
            w.ignoresMouseEvents()
        );
    }
    run_for(ml, d);
    println!(
        "front window (pid, id) before {front:?}, after {:?}; we are active={} key window={}",
        tatami_platform::apps::front_window(),
        app.isActive(),
        app.keyWindow().is_some()
    );
    p.show_overlay(None, &main);
    println!("hidden");
    Ok(())
}

fn status(text: &str, d: Duration) -> Result<(), String> {
    let ml = main_loop();
    let mut p = MacPlatform::new();
    let t = Instant::now();
    p.set_status(text);
    println!("status {text:?} set in {:?}", t.elapsed());
    run_for(ml, d);
    p.set_status("");
    println!("cleared (icon only)");
    Ok(())
}

/// Status "[1]" plus a sample menu; prints the `Command` of any picked item.
fn menu(d: Duration) -> Result<(), String> {
    let ml = main_loop();
    let mut p = MacPlatform::new();
    p.set_event_sink(Box::new(|ev| {
        if let PlatformEvent::Command(c) = ev {
            println!("command {c:?}");
        }
    }));
    let ws = |title: &str, n: &str, checked: bool| MenuItem {
        checked,
        indent: 1,
        ..MenuItem::action(title, &["workspace", n])
    };
    let items = [
        MenuItem::header("Monitor: Built-in"),
        ws("1", "1", true),
        ws("2", "2", false),
        MenuItem::separator(),
        MenuItem::action("Reload config", &["reload-config"]),
        MenuItem::action("Quit tatami", &["quit"]),
    ];
    let t = Instant::now();
    p.set_status("[1]");
    let status = t.elapsed();
    let t = Instant::now();
    p.set_menu(&items);
    let first = t.elapsed();
    let t = Instant::now();
    p.set_menu(&items);
    let same = t.elapsed();
    println!(
        "status item created in {status:?}, menu built in {first:?}, identical menu re-set {same:?}; listening {d:?}"
    );
    run_for(ml, d);
    println!("done");
    Ok(())
}
