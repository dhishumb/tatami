//! Event sources: a per-app `AXObserver`, NSWorkspace / screen notifications
//! and trust polling. All deliver into `Shared::emit` on the main thread.

use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::rc::{Rc, Weak};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2_app_kit::{
    NSApplicationDidChangeScreenParametersNotification, NSRunningApplication, NSWorkspace,
    NSWorkspaceApplicationKey, NSWorkspaceDidActivateApplicationNotification,
    NSWorkspaceDidLaunchApplicationNotification, NSWorkspaceDidTerminateApplicationNotification,
};
use objc2_application_services::{AXError, AXObserver, AXUIElement};
use objc2_core_foundation::{
    CFAbsoluteTimeGetCurrent, CFRetained, CFRunLoopSource, CFRunLoopTimer, CFString,
};
use objc2_foundation::{
    NSNotification, NSNotificationCenter, NSNotificationName, NSOperationQueue,
};
use tatami_core::{PlatformError, PlatformEvent as Ev};
use tatami_protocol::WindowHandle;

use crate::ax::{self, AxElement, check, to_platform_error};
use crate::runloop::{common_modes, main_run_loop};
use crate::{AppEntry, Shared};

/// Registered on each app element; the index is the `kind` in `on_ax`.
/// Registration fails as a whole only if `AXWindowCreated` (first) fails.
const NOTES: [&str; 9] = [
    "AXWindowCreated",
    "AXUIElementDestroyed",
    "AXFocusedWindowChanged",
    "AXWindowMoved",
    "AXWindowResized",
    "AXWindowMiniaturized",
    "AXWindowDeminiaturized",
    "AXTitleChanged",
    // Per-app activation straight from AX: NSWorkspace's activation
    // notification never fires for an app it lost track of (plyterm).
    "AXApplicationActivated",
];

const TRUST_POLL_SECS: f64 = 2.0;

thread_local! {
    /// The platform the AX callback delivers to (its refcon is only the pid).
    static CURRENT: RefCell<Weak<Shared>> = RefCell::default();
    static NOTE_NAMES: Vec<CFRetained<CFString>> =
        NOTES.iter().map(|n| CFString::from_static_str(n)).collect();
}

pub(crate) fn set_current(sh: &Rc<Shared>) {
    CURRENT.set(Rc::downgrade(sh));
}

/// Keeps one app's observer registered; dropping it detaches the source.
pub(crate) struct AppObserver {
    _obs: CFRetained<AXObserver>,
    src: CFRetained<CFRunLoopSource>,
}

impl Drop for AppObserver {
    fn drop(&mut self) {
        main_run_loop().remove_source(Some(&self.src), common_modes());
        self.src.invalidate();
    }
}

pub(crate) fn observe_app(sh: &Shared, pid: i32) -> Result<(), PlatformError> {
    if pid <= 0 {
        return Err(PlatformError::NoApp(pid));
    }
    let app = sh.app_el(pid);
    if sh
        .apps
        .borrow()
        .get(&pid)
        .is_some_and(|a| a.observer.is_some())
    {
        return Ok(());
    }
    let mut raw: *mut AXObserver = ptr::null_mut();
    // SAFETY: `ax_callback` matches the callback ABI; `raw` is a valid out-pointer.
    check(unsafe { AXObserver::create(pid, Some(ax_callback), NonNull::from(&mut raw)) })
        .map_err(to_platform_error)?;
    let raw = NonNull::new(raw).ok_or(PlatformError::Ax(AXError::Failure.0))?;
    // SAFETY: Create rule, we own this reference.
    let obs: CFRetained<AXObserver> = unsafe { CFRetained::from_raw(raw) };

    let refcon = pid as usize as *mut c_void;
    NOTE_NAMES.with(|names| {
        for (i, name) in names.iter().enumerate() {
            // SAFETY: live observer, element and name; refcon is an opaque integer.
            let e = unsafe { obs.add_notification(app.raw(), name, refcon) };
            if e == AXError::Success || e == AXError::NotificationAlreadyRegistered {
                continue;
            }
            if i == 0 {
                return Err(to_platform_error(e));
            }
            tracing::debug!(
                pid,
                note = NOTES[i],
                err = e.0,
                "AX notification not registered"
            );
        }
        Ok(())
    })?;

    // SAFETY: the observer is live; the source is returned +0 and retained by the binding.
    let src = unsafe { obs.run_loop_source() };
    main_run_loop().add_source(Some(&src), common_modes());
    let mut apps = sh.apps.borrow_mut();
    let entry = apps.entry(pid).or_insert_with(|| AppEntry {
        el: app,
        observer: None,
    });
    entry.observer = Some(AppObserver { _obs: obs, src });
    Ok(())
}

unsafe extern "C-unwind" fn ax_callback(
    _: NonNull<AXObserver>,
    el: NonNull<AXUIElement>,
    note: NonNull<CFString>,
    refcon: *mut c_void,
) {
    let Some(sh) = CURRENT.with_borrow(Weak::upgrade) else {
        return;
    };
    // SAFETY: AX passes objects that stay alive for the duration of the callback.
    let (el, note) = unsafe { (el.as_ref(), note.as_ref()) };
    let Some(kind) = NOTE_NAMES.with(|n| n.iter().position(|n| **n == *note)) else {
        return;
    };
    sh.on_ax(refcon as usize as i32, kind, el);
}

impl Shared {
    fn on_ax(&self, pid: i32, kind: usize, el: &AXUIElement) {
        let ev = match kind {
            0 => self
                .window_info(pid, &AxElement::from_ref(el))
                .map(Ev::WindowCreated),
            1 => self.forget(pid, el).map(Ev::WindowDestroyed),
            2 => self.handle(pid, el).map(|(h, _)| Ev::WindowFocused(h)),
            3 | 4 => self.handle(pid, el).and_then(|(h, el)| {
                let r = self.read_frame(&el).ok()?;
                Some(if kind == 3 {
                    Ev::WindowMoved(h, r)
                } else {
                    Ev::WindowResized(h, r)
                })
            }),
            5 | 6 => self
                .handle(pid, el)
                .map(|(h, _)| Ev::WindowMinimized(h, kind == 5)),
            // Fires for any titled element; keep only tracked windows.
            7 => self
                .tracked(el)
                .and_then(|(h, el)| Some(Ev::TitleChanged(h, el.attr_string("AXTitle").ok()?))),
            _ => Some(Ev::AppActivated(pid)),
        };
        if let Some(ev) = ev {
            self.emit(ev);
        }
    }

    /// The handle of window `el`, tracking it if new.
    fn handle(&self, pid: i32, el: &AXUIElement) -> Option<(WindowHandle, AxElement)> {
        let el = AxElement::from_ref(el);
        let h = WindowHandle(el.window_id().ok()? as u64);
        if !self.windows.borrow().contains_key(&h) {
            self.track(pid, &el)?;
        }
        Some((h, el))
    }

    /// A tracked window whose stored element is `el` itself (not a child).
    fn tracked(&self, el: &AXUIElement) -> Option<(WindowHandle, AxElement)> {
        let id = AxElement::from_ref(el).window_id().ok()?;
        let h = WindowHandle(id as u64);
        let ws = self.windows.borrow();
        let e = ws.get(&h).filter(|e| e.el.raw() == el)?;
        Some((h, e.el.clone()))
    }

    /// Drop the window whose element is `el`. The element may be dead, so
    /// match by `CFEqual` rather than asking it for its id.
    fn forget(&self, pid: i32, el: &AXUIElement) -> Option<WindowHandle> {
        let mut ws = self.windows.borrow_mut();
        let h = ws
            .iter()
            .find(|(_, e)| e.pid == pid && e.el.raw() == el)
            .map(|(h, _)| *h)?;
        ws.remove(&h);
        Some(h)
    }
}

type Token = Retained<ProtocolObject<dyn NSObjectProtocol>>;

/// Process-wide observers; removed on drop.
pub(crate) struct Global {
    tokens: Vec<(Retained<NSNotificationCenter>, Token)>,
    trust: Option<CFRetained<CFRunLoopTimer>>,
}

impl Global {
    pub(crate) fn start(sh: &Rc<Shared>) -> Self {
        let ws = NSWorkspace::sharedWorkspace().notificationCenter();
        let dc = NSNotificationCenter::defaultCenter();
        // SAFETY: reading immutable framework constants.
        let (launch, term, act, screens) = unsafe {
            (
                NSWorkspaceDidLaunchApplicationNotification,
                NSWorkspaceDidTerminateApplicationNotification,
                NSWorkspaceDidActivateApplicationNotification,
                NSApplicationDidChangeScreenParametersNotification,
            )
        };
        let tokens = vec![
            observe(&ws, launch, sh, |sh, n| {
                if let Some(a) = app_of(n).map(|a| crate::apps::info(&a))
                    && a.pid > 0
                {
                    sh.emit(Ev::AppLaunched(a));
                }
            }),
            observe(&ws, term, sh, |sh, n| {
                if let Some(a) = app_of(n) {
                    sh.emit(Ev::AppTerminated(a.processIdentifier()));
                }
            }),
            observe(&ws, act, sh, |sh, n| {
                if let Some(a) = app_of(n) {
                    sh.emit(Ev::AppActivated(a.processIdentifier()));
                }
            }),
            observe(&dc, screens, sh, |sh, _| sh.emit(Ev::ScreensChanged)),
        ];
        Self {
            tokens,
            trust: trust_poll(sh),
        }
    }
}

impl Drop for Global {
    fn drop(&mut self) {
        for (center, token) in &self.tokens {
            // SAFETY: `token` was returned by this center's addObserverForName.
            unsafe { center.removeObserver(token.as_ref()) };
        }
        if let Some(t) = &self.trust {
            t.invalidate();
        }
    }
}

fn observe(
    center: &Retained<NSNotificationCenter>,
    name: &NSNotificationName,
    sh: &Rc<Shared>,
    f: fn(&Shared, &NSNotification),
) -> (Retained<NSNotificationCenter>, Token) {
    let weak = Rc::downgrade(sh);
    let block = RcBlock::new(move |n: NonNull<NSNotification>| {
        if let Some(sh) = weak.upgrade() {
            // SAFETY: the notification is live for the duration of the block.
            f(&sh, unsafe { n.as_ref() });
        }
    });
    // SAFETY: queued on the main queue, so the non-Send captures stay on the main thread.
    let token = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(name),
            None,
            Some(&NSOperationQueue::mainQueue()),
            &block,
        )
    };
    (center.clone(), token)
}

fn app_of(n: &NSNotification) -> Option<Retained<NSRunningApplication>> {
    // SAFETY: reading an immutable framework constant.
    let key = unsafe { NSWorkspaceApplicationKey };
    n.userInfo()?.objectForKey(key)?.downcast().ok()
}

/// Poll `AXIsProcessTrusted` every 2 s while untrusted; emit once and stop.
fn trust_poll(sh: &Rc<Shared>) -> Option<CFRetained<CFRunLoopTimer>> {
    if ax::is_trusted() {
        return None;
    }
    let weak = Rc::downgrade(sh);
    let block = RcBlock::new(move |t: *mut CFRunLoopTimer| {
        if !ax::is_trusted() {
            return;
        }
        // SAFETY: CF passes the timer that is firing, alive for the call.
        if let Some(t) = unsafe { t.as_ref() } {
            t.invalidate();
        }
        if let Some(sh) = weak.upgrade() {
            sh.emit(Ev::TrustChanged(true));
        }
    });
    let first = CFAbsoluteTimeGetCurrent() + TRUST_POLL_SECS;
    // SAFETY: a valid block; CF copies it.
    let timer =
        unsafe { CFRunLoopTimer::with_handler(None, first, TRUST_POLL_SECS, 0, 0, Some(&block)) }?;
    main_run_loop().add_timer(Some(&timer), common_modes());
    Some(timer)
}
