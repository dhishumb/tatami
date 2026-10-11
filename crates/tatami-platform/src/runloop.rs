//! The main loop: `NSApp.run()` plus one CF source for cross-thread wake-ups
//! and one reusable timer. All callbacks run on the main thread; the waker is
//! the only piece that crosses threads, and it only signals the CF source.

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::Arc;
use std::time::Instant;

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSEvent, NSEventModifierFlags, NSEventType,
};
use objc2_core_foundation::{
    CFAbsoluteTimeGetCurrent, CFRetained, CFRunLoop, CFRunLoopSource, CFRunLoopSourceContext,
    CFRunLoopTimer, CGPoint, kCFRunLoopCommonModes,
};

/// Fire date that never comes (~31k years past 2001).
const FAR_FUTURE: f64 = 1e12;

type Slot = Option<Box<dyn FnMut()>>;

#[derive(Default)]
struct State {
    on_wake: Slot,
    on_timer: Slot,
    queue: Vec<Box<dyn FnOnce()>>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::default();
}

/// Run the callback in `slot` without holding the borrow, so it may call back
/// into `MainLoop` (or replace itself).
fn call(slot: fn(&mut State) -> &mut Slot) {
    let Some(mut f) = STATE.with_borrow_mut(|s| slot(s).take()) else {
        return;
    };
    f();
    STATE.with_borrow_mut(|s| {
        slot(s).get_or_insert(f);
    });
}

unsafe extern "C-unwind" fn source_perform(_: *mut c_void) {
    let queued = STATE.with_borrow_mut(|s| std::mem::take(&mut s.queue));
    for f in queued {
        f();
    }
    call(|s| &mut s.on_wake);
}

unsafe extern "C-unwind" fn timer_fired(_: *mut CFRunLoopTimer, _: *mut c_void) {
    call(|s| &mut s.on_timer);
}

pub(crate) fn common_modes() -> Option<&'static objc2_core_foundation::CFRunLoopMode> {
    // SAFETY: reading an immutable framework constant.
    unsafe { kCFRunLoopCommonModes }
}

pub(crate) fn main_run_loop() -> CFRetained<CFRunLoop> {
    CFRunLoop::main().expect("main run loop")
}

/// The pieces the waker needs, shared with other threads.
struct Signal {
    source: CFRetained<CFRunLoopSource>,
    main: CFRetained<CFRunLoop>,
}

// SAFETY: CFRunLoopSourceSignal and CFRunLoopWakeUp are documented thread-safe,
// and the waker never touches anything else.
unsafe impl Send for Signal {}
// SAFETY: as above; both calls take the objects by shared reference.
unsafe impl Sync for Signal {}

impl Signal {
    fn fire(&self) {
        self.source.signal();
        self.main.wake_up();
    }
}

#[derive(Clone)]
pub struct MainLoop {
    app: Retained<NSApplication>,
    signal: Arc<Signal>,
    timer: CFRetained<CFRunLoopTimer>,
}

impl MainLoop {
    pub fn new(mtm: MainThreadMarker) -> Self {
        let app = NSApplication::sharedApplication(mtm);
        // No Dock icon, no menu bar; still allowed status items and panels.
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        let main = main_run_loop();

        let mut ctx = CFRunLoopSourceContext {
            version: 0,
            info: std::ptr::null_mut(),
            retain: None,
            release: None,
            copyDescription: None,
            equal: None,
            hash: None,
            schedule: None,
            cancel: None,
            perform: Some(source_perform),
        };
        // SAFETY: `ctx` is a valid version-0 context; CF copies it.
        let source = unsafe { CFRunLoopSource::new(None, 0, &mut ctx) }.expect("CFRunLoopSource");
        main.add_source(Some(&source), common_modes());

        // Repeating with a huge interval, so it survives firing and is only
        // ever moved with `set_next_fire_date`.
        // SAFETY: `timer_fired` matches the callback ABI; a null context is allowed.
        let timer = unsafe {
            CFRunLoopTimer::new(
                None,
                FAR_FUTURE,
                FAR_FUTURE,
                0,
                0,
                Some(timer_fired),
                std::ptr::null_mut(),
            )
        }
        .expect("CFRunLoopTimer");
        main.add_timer(Some(&timer), common_modes());

        Self {
            app,
            signal: Arc::new(Signal { source, main }),
            timer,
        }
    }

    /// Blocks until `stop`.
    pub fn run(self) {
        self.app.run();
    }

    /// Callable from any thread: runs `on_wake` (and queued `perform`s) on
    /// the main thread. Wake-ups coalesce.
    pub fn waker(&self) -> Arc<dyn Fn() + Send + Sync> {
        let s = self.signal.clone();
        Arc::new(move || s.fire())
    }

    pub fn on_wake(&self, f: Box<dyn FnMut()>) {
        STATE.with_borrow_mut(|s| s.on_wake = Some(f));
    }

    /// Fire `on_timer` once at `at`; `None` disarms. Re-arming replaces.
    pub fn set_timer(&self, at: Option<Instant>) {
        let when = match at {
            Some(t) => {
                CFAbsoluteTimeGetCurrent()
                    + t.saturating_duration_since(Instant::now()).as_secs_f64()
            }
            None => FAR_FUTURE,
        };
        self.timer.set_next_fire_date(when);
    }

    pub fn on_timer(&self, f: Box<dyn FnMut()>) {
        STATE.with_borrow_mut(|s| s.on_timer = Some(f));
    }

    /// Run `f` once on the next pass of the loop.
    pub fn perform(&self, f: Box<dyn FnOnce()>) {
        STATE.with_borrow_mut(|s| s.queue.push(f));
        self.signal.fire();
    }

    /// Make `run` return. `NSApp.stop` only takes effect after the next event,
    /// so post an empty one.
    pub fn stop(&self) {
        self.app.stop(None);
        let ev = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
            NSEventType::ApplicationDefined,
            CGPoint::new(0.0, 0.0),
            NSEventModifierFlags::empty(),
            0.0,
            0,
            None,
            0,
            0,
            0,
        );
        if let Some(ev) = ev {
            self.app.postEvent_atStart(&ev, true);
        }
    }
}
