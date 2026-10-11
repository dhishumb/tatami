//! The global key tap: a session `CGEventTap` on the main run loop that turns
//! key events into `RawKeyEvent`s and lets the sink swallow them.

use std::ffi::c_void;
use std::ptr::{self, NonNull};

use objc2_core_foundation::{CFMachPort, CFRetained, CFRunLoopSource};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType,
};
use tatami_core::{KeyDisposition, KeySink, Mods, PlatformError, RawKeyEvent};

use crate::ax;
use crate::runloop::{common_modes, main_run_loop};

/// Device-independent modifier bits; Fn, CapsLock and the numeric-pad bit are ignored.
pub fn mods_from_flags(flags: u64) -> Mods {
    let has = |m: CGEventFlags| flags & m.bits() != 0;
    Mods {
        alt: has(CGEventFlags::MaskAlternate),
        shift: has(CGEventFlags::MaskShift),
        ctrl: has(CGEventFlags::MaskControl),
        cmd: has(CGEventFlags::MaskCommand),
    }
}

/// What the callback reaches through its `user_info` pointer.
struct TapState {
    sink: KeySink,
    port: Option<CFRetained<CFMachPort>>,
}

/// Owns the tap; dropping it removes it from the run loop.
pub struct KeyTap {
    port: CFRetained<CFMachPort>,
    src: CFRetained<CFRunLoopSource>,
    state: NonNull<TapState>,
}

impl KeyTap {
    pub fn install(sink: KeySink) -> Result<KeyTap, PlatformError> {
        if !ax::is_trusted() {
            return Err(PlatformError::NotTrusted);
        }
        let state = NonNull::from(Box::leak(Box::new(TapState { sink, port: None })));
        let mask = [
            CGEventType::KeyDown,
            CGEventType::KeyUp,
            CGEventType::FlagsChanged,
        ]
        .iter()
        .fold(0u64, |m, t| m | 1 << t.0);
        // SAFETY: `tap_callback` matches the callback ABI; `state` stays valid
        // until `Drop`, which removes the tap first.
        let port = unsafe {
            CGEvent::tap_create(
                CGEventTapLocation::SessionEventTap,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                mask,
                Some(tap_callback),
                state.as_ptr().cast(),
            )
        };
        let src = port
            .as_ref()
            .and_then(|p| CFMachPort::new_run_loop_source(None, Some(p), 0));
        let (Some(port), Some(src)) = (port, src) else {
            // SAFETY: the tap never reached a run loop, so the callback never saw `state`.
            drop(unsafe { Box::from_raw(state.as_ptr()) });
            return Err(PlatformError::Other("CGEventTapCreate failed".into()));
        };
        // SAFETY: the callback cannot run yet (not on a run loop), so this is the only access.
        unsafe { (*state.as_ptr()).port = Some(port.clone()) };
        main_run_loop().add_source(Some(&src), common_modes());
        CGEvent::tap_enable(&port, true);
        Ok(KeyTap { port, src, state })
    }
}

impl Drop for KeyTap {
    fn drop(&mut self) {
        CGEvent::tap_enable(&self.port, false);
        main_run_loop().remove_source(Some(&self.src), common_modes());
        self.port.invalidate();
        // SAFETY: the tap is off the run loop and invalidated; no callback can see `state` now.
        drop(unsafe { Box::from_raw(self.state.as_ptr()) });
    }
}

unsafe extern "C-unwind" fn tap_callback(
    _: CGEventTapProxy,
    ty: CGEventType,
    event: NonNull<CGEvent>,
    info: *mut c_void,
) -> *mut CGEvent {
    // SAFETY: `info` is the `TapState` from `install`, freed only after the tap is removed;
    // the callback runs on the main thread and does not re-enter.
    let st = unsafe { &mut *info.cast::<TapState>() };
    // SAFETY: CG passes a live event for the duration of the callback.
    let ev = Some(unsafe { event.as_ref() });
    let mods = || mods_from_flags(CGEvent::flags(ev).bits());
    let keycode = || CGEvent::integer_value_field(ev, CGEventField::KeyboardEventKeycode) as u16;
    let key = match ty {
        CGEventType::KeyDown => RawKeyEvent::Down {
            keycode: keycode(),
            mods: mods(),
            repeat: CGEvent::integer_value_field(ev, CGEventField::KeyboardEventAutorepeat) != 0,
        },
        CGEventType::KeyUp => RawKeyEvent::Up {
            keycode: keycode(),
            mods: mods(),
        },
        CGEventType::FlagsChanged => RawKeyEvent::Flags { mods: mods() },
        CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
            tracing::warn!(?ty, "event tap disabled by macOS; re-enabling");
            if let Some(p) = &st.port {
                CGEvent::tap_enable(p, true);
            }
            return event.as_ptr();
        }
        _ => return event.as_ptr(),
    };
    match (st.sink)(key) {
        KeyDisposition::Swallow => ptr::null_mut(),
        KeyDisposition::Pass => event.as_ptr(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mods_ignore_fn_capslock_numpad() {
        let f = |m: CGEventFlags| m.bits();
        let all = f(CGEventFlags::MaskAlternate)
            | f(CGEventFlags::MaskShift)
            | f(CGEventFlags::MaskControl)
            | f(CGEventFlags::MaskCommand);
        let noise = f(CGEventFlags::MaskSecondaryFn)
            | f(CGEventFlags::MaskAlphaShift)
            | f(CGEventFlags::MaskNumericPad)
            | 0x100; // device-dependent left-control bit
        assert_eq!(mods_from_flags(noise), Mods::default());
        assert_eq!(
            mods_from_flags(all | noise),
            Mods {
                alt: true,
                shift: true,
                ctrl: true,
                cmd: true
            }
        );
        let alt_shift =
            mods_from_flags(f(CGEventFlags::MaskAlternate) | f(CGEventFlags::MaskShift));
        assert!(alt_shift.alt && alt_shift.shift && !alt_shift.ctrl && !alt_shift.cmd);
    }
}
